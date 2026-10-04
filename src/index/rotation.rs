//! Seeded Haar-random rotation for the TurboQuant_prod MSE stage.
//!
//! TurboQuant quantizes `x′ = Π·x` rather than `x`. When the orthogonal
//! matrix Π is uniformly (Haar) distributed, `Π·x` for a unit `x` is uniform
//! on the sphere: every coordinate has the density
//! `f(t) ∝ (1 − t²)^((d−3)/2)`, ≈ N(0, 1/d) at d = 512, and distinct
//! coordinates are nearly independent. That holds whatever the anisotropy of
//! the raw embedding, and it is what lets one scalar codebook (#164) serve
//! every coordinate.
//!
//! # Generation
//!
//! Under the materialization contract (see [`super::codec_config`]) only
//! builders run [`Rotation::generate`]; the query side loads the stored matrix
//! with [`Rotation::from_bytes`] and never runs QR.
//!
//! 1. G is a d×d matrix of N(0, 1) draws from
//!    [`fill_standard_normal`]`(seed, ROTATION_STREAM_ID, ..)`, filled column
//!    by column: `G[i][j]` is draw `j·d + i`.
//! 2. Householder QR, `G = Q·R`, in f64.
//! 3. Column j of Q is multiplied by `sign(R_jj)` (and row j of R with it).
//!    QR with a positive R diagonal is unique, so the result doesn't depend
//!    on the QR algorithm's sign convention, and for Gaussian G it is exactly
//!    Haar-distributed (Mezzadri 2007, "How to generate random matrices from
//!    the classical compact groups"). Without this step Householder's
//!    convention biases Q: the first coordinate of `Q·e₁` is always negative.
//! 4. Π = Q, rounded to f32 and stored row-major.
//!
//! # Reproducibility
//!
//! `(dim, seed, generator_version)` determines the bytes of the matrix on
//! every target with IEEE 754 binary64 arithmetic, under the same guarantee
//! as the sampler. Every step after sampling uses only `+ − × ÷ sqrt`, each
//! correctly rounded, in a fixed order on one thread, and Rust never fuses or
//! reorders float operations.
//!
//! `generator_version` is the
//! [`GAUSSIAN_GENERATOR_VERSION`](super::gaussian::GAUSSIAN_GENERATOR_VERSION)
//! the matrix was generated under. That version covers this QR as well as
//! the sampler: any change to `generate`'s output, even with the sampler
//! unchanged, must bump it, so that a version always names one byte sequence.
//! The golden test below fails on such a change.
//!
//! [`Rotation::rotate`] and [`Rotation::inverse_rotate`] are deterministic
//! too: both accumulate in index order in f32. A builder that encodes
//! documents through them gets the same bytes on every target.
//!
//! # Precision
//!
//! Rounding an orthogonal matrix to f32 moves each entry by at most
//! `u = 2⁻²⁴` relative, so by Cauchy–Schwarz every entry of `ΠᵀΠ − I` and
//! `ΠΠᵀ − I` is within `2u + u²` of the f64 matrix's own orthogonality
//! error. The tests pin that bound; the measured values are recorded there.
//!
//! # File format
//!
//! All integers and values are little-endian.
//!
//! | offset | size     | field                                  |
//! |--------|----------|----------------------------------------|
//! | 0      | 4        | magic `TQRT`                           |
//! | 4      | 4        | `dim` (u32)                            |
//! | 8      | 8        | `seed` (u64)                           |
//! | 16     | 4        | `generator_version` (u32)              |
//! | 20     | 4·dim²   | Π as row-major f32, `Π[i][j]` at `i·dim + j` |
//!
//! A v4 static release stores it as [`ROTATION_FILE`].

use super::assets::{parse_values, write_values, AssetError};
use super::gaussian::{fill_standard_normal, GAUSSIAN_GENERATOR_VERSION};

/// The rotation's file name in a v4 static release.
pub const ROTATION_FILE: &str = "rotation.bin";

const ROTATION_MAGIC: [u8; 4] = *b"TQRT";
const ROTATION_HEADER_SIZE: usize = 20;

/// The sampler keystream G is drawn from. A tag rather than a small integer,
/// so other generators (the [QJL matrix](super::QjlMatrix)) can take their
/// own streams and stay independent of the rotation even when their seeds
/// are equal.
pub(super) const ROTATION_STREAM_ID: u64 = u64::from_le_bytes(*b"rotation");

/// A Haar-random d×d orthogonal matrix Π.
///
/// A distinct type from [`ProjectionMatrix`](super::ProjectionMatrix), so a
/// QJL projection can't be passed where a rotation is expected.
#[derive(Debug, Clone, PartialEq)]
pub struct Rotation {
    dim: u32,
    seed: u64,
    generator_version: u32,
    /// Π, row-major: `Π[i][j]` is `matrix[i·dim + j]`.
    matrix: Vec<f32>,
}

impl Rotation {
    /// Generates the `(dim, seed)` rotation; see the module docs. Offline
    /// only: at d = 512 it does about 3.6·10⁸ flops on one thread.
    ///
    /// # Panics
    ///
    /// If `dim` is zero.
    pub fn generate(dim: u32, seed: u64) -> Self {
        assert!(dim > 0, "dimensions must be positive");
        let d = dim as usize;

        let mut r = vec![0.0; d * d];
        fill_standard_normal(seed, ROTATION_STREAM_ID, &mut r);
        let mut q = householder_qr(d, &mut r);
        make_r_diagonal_positive(d, &mut q, &mut r);

        // Q is column-major; store Π = Q row-major.
        let mut matrix = vec![0.0; d * d];
        for (j, column) in q.chunks_exact(d).enumerate() {
            for (i, &value) in column.iter().enumerate() {
                matrix[i * d + j] = value as f32;
            }
        }

        Self {
            dim,
            seed,
            generator_version: GAUSSIAN_GENERATOR_VERSION,
            matrix,
        }
    }

    /// Writes `Π·x` into `out`. Allocation-free.
    pub fn rotate(&self, x: &[f32], out: &mut [f32]) -> Result<(), AssetError> {
        self.check_len(x.len())?;
        self.check_len(out.len())?;

        for (slot, row) in out.iter_mut().zip(self.matrix.chunks_exact(x.len())) {
            *slot = row.iter().zip(x).map(|(lhs, rhs)| lhs * rhs).sum();
        }
        Ok(())
    }

    /// Writes `Πᵀ·y`, the inverse rotation, into `out`. Allocation-free.
    pub fn inverse_rotate(&self, y: &[f32], out: &mut [f32]) -> Result<(), AssetError> {
        self.check_len(y.len())?;
        self.check_len(out.len())?;

        // Πᵀ·y = Σ_i y_i·(row i of Π). Adding whole rows reads Π in storage
        // order instead of striding down its columns.
        out.fill(0.0);
        for (&weight, row) in y.iter().zip(self.matrix.chunks_exact(y.len())) {
            for (slot, &value) in out.iter_mut().zip(row) {
                *slot += weight * value;
            }
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ROTATION_HEADER_SIZE + self.matrix.len() * 4);
        out.extend_from_slice(&ROTATION_MAGIC);
        out.extend_from_slice(&self.dim.to_le_bytes());
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.extend_from_slice(&self.generator_version.to_le_bytes());
        write_values(&mut out, &self.matrix);
        out
    }

    /// Parses the file format in the module docs. It checks the layout only,
    /// not orthogonality or the generator version: a loader compares
    /// [`seed`](Self::seed) and
    /// [`generator_version`](Self::generator_version) against the release's
    /// codec config.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AssetError> {
        let Some((header, data)) = bytes.split_first_chunk::<ROTATION_HEADER_SIZE>() else {
            return Err(AssetError::InvalidSize {
                minimum: ROTATION_HEADER_SIZE,
                actual: bytes.len(),
            });
        };

        let magic: [u8; 4] = header[0..4].try_into().unwrap();
        if magic != ROTATION_MAGIC {
            return Err(AssetError::InvalidMagic {
                expected: ROTATION_MAGIC,
                actual: magic,
            });
        }
        let dim = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let seed = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let generator_version = u32::from_le_bytes(header[16..20].try_into().unwrap());
        let matrix = parse_values(data, dim, dim)?;

        Ok(Self {
            dim,
            seed,
            generator_version,
            matrix,
        })
    }

    pub fn dim(&self) -> u32 {
        self.dim
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn generator_version(&self) -> u32 {
        self.generator_version
    }

    /// Π, row-major.
    pub fn values(&self) -> &[f32] {
        &self.matrix
    }

    fn check_len(&self, len: usize) -> Result<(), AssetError> {
        if len != self.dim as usize {
            return Err(AssetError::DimensionMismatch {
                expected: self.dim as usize,
                actual: len,
            });
        }
        Ok(())
    }
}

/// Householder QR of the d×d matrix `a`, stored column-major (column j is
/// `a[j·d..(j+1)·d]`). Overwrites `a` with R and returns Q, also
/// column-major.
///
/// Each reflector takes `R_kk = −sign(a_kk)·‖a[k.., k]‖`, the conventional
/// choice that avoids cancellation, so R's diagonal has arbitrary signs until
/// [`make_r_diagonal_positive`].
fn householder_qr(d: usize, a: &mut [f64]) -> Vec<f64> {
    let mut reflectors = Vec::with_capacity(d);
    for k in 0..d {
        let (done, later) = a.split_at_mut((k + 1) * d);
        let reflector = Reflector::zeroing_below_first(&mut done[k * d + k..]);
        for column in later.chunks_exact_mut(d) {
            reflector.apply(&mut column[k..]);
        }
        reflectors.push(reflector);
    }

    // Q = H_0·H_1⋯H_{d−1}, accumulated from the right: before H_k is
    // applied, Q is the identity outside rows and columns k+1.., so H_k only
    // changes Q[k.., k..].
    let mut q = vec![0.0; d * d];
    for i in 0..d {
        q[i * d + i] = 1.0;
    }
    for (k, reflector) in reflectors.iter().enumerate().rev() {
        for column in q.chunks_exact_mut(d).skip(k) {
            reflector.apply(&mut column[k..]);
        }
    }
    q
}

/// Flips the sign of column j of Q and row j of R wherever `R_jj < 0`.
/// `Q·R` is unchanged, and the result is the unique QR with a positive R
/// diagonal. A zero `R_jj` (only possible for a singular input) is left as
/// is. Both matrices are column-major.
fn make_r_diagonal_positive(d: usize, q: &mut [f64], r: &mut [f64]) {
    for j in 0..d {
        if r[j * d + j] < 0.0 {
            q[j * d..(j + 1) * d]
                .iter_mut()
                .for_each(|value| *value = -*value);
            r[j..]
                .iter_mut()
                .step_by(d)
                .for_each(|value| *value = -*value);
        }
    }
}

/// The Householder reflector `H = I − β·v·vᵀ`. `β = 0` is the identity.
struct Reflector {
    v: Vec<f64>,
    beta: f64,
}

impl Reflector {
    /// Builds the H with `H·x = α·e₁`, `α = −sign(x₀)·‖x‖`, and overwrites
    /// `x` with `α·e₁`. A zero `x` gets the identity.
    fn zeroing_below_first(x: &mut [f64]) -> Self {
        let norm = x.iter().map(|value| value * value).sum::<f64>().sqrt();
        if norm == 0.0 {
            return Self {
                v: vec![0.0; x.len()],
                beta: 0.0,
            };
        }

        let alpha = if x[0] < 0.0 { norm } else { -norm };
        let mut v = x.to_vec();
        v[0] -= alpha;
        // |v₀| = |x₀| + ‖x‖ > 0, so vᵀv > 0.
        let beta = 2.0 / v.iter().map(|value| value * value).sum::<f64>();

        x[0] = alpha;
        x[1..].fill(0.0);
        Self { v, beta }
    }

    /// `y ← H·y`.
    fn apply(&self, y: &mut [f64]) {
        let scale = self.beta * self.v.iter().zip(&*y).map(|(v, y)| v * y).sum::<f64>();
        for (y, v) in y.iter_mut().zip(&self.v) {
            *y -= scale * v;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::index::release_manifest::sha256_hex;
    use crate::index::CentroidTable;

    /// sha256 of `Rotation::generate(512, 163).to_bytes()`, captured on x86_64
    /// when `GAUSSIAN_GENERATOR_VERSION` was 1. CI runs this on aarch64.
    /// numpy's LAPACK QR of the same G, sign-canonicalized and rounded to
    /// f32, matched this matrix in every entry.
    const GOLDEN_512_SEED_163_SHA256: &str =
        "31835fa7a5d584316449d4c401985f7df7f11f45948ec714ef4f5a0c10a77a59";

    /// u, the unit roundoff of f32.
    const U: f64 = f32::EPSILON as f64 / 2.0;
    /// Householder QR loses orthogonality by a small multiple of d·ε₆₄.
    const F64_ORTHOGONALITY_TOLERANCE_512: f64 = 512.0 * f64::EPSILON;
    /// Per-coordinate z-score limit in the uniformity checks: five standard
    /// errors, as in the sampler's moment test.
    const Z_TOLERANCE: f64 = 5.0;

    fn rotation_512() -> &'static Rotation {
        static ROTATION: OnceLock<Rotation> = OnceLock::new();
        ROTATION.get_or_init(|| Rotation::generate(512, 163))
    }

    /// The G that `Rotation::generate(d, seed)` factors, column-major.
    fn gaussian_matrix(d: usize, seed: u64) -> Vec<f64> {
        let mut g = vec![0.0; d * d];
        fill_standard_normal(seed, ROTATION_STREAM_ID, &mut g);
        g
    }

    fn transpose(d: usize, m: &[f64]) -> Vec<f64> {
        let mut out = vec![0.0; d * d];
        for (i, row) in m.chunks_exact(d).enumerate() {
            for (j, &value) in row.iter().enumerate() {
                out[j * d + i] = value;
            }
        }
        out
    }

    fn to_f64(values: &[f32]) -> Vec<f64> {
        values.iter().map(|&value| f64::from(value)).collect()
    }

    fn dot(lhs: &[f64], rhs: &[f64]) -> f64 {
        lhs.iter().zip(rhs).map(|(a, b)| a * b).sum()
    }

    fn norm(values: &[f32]) -> f64 {
        let values = to_f64(values);
        dot(&values, &values).sqrt()
    }

    /// `max |(M·Mᵀ − I)_ik|`, where the rows of M are the consecutive
    /// length-d runs of `rows`.
    fn max_gram_error(d: usize, rows: &[f64]) -> f64 {
        let rows: Vec<&[f64]> = rows.chunks_exact(d).collect();
        let mut worst = 0.0f64;
        for (i, row) in rows.iter().enumerate() {
            for (k, other) in rows.iter().enumerate().skip(i) {
                let identity = if i == k { 1.0 } else { 0.0 };
                worst = worst.max((dot(row, other) - identity).abs());
            }
        }
        worst
    }

    /// `Q·R` for column-major Q and R.
    fn product(d: usize, q: &[f64], r: &[f64]) -> Vec<f64> {
        let mut out = vec![0.0; d * d];
        for (out_column, r_column) in out.chunks_exact_mut(d).zip(r.chunks_exact(d)) {
            for (q_column, &weight) in q.chunks_exact(d).zip(r_column) {
                for (slot, value) in out_column.iter_mut().zip(q_column) {
                    *slot += weight * value;
                }
            }
        }
        out
    }

    fn max_abs_difference(lhs: &[f64], rhs: &[f64]) -> f64 {
        lhs.iter()
            .zip(rhs)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn generate_is_pinned() {
        // Re-capture the golden whenever the version is bumped.
        assert_eq!(GAUSSIAN_GENERATOR_VERSION, 1);
        let rotation = rotation_512();
        assert_eq!(rotation.generator_version(), GAUSSIAN_GENERATOR_VERSION);
        assert_eq!(sha256_hex(&rotation.to_bytes()), GOLDEN_512_SEED_163_SHA256);
    }

    #[test]
    fn generate_is_deterministic_and_seed_dependent() {
        let rotation = Rotation::generate(64, 7);
        assert_eq!(Rotation::generate(64, 7).to_bytes(), rotation.to_bytes());
        for seed in [8, 1 << 40] {
            assert_ne!(Rotation::generate(64, seed).values(), rotation.values());
        }
    }

    #[test]
    #[should_panic(expected = "dimensions must be positive")]
    fn generate_rejects_zero_dim() {
        Rotation::generate(0, 1);
    }

    #[test]
    fn rotate_applies_pi_and_inverse_rotate_applies_its_transpose() {
        let d = 16;
        let rotation = Rotation::generate(d as u32, 3);
        let pi = rotation.values();
        let mut basis = vec![0.0; d];
        let mut out = vec![0.0; d];
        for j in 0..d {
            basis.fill(0.0);
            basis[j] = 1.0;
            // Π·e_j is column j and Πᵀ·e_j is row j, exactly: every other
            // product is zero.
            rotation.rotate(&basis, &mut out).unwrap();
            assert!((0..d).all(|i| out[i] == pi[i * d + j]), "Π·e_{j}");
            rotation.inverse_rotate(&basis, &mut out).unwrap();
            assert_eq!(out, pi[j * d..(j + 1) * d], "Πᵀ·e_{j}");
        }
    }

    #[test]
    fn canonical_signs_give_the_unique_qr_with_positive_r_diagonal() {
        let d = 64;
        let g = gaussian_matrix(d, 163);
        let mut r = g.clone();
        let raw_q = householder_qr(d, &mut r);
        // Householder's R_kk = −sign(a_kk)·‖a[k.., k]‖ has a random sign;
        // observed 32 of 64 negative.
        assert!(
            (0..d).any(|j| r[j * d + j] < 0.0),
            "raw R diagonal is already positive"
        );
        assert!(max_abs_difference(&product(d, &raw_q, &r), &g) < 1e-12);

        let mut q = raw_q.clone();
        make_r_diagonal_positive(d, &mut q, &mut r);
        assert!((0..d).all(|j| r[j * d + j] > 0.0));
        // Still a QR of G, but not the unconditioned one.
        assert!(max_abs_difference(&product(d, &q, &r), &g) < 1e-12);
        assert_ne!(q, raw_q);

        // QR with a positive R diagonal is unique, so the canonical Q equals
        // the one modified Gram–Schmidt produces, which has that property by
        // construction. Observed difference 3.2e-15.
        let mut gram_schmidt = g.clone();
        for j in 0..d {
            let (done, rest) = gram_schmidt.split_at_mut(j * d);
            let column = &mut rest[..d];
            for basis in done.chunks_exact(d) {
                let projection = dot(basis, column);
                for (value, b) in column.iter_mut().zip(basis) {
                    *value -= projection * b;
                }
            }
            let length = dot(column, column).sqrt();
            column.iter_mut().for_each(|value| *value /= length);
        }
        assert!(max_abs_difference(&q, &gram_schmidt) < 1e-12);
    }

    #[test]
    fn f64_qr_is_orthogonal_at_production_dim() {
        let d = 512;
        let mut r = gaussian_matrix(d, 163);
        let q = householder_qr(d, &mut r);
        // Observed 4.4e-15 and 4.0e-15. Q is column-major, so its storage
        // rows are its columns.
        let q_t_q = max_gram_error(d, &q);
        let q_q_t = max_gram_error(d, &transpose(d, &q));
        assert!(
            q_t_q <= F64_ORTHOGONALITY_TOLERANCE_512,
            "QᵀQ − I: {q_t_q:e}"
        );
        assert!(
            q_q_t <= F64_ORTHOGONALITY_TOLERANCE_512,
            "QQᵀ − I: {q_q_t:e}"
        );
    }

    #[test]
    fn rounded_rotation_is_orthogonal_within_the_f32_rounding_bound() {
        let d = 512;
        // The bound from the module docs, 2u + u² ≈ 1.19e-7, on top of the
        // f64 Q's own error. Observed 1.16e-8 (ΠᵀΠ) and 1.20e-8 (ΠΠᵀ), and
        // numpy reproduces both.
        let tolerance = 2.0 * U + U * U + F64_ORTHOGONALITY_TOLERANCE_512;
        let pi = to_f64(rotation_512().values());
        let pi_t_pi = max_gram_error(d, &transpose(d, &pi));
        let pi_pi_t = max_gram_error(d, &pi);
        assert!(pi_t_pi <= tolerance, "ΠᵀΠ − I: {pi_t_pi:e}");
        assert!(pi_pi_t <= tolerance, "ΠΠᵀ − I: {pi_pi_t:e}");
    }

    /// 1000 standard-normal vectors at d = 512. Rounding in the f32
    /// accumulation dominates both errors: with the sums done in f64, the
    /// f32 cast alone moves ‖Πx‖ by at most 3.7e-9 on these vectors.
    #[test]
    fn rotate_preserves_norms_and_inverse_rotate_undoes_it() {
        const VECTORS: usize = 1000;
        let rotation = rotation_512();
        let d = rotation.dim() as usize;
        let mut draws = vec![0.0; VECTORS * d];
        fill_standard_normal(0x163, 0, &mut draws);

        let mut x = vec![0.0; d];
        let mut rotated = vec![0.0; d];
        let mut restored = vec![0.0; d];
        let mut error = vec![0.0; d];
        let mut worst_norm = 0.0f64;
        let mut worst_round_trip = 0.0f64;
        for draw in draws.chunks_exact(d) {
            x.iter_mut().zip(draw).for_each(|(x, &v)| *x = v as f32);
            rotation.rotate(&x, &mut rotated).unwrap();
            rotation.inverse_rotate(&rotated, &mut restored).unwrap();
            let x_norm = norm(&x);
            worst_norm = worst_norm.max((norm(&rotated) - x_norm).abs() / x_norm);
            for ((e, r), x) in error.iter_mut().zip(&restored).zip(&x) {
                *e = r - x;
            }
            worst_round_trip = worst_round_trip.max(norm(&error) / x_norm);
        }

        // Each f32 sum leaves an error of order u·‖x‖ in every output
        // coordinate. To first order the norm only sees the error's
        // component along Πx, which is O(u) whatever d. Observed 9.7e-8 ≈ 1.6u.
        assert!(worst_norm <= 8.0 * U, "norm error {worst_norm:e}");
        // The round trip sees the whole error vector, O(√d·u). Observed
        // 6.6e-7 ≈ 0.5·√d·u.
        let round_trip_tolerance = 2.0 * (d as f64).sqrt() * U;
        assert!(
            worst_round_trip <= round_trip_tolerance,
            "round-trip error {worst_round_trip:e}"
        );
    }

    /// x_j = 1/(j + 1): far from isotropic, largest on coordinate 0.
    fn anisotropic(d: usize) -> Vec<f64> {
        (0..d).map(|j| 1.0 / (j + 1) as f64).collect()
    }

    /// Worst per-coordinate z-scores (of the mean, of the variance) of
    /// `samples`, each a length-d vector, against the uniform distribution
    /// on the sphere of squared radius `norm_sq`: each coordinate has mean 0,
    /// variance σ² = norm_sq/d and fourth moment 3·norm_sq²/(d·(d+2)).
    fn worst_sphere_z_scores(d: usize, norm_sq: f64, samples: &[Vec<f64>]) -> (f64, f64) {
        let n = samples.len() as f64;
        let variance = norm_sq / d as f64;
        let fourth_moment = 3.0 * norm_sq * norm_sq / (d * (d + 2)) as f64;
        let mean_error = (variance / n).sqrt();
        let variance_error = ((fourth_moment - variance * variance) / n).sqrt();

        let mut worst_mean = 0.0f64;
        let mut worst_variance = 0.0f64;
        for i in 0..d {
            let mean = samples.iter().map(|s| s[i]).sum::<f64>() / n;
            let sample_variance =
                samples.iter().map(|s| (s[i] - mean).powi(2)).sum::<f64>() / (n - 1.0);
            worst_mean = worst_mean.max(mean.abs() / mean_error);
            worst_variance =
                worst_variance.max((sample_variance - variance).abs() / variance_error);
        }
        (worst_mean, worst_variance)
    }

    /// For Haar Π, Π·e₁ and Π·x are uniform on the spheres of radius 1 and
    /// ‖x‖. Returns the worst z-scores of their coordinates over seeds
    /// `0..seeds`, where `rotation(seed)` is Π, row-major.
    fn haar_z_scores(d: usize, seeds: u64, rotation: impl Fn(u64) -> Vec<f64>) -> [(f64, f64); 2] {
        let x = anisotropic(d);
        let mut first_columns = Vec::new();
        let mut rotated = Vec::new();
        for seed in 0..seeds {
            let pi = rotation(seed);
            first_columns.push(pi.chunks_exact(d).map(|row| row[0]).collect());
            rotated.push(pi.chunks_exact(d).map(|row| dot(row, &x)).collect());
        }
        [
            worst_sphere_z_scores(d, 1.0, &first_columns),
            worst_sphere_z_scores(d, dot(&x, &x), &rotated),
        ]
    }

    fn assert_haar_coordinates(d: usize, seeds: u64) {
        let generated = |seed| to_f64(Rotation::generate(d as u32, seed).values());
        let [e1, x] = haar_z_scores(d, seeds, generated);
        for (label, (mean_z, variance_z)) in [("Π·e₁", e1), ("Π·x", x)] {
            assert!(mean_z < Z_TOLERANCE, "{label}: mean z-score {mean_z}");
            assert!(
                variance_z < Z_TOLERANCE,
                "{label}: variance z-score {variance_z}"
            );
        }
    }

    /// Reduced size for CI. Observed worst z-scores: Π·e₁ 2.4 (mean) and 2.6
    /// (variance), Π·x 3.3 and 2.4.
    #[test]
    fn rotated_coordinates_are_uniform_on_the_sphere() {
        assert_haar_coordinates(32, 2000);
    }

    /// The check above has teeth: without sign canonicalization, the first
    /// coordinate of Q·e₁ is always −|g₀₀|/‖g₀‖. Observed mean z-scores 36
    /// (Q·e₁) and 28 (Q·x).
    #[test]
    fn unconditioned_householder_q_fails_the_uniformity_check() {
        let d = 32;
        let unconditioned = |seed| {
            let mut r = gaussian_matrix(d, seed);
            transpose(d, &householder_qr(d, &mut r))
        };
        let [(e1_mean_z, _), (x_mean_z, _)] = haar_z_scores(d, 2000, unconditioned);
        assert!(e1_mean_z > Z_TOLERANCE, "Q·e₁: mean z-score {e1_mean_z}");
        assert!(x_mean_z > Z_TOLERANCE, "Q·x: mean z-score {x_mean_z}");
    }

    /// Observed worst z-scores: Π·e₁ 3.2 (mean) and 3.6 (variance), Π·x 3.4
    /// and 3.2, in line with the largest of 512 standard normals.
    #[test]
    #[ignore = "20 s under --release: cargo test --release --lib rotation -- --ignored"]
    fn rotated_coordinates_are_uniform_on_the_sphere_at_production_dim() {
        assert_haar_coordinates(512, 200);
    }

    #[test]
    fn bytes_round_trip_in_the_documented_layout() {
        let rotation = Rotation::generate(4, 0x0102_0304_0506_0708);
        let bytes = rotation.to_bytes();
        assert_eq!(bytes.len(), 20 + 4 * 16);
        assert_eq!(&bytes[0..4], b"TQRT");
        assert_eq!(bytes[4..8], 4u32.to_le_bytes());
        assert_eq!(bytes[8..16], 0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(bytes[16..20], GAUSSIAN_GENERATOR_VERSION.to_le_bytes());
        // Row-major: Π[0][1] follows Π[0][0].
        assert_eq!(bytes[24..28], rotation.values()[1].to_le_bytes());

        let parsed = Rotation::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, rotation);
        assert_eq!(parsed.dim(), 4);
        assert_eq!(parsed.seed(), 0x0102_0304_0506_0708);
        assert_eq!(parsed.generator_version(), GAUSSIAN_GENERATOR_VERSION);
    }

    #[test]
    fn from_bytes_rejects_malformed_input() {
        let bytes = Rotation::generate(4, 1).to_bytes();
        let with_header_field = |offset: usize, value: &[u8]| {
            let mut edited = bytes.clone();
            edited[offset..offset + value.len()].copy_from_slice(value);
            Rotation::from_bytes(&edited)
        };
        let wrong_value_count = |actual_values| {
            Err(AssetError::InvalidLayout {
                expected_values: 16,
                actual_values,
            })
        };

        assert_eq!(
            Rotation::from_bytes(&bytes[..19]),
            Err(AssetError::InvalidSize {
                minimum: 20,
                actual: 19
            })
        );
        assert_eq!(
            with_header_field(0, b"TQNT"),
            Err(AssetError::InvalidMagic {
                expected: *b"TQRT",
                actual: *b"TQNT"
            })
        );
        assert_eq!(
            with_header_field(4, &0u32.to_le_bytes()),
            Err(AssetError::InvalidDim)
        );
        assert_eq!(
            with_header_field(4, &5u32.to_le_bytes()),
            Err(AssetError::InvalidLayout {
                expected_values: 25,
                actual_values: 16
            })
        );
        assert_eq!(
            Rotation::from_bytes(&bytes[..bytes.len() - 4]),
            wrong_value_count(15)
        );
        assert_eq!(
            Rotation::from_bytes(&bytes[..bytes.len() - 1]),
            wrong_value_count(15)
        );
        assert_eq!(
            Rotation::from_bytes(&[&bytes[..], &[0; 4]].concat()),
            wrong_value_count(17)
        );

        // Another asset kind is caught by its magic, not misread.
        let centroids = CentroidTable::generate(4, 4, 7).to_bytes();
        assert!(matches!(
            Rotation::from_bytes(&centroids),
            Err(AssetError::InvalidMagic { .. })
        ));
    }

    #[test]
    fn rotate_and_inverse_rotate_reject_mismatched_lengths() {
        let rotation = Rotation::generate(8, 1);
        let mismatch = |actual| {
            Err(AssetError::DimensionMismatch {
                expected: 8,
                actual,
            })
        };
        for apply in [Rotation::rotate, Rotation::inverse_rotate] {
            assert_eq!(apply(&rotation, &[0.0; 7], &mut [0.0; 8]), mismatch(7));
            assert_eq!(apply(&rotation, &[0.0; 8], &mut [0.0; 9]), mismatch(9));
        }
    }
}
