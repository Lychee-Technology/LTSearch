//! QJL sign sketch for the TurboQuant_prod residual stage.
//!
//! After the MSE stage, TurboQuant_prod keeps one bit per row of an m×d
//! matrix S with i.i.d. N(0, 1) entries: the signs `z = sign(S·r)` of the
//! residual r ∈ ℝ^d, together with γ = ‖r‖. For a query y,
//!
//! ```text
//! γ·√(π/2)/m · ⟨z, S·y⟩
//! ```
//!
//! estimates ⟨r, y⟩ without bias. For one Gaussian row s, `⟨s, r̂⟩` and
//! `⟨s, y⟩` (r̂ = r/‖r‖) are jointly normal with covariance ⟨r̂, y⟩, so
//! `E[sign(⟨s, r⟩)·⟨s, y⟩] = √(2/π)·⟨r̂, y⟩`. The rows are independent, so
//! the variance is exactly `(γ²/m)·((π/2)·‖y‖² − ⟨r̂, y⟩²)`. Both facts need
//! Gaussian rows: with the legacy codec's U[−1, 1] entries the expectation is
//! about ⟨r, y⟩/√3. The tests check the bias and the variance by Monte Carlo
//! over seeds, and that U[−1, 1] entries fail the bias check.
//!
//! # Generation
//!
//! Under the materialization contract (see [`super::codec_config`]) only
//! builders run [`QjlMatrix::generate`]; the query side loads the stored
//! matrix with [`QjlMatrix::from_bytes`].
//!
//! S is filled row by row from
//! [`fill_standard_normal`](super::fill_standard_normal)`(seed,
//! QJL_STREAM_ID, ..)`: `S[j][i]` is draw `j·d + i`, rounded to f32. A
//! matrix with fewer rows is therefore a prefix of one with more, for the
//! same `(dim, seed)`. `QJL_STREAM_ID` is an 8-byte tag like the rotation's
//! but a different one, so S is independent of Π even when a config sets
//! `qjl_seed == mse_seed`.
//!
//! The reproducibility guarantee and versioning are those of the
//! [rotation](super::rotation): `(dim, qjl_dim, seed, generator_version)`
//! determines the bytes on every IEEE 754 target, and any change to how
//! `generate` turns draws into a matrix bumps
//! [`GAUSSIAN_GENERATOR_VERSION`]. The golden test below fails on such a
//! change.
//!
//! # Encoding and scoring
//!
//! [`QjlMatrix::encode`] computes each row's product with the residual as a
//! sequential f32 sum in index order, like [`Rotation`](super::Rotation), so
//! a builder gets the same bytes on every target. Bit j is set when row j's
//! product is ≥ 0, so sign(0) is +1 (−0.0 included), and is stored LSB-first
//! in byte `j / 8`, as in the legacy codec. The bits past m in the last byte
//! are zero.
//!
//! [`QjlMatrix::prepare_query`] computes `p = S·y` and the scale `√(π/2)/m`
//! once per query. Per record, [`PreparedQjlQuery::estimate`] is then m
//! signed adds and two multiplies. The sum is a sequential f32 sum, the same
//! accumulation the legacy prepared scorer kept in #162.
//!
//! # File format
//!
//! All integers and values are little-endian.
//!
//! | offset | size        | field                                   |
//! |--------|-------------|-----------------------------------------|
//! | 0      | 4           | magic `TQJL`                            |
//! | 4      | 4           | `dim`, d (u32)                          |
//! | 8      | 4           | `qjl_dim`, m (u32)                      |
//! | 12     | 8           | `seed` (u64)                            |
//! | 20     | 4           | `generator_version` (u32)               |
//! | 24     | 4·m·d       | S as row-major f32, `S[j][i]` at `j·d + i` |
//!
//! A v4 static release stores it as [`QJL_FILE`].

use std::f64::consts::FRAC_PI_2;

use super::assets::{parse_values, write_values, AssetError};
use super::gaussian::{StandardNormalStream, GAUSSIAN_GENERATOR_VERSION};

/// The QJL matrix's file name in a v4 static release.
pub const QJL_FILE: &str = "qjl.bin";

const QJL_MAGIC: [u8; 4] = *b"TQJL";
const QJL_HEADER_SIZE: usize = 24;

/// The sampler keystream S is drawn from. Distinct from the rotation's, so
/// equal seeds don't make S a copy of the rotation's Gaussian draws.
const QJL_STREAM_ID: u64 = u64::from_le_bytes(*b"qjl_sign");

/// The m×d Gaussian matrix S of the QJL sketch.
///
/// A distinct type from [`ProjectionMatrix`](super::ProjectionMatrix) (the
/// legacy codec's uniform projection) and [`Rotation`](super::Rotation), so
/// neither can be passed where S is expected.
#[derive(Debug, Clone, PartialEq)]
pub struct QjlMatrix {
    /// d, the length of the vectors S multiplies.
    dim: u32,
    /// m, the number of rows and of sign bits per record.
    qjl_dim: u32,
    seed: u64,
    generator_version: u32,
    /// S, row-major: `S[j][i]` is `values[j·dim + i]`.
    values: Vec<f32>,
}

impl QjlMatrix {
    /// Generates the `(dim, qjl_dim, seed)` matrix; see the module docs.
    ///
    /// # Panics
    ///
    /// If `dim` or `qjl_dim` is zero.
    pub fn generate(dim: u32, qjl_dim: u32, seed: u64) -> Self {
        assert!(dim > 0 && qjl_dim > 0, "dimensions must be positive");

        let mut stream = StandardNormalStream::new(seed, QJL_STREAM_ID);
        let mut row = vec![0.0; dim as usize];
        let mut values = Vec::with_capacity(qjl_dim as usize * dim as usize);
        for _ in 0..qjl_dim {
            stream.fill(&mut row);
            values.extend(row.iter().map(|&value| value as f32));
        }

        Self {
            dim,
            qjl_dim,
            seed,
            generator_version: GAUSSIAN_GENERATOR_VERSION,
            values,
        }
    }

    /// Writes `sign(S·residual)` into `signs`, which must be
    /// [`signs_len`](Self::signs_len) bytes long. Every byte is overwritten.
    /// Allocation-free.
    ///
    /// A non-finite residual gives meaningless bits; callers reject
    /// non-finite input.
    pub fn encode(&self, residual: &[f32], signs: &mut [u8]) -> Result<(), AssetError> {
        check_len(self.dim as usize, residual.len())?;
        check_len(self.signs_len(), signs.len())?;

        signs.fill(0);
        for (j, row) in self.values.chunks_exact(residual.len()).enumerate() {
            let product: f32 = row.iter().zip(residual).map(|(s, r)| s * r).sum();
            signs[j / 8] |= u8::from(product >= 0.0) << (j % 8);
        }
        Ok(())
    }

    /// Computes everything [`PreparedQjlQuery::estimate`] needs from `query`.
    pub fn prepare_query(&self, query: &[f32]) -> Result<PreparedQjlQuery, AssetError> {
        check_len(self.dim as usize, query.len())?;

        let projected = self
            .values
            .chunks_exact(query.len())
            .map(|row| row.iter().zip(query).map(|(s, y)| s * y).sum())
            .collect();
        Ok(PreparedQjlQuery {
            projected,
            scale: (FRAC_PI_2.sqrt() / f64::from(self.qjl_dim)) as f32,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(QJL_HEADER_SIZE + self.values.len() * 4);
        out.extend_from_slice(&QJL_MAGIC);
        out.extend_from_slice(&self.dim.to_le_bytes());
        out.extend_from_slice(&self.qjl_dim.to_le_bytes());
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.extend_from_slice(&self.generator_version.to_le_bytes());
        write_values(&mut out, &self.values);
        out
    }

    /// Parses the file format in the module docs. Like
    /// [`Rotation::from_bytes`](super::Rotation::from_bytes), it checks the
    /// layout only; a loader compares the header fields against the
    /// release's codec config.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AssetError> {
        let Some((header, data)) = bytes.split_first_chunk::<QJL_HEADER_SIZE>() else {
            return Err(AssetError::InvalidSize {
                minimum: QJL_HEADER_SIZE,
                actual: bytes.len(),
            });
        };

        let magic: [u8; 4] = header[0..4].try_into().unwrap();
        if magic != QJL_MAGIC {
            return Err(AssetError::InvalidMagic {
                expected: QJL_MAGIC,
                actual: magic,
            });
        }
        let dim = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let qjl_dim = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let seed = u64::from_le_bytes(header[12..20].try_into().unwrap());
        let generator_version = u32::from_le_bytes(header[20..24].try_into().unwrap());
        let values = parse_values(data, qjl_dim, dim)?;

        Ok(Self {
            dim,
            qjl_dim,
            seed,
            generator_version,
            values,
        })
    }

    /// d.
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// m.
    pub fn qjl_dim(&self) -> u32 {
        self.qjl_dim
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn generator_version(&self) -> u32 {
        self.generator_version
    }

    /// Bytes per sign code, `⌈m/8⌉`.
    pub fn signs_len(&self) -> usize {
        self.qjl_dim.div_ceil(8) as usize
    }

    /// S, row-major.
    pub fn values(&self) -> &[f32] {
        &self.values
    }
}

/// The query side of the QJL estimate: `p = S·y` and `√(π/2)/m`.
///
/// It holds no matrix, so estimating a record can't do a matvec.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedQjlQuery {
    /// `p = S·y`, one value per row.
    projected: Vec<f32>,
    /// `√(π/2)/m`, computed in f64 and rounded once.
    scale: f32,
}

impl PreparedQjlQuery {
    /// `γ·√(π/2)/m·Σ_j z_j·p_j`, where z_j = ±1 is bit j of `signs`: the
    /// estimate of ⟨r, y⟩ for the residual r that `signs` and `gamma` were
    /// encoded from. Allocation-free.
    ///
    /// # Panics
    ///
    /// If `signs.len()` isn't `⌈m/8⌉` for the m this query was prepared
    /// with. A code from a matrix with another m would otherwise be scored
    /// against the wrong rows, or silently truncated.
    pub fn estimate(&self, signs: &[u8], gamma: f32) -> f32 {
        gamma * self.scale * self.signed_sum(signs)
    }

    /// `Σ_j z_j·p_j`, in row order.
    fn signed_sum(&self, signs: &[u8]) -> f32 {
        assert_eq!(
            signs.len(),
            self.projected.len().div_ceil(8),
            "sign code length doesn't match qjl_dim {}",
            self.projected.len()
        );

        let mut sum = 0.0;
        for (values, &byte) in self.projected.chunks(8).zip(signs) {
            for (bit, &value) in values.iter().enumerate() {
                sum += if (byte >> bit) & 1 == 1 {
                    value
                } else {
                    -value
                };
            }
        }
        sum
    }
}

fn check_len(expected: usize, actual: usize) -> Result<(), AssetError> {
    if actual != expected {
        return Err(AssetError::DimensionMismatch { expected, actual });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::fill_standard_normal;
    use crate::index::release_manifest::sha256_hex;
    use crate::index::rotation::ROTATION_STREAM_ID;
    use crate::index::ProjectionMatrix;

    /// sha256 of `QjlMatrix::generate(512, 512, 165).to_bytes()`, captured
    /// on x86_64 when `GAUSSIAN_GENERATOR_VERSION` was 1.
    const GOLDEN_512_BY_512_SEED_165_SHA256: &str =
        "91711f45c09a3780bd5378382032f744ffaab386cc76288fe07e537a67182b6f";

    /// The Monte Carlo checks below allow this many standard errors, as the
    /// issue specifies for the bias.
    const Z_TOLERANCE: f64 = 4.0;

    fn to_f64(values: &[f32]) -> Vec<f64> {
        values.iter().map(|&value| f64::from(value)).collect()
    }

    fn dot(lhs: &[f64], rhs: &[f64]) -> f64 {
        lhs.iter().zip(rhs).map(|(a, b)| a * b).sum()
    }

    fn with_values(dim: u32, qjl_dim: u32, values: Vec<f32>) -> QjlMatrix {
        assert_eq!(values.len(), dim as usize * qjl_dim as usize);
        QjlMatrix {
            dim,
            qjl_dim,
            seed: 0,
            generator_version: GAUSSIAN_GENERATOR_VERSION,
            values,
        }
    }

    #[test]
    fn generate_is_pinned() {
        // Re-capture the golden whenever the version is bumped.
        assert_eq!(GAUSSIAN_GENERATOR_VERSION, 1);
        let matrix = QjlMatrix::generate(512, 512, 165);
        assert_eq!(matrix.generator_version(), GAUSSIAN_GENERATOR_VERSION);
        assert_eq!(
            sha256_hex(&matrix.to_bytes()),
            GOLDEN_512_BY_512_SEED_165_SHA256
        );
    }

    #[test]
    fn entries_are_the_sampler_draws_row_by_row() {
        let (dim, qjl_dim, seed) = (24, 40, 9);
        let mut draws = vec![0.0; dim * qjl_dim];
        fill_standard_normal(seed, QJL_STREAM_ID, &mut draws);
        let expected: Vec<f32> = draws.iter().map(|&draw| draw as f32).collect();
        let matrix = QjlMatrix::generate(dim as u32, qjl_dim as u32, seed);
        assert_eq!(matrix.values(), expected);

        // So fewer rows give a prefix of the same matrix.
        let shorter = QjlMatrix::generate(dim as u32, 8, seed);
        assert_eq!(shorter.values(), &expected[..8 * dim]);
    }

    #[test]
    fn generate_is_deterministic_and_seed_dependent() {
        let matrix = QjlMatrix::generate(32, 16, 7);
        assert_eq!(QjlMatrix::generate(32, 16, 7).to_bytes(), matrix.to_bytes());
        for seed in [8, 1 << 40] {
            assert_ne!(QjlMatrix::generate(32, 16, seed).values(), matrix.values());
        }
    }

    #[test]
    fn s_is_independent_of_the_rotation_at_equal_seeds() {
        assert_ne!(QJL_STREAM_ID, ROTATION_STREAM_ID);
        let mut rotation_draws = vec![0.0; 64];
        fill_standard_normal(163, ROTATION_STREAM_ID, &mut rotation_draws);
        let matrix = QjlMatrix::generate(64, 1, 163);
        let shared = matrix
            .values()
            .iter()
            .zip(&rotation_draws)
            .filter(|&(&s, &g)| s == g as f32)
            .count();
        assert_eq!(shared, 0);
    }

    #[test]
    #[should_panic(expected = "dimensions must be positive")]
    fn generate_rejects_zero_dim() {
        QjlMatrix::generate(0, 8, 1);
    }

    #[test]
    #[should_panic(expected = "dimensions must be positive")]
    fn generate_rejects_zero_qjl_dim() {
        QjlMatrix::generate(8, 0, 1);
    }

    #[test]
    fn encode_sets_bit_j_from_the_sign_of_row_j() {
        // d = 2, m = 11: the residual (1, 1) makes row j's product the sum
        // of its entries. The last two rows give +0.0 and −0.0, both ≥ 0.
        let rows: [[f32; 2]; 11] = [
            [1.0, 0.0],
            [-1.0, 0.0],
            [0.5, -0.25],
            [-0.5, 0.25],
            [0.0, 2.0],
            [0.0, -2.0],
            [3.0, -4.0],
            [-3.0, 4.0],
            [1e-30, 0.0],
            [1.0, -1.0],
            [-0.0, -0.0],
        ];
        let matrix = with_values(2, 11, rows.concat());
        assert_eq!(matrix.signs_len(), 2);

        let mut signs = [0xff; 2];
        matrix.encode(&[1.0, 1.0], &mut signs).unwrap();
        // Rows 0, 2, 4, 7 and 8 are positive; 9 and 10 are zero. Bits 11..16
        // of the second byte are padding and stay clear.
        assert_eq!(signs, [0b1001_0101, 0b0000_0111]);
    }

    #[test]
    fn estimate_is_gamma_times_scale_times_the_signed_projection_sum() {
        let (dim, qjl_dim) = (8, 13);
        let matrix = QjlMatrix::generate(dim, qjl_dim, 3);
        let query: Vec<f32> = (0..dim).map(|i| i as f32 / 8.0 - 0.4).collect();
        let prepared = matrix.prepare_query(&query).unwrap();
        let signs = [0b1010_0110, 0b0001_0011];
        let gamma = 0.3;

        let query = to_f64(&query);
        let projected: Vec<f64> = matrix
            .values()
            .chunks_exact(dim as usize)
            .map(|row| dot(&to_f64(row), &query))
            .collect();
        let signed_sum: f64 = projected
            .iter()
            .enumerate()
            .map(|(j, p)| {
                if (signs[j / 8] >> (j % 8)) & 1 == 1 {
                    *p
                } else {
                    -p
                }
            })
            .sum();
        let factor = f64::from(gamma) * FRAC_PI_2.sqrt() / f64::from(qjl_dim);
        let expected = factor * signed_sum;
        // f32 rounding, relative to the terms' magnitudes rather than to a
        // sum that may cancel.
        let magnitude = factor * projected.iter().map(|p| p.abs()).sum::<f64>();
        let actual = f64::from(prepared.estimate(&signs, gamma));
        assert!(
            (actual - expected).abs() <= 1e-6 * magnitude,
            "{actual} vs {expected}"
        );
    }

    #[test]
    fn length_mismatches_are_errors() {
        let matrix = QjlMatrix::generate(8, 12, 1);
        let mismatch = |expected, actual| AssetError::DimensionMismatch { expected, actual };
        assert_eq!(matrix.encode(&[0.0; 7], &mut [0; 2]), Err(mismatch(8, 7)));
        // A buffer sized for d or m bits instead of ⌈m/8⌉ bytes.
        assert_eq!(matrix.encode(&[0.0; 8], &mut [0; 1]), Err(mismatch(2, 1)));
        assert_eq!(matrix.encode(&[0.0; 8], &mut [0; 12]), Err(mismatch(2, 12)));
        assert_eq!(matrix.prepare_query(&[0.0; 12]), Err(mismatch(8, 12)));
    }

    #[test]
    #[should_panic(expected = "sign code length doesn't match qjl_dim 24")]
    fn estimate_rejects_a_code_for_another_qjl_dim() {
        let prepared = QjlMatrix::generate(8, 24, 1)
            .prepare_query(&[0.5; 8])
            .unwrap();
        let other = QjlMatrix::generate(8, 16, 1);
        let mut signs = vec![0; other.signs_len()];
        other.encode(&[0.5; 8], &mut signs).unwrap();
        prepared.estimate(&signs, 1.0);
    }

    #[test]
    fn bytes_round_trip_in_the_documented_layout() {
        let matrix = QjlMatrix::generate(4, 3, 0x0102_0304_0506_0708);
        let bytes = matrix.to_bytes();
        assert_eq!(bytes.len(), 24 + 4 * 12);
        assert_eq!(&bytes[0..4], b"TQJL");
        assert_eq!(bytes[4..8], 4u32.to_le_bytes());
        assert_eq!(bytes[8..12], 3u32.to_le_bytes());
        assert_eq!(bytes[12..20], 0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(bytes[20..24], GAUSSIAN_GENERATOR_VERSION.to_le_bytes());
        // Row-major: S[1][0] follows S[0][3].
        assert_eq!(bytes[40..44], matrix.values()[4].to_le_bytes());

        let parsed = QjlMatrix::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, matrix);
        assert_eq!(parsed.dim(), 4);
        assert_eq!(parsed.qjl_dim(), 3);
        assert_eq!(parsed.seed(), 0x0102_0304_0506_0708);
        assert_eq!(parsed.generator_version(), GAUSSIAN_GENERATOR_VERSION);
    }

    #[test]
    fn from_bytes_rejects_malformed_input() {
        let bytes = QjlMatrix::generate(4, 3, 1).to_bytes();
        let with_header_field = |offset: usize, value: &[u8]| {
            let mut edited = bytes.clone();
            edited[offset..offset + value.len()].copy_from_slice(value);
            QjlMatrix::from_bytes(&edited)
        };
        let wrong_value_count = |actual_values| {
            Err(AssetError::InvalidLayout {
                expected_values: 12,
                actual_values,
            })
        };

        assert_eq!(
            QjlMatrix::from_bytes(&bytes[..23]),
            Err(AssetError::InvalidSize {
                minimum: 24,
                actual: 23
            })
        );
        assert_eq!(
            with_header_field(0, b"TQRT"),
            Err(AssetError::InvalidMagic {
                expected: *b"TQJL",
                actual: *b"TQRT"
            })
        );
        assert_eq!(
            with_header_field(4, &0u32.to_le_bytes()),
            Err(AssetError::InvalidDim)
        );
        assert_eq!(
            with_header_field(8, &0u32.to_le_bytes()),
            Err(AssetError::InvalidDim)
        );
        assert_eq!(
            with_header_field(8, &4u32.to_le_bytes()),
            Err(AssetError::InvalidLayout {
                expected_values: 16,
                actual_values: 12
            })
        );
        assert_eq!(
            QjlMatrix::from_bytes(&bytes[..bytes.len() - 4]),
            wrong_value_count(11)
        );
        assert_eq!(
            QjlMatrix::from_bytes(&bytes[..bytes.len() - 1]),
            wrong_value_count(11)
        );
        assert_eq!(
            QjlMatrix::from_bytes(&[&bytes[..], &[0; 4]].concat()),
            wrong_value_count(13)
        );

        // A rotation file is caught by its magic, not misread.
        let rotation = crate::index::Rotation::generate(4, 1).to_bytes();
        assert!(matches!(
            QjlMatrix::from_bytes(&rotation),
            Err(AssetError::InvalidMagic { .. })
        ));
    }

    // Monte Carlo checks of the estimator. Each seed is an independent S; r
    // and y are fixed.

    /// d for the bias checks: small enough for the dev profile, and ≠ m so a
    /// scale computed from d instead of m fails.
    const BIAS_DIM: usize = 32;
    const BIAS_QJL_DIM: u32 = 128;
    const BIAS_SEEDS: u64 = 1000;

    /// A fixed residual r with ‖r‖ = 0.35, about the residual norm of a unit
    /// vector under the 2-bit codebook, and a unit query y with
    /// ⟨r̂, y⟩ = 0.5.
    fn residual_and_query(dim: usize) -> (Vec<f32>, Vec<f32>) {
        let unit = |v: Vec<f64>| {
            let length = dot(&v, &v).sqrt();
            v.into_iter()
                .map(|value| value / length)
                .collect::<Vec<_>>()
        };
        let r_hat = unit((0..dim).map(|i| 1.0 / (i + 1) as f64 - 0.2).collect());
        let other: Vec<f64> = (0..dim)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 } * ((i % 5) + 1) as f64)
            .collect();
        let along = dot(&other, &r_hat);
        let w = unit(
            other
                .iter()
                .zip(&r_hat)
                .map(|(o, r)| o - along * r)
                .collect(),
        );

        let residual = r_hat.iter().map(|&value| (0.35 * value) as f32).collect();
        let query = r_hat
            .iter()
            .zip(&w)
            .map(|(r, w)| (0.5 * r + 0.75f64.sqrt() * w) as f32)
            .collect();
        (residual, query)
    }

    /// The estimate of ⟨r, y⟩ through S = `matrix`, with the prepared query
    /// passed through `adjust`.
    fn estimate(
        matrix: &QjlMatrix,
        residual: &[f32],
        query: &[f32],
        adjust: impl Fn(PreparedQjlQuery) -> PreparedQjlQuery,
    ) -> f64 {
        let gamma = dot(&to_f64(residual), &to_f64(residual)).sqrt() as f32;
        let mut signs = vec![0; matrix.signs_len()];
        matrix.encode(residual, &mut signs).unwrap();
        let prepared = adjust(matrix.prepare_query(query).unwrap());
        f64::from(prepared.estimate(&signs, gamma))
    }

    fn mean_and_variance(samples: &[f64]) -> (f64, f64) {
        let n = samples.len() as f64;
        let mean = samples.iter().sum::<f64>() / n;
        let variance = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / (n - 1.0);
        (mean, variance)
    }

    /// (bias, standard error) of the mean estimate against ⟨r, y⟩.
    fn bias_and_standard_error(
        matrix: impl Fn(u64) -> QjlMatrix,
        adjust: impl Fn(PreparedQjlQuery) -> PreparedQjlQuery,
    ) -> (f64, f64) {
        let (residual, query) = residual_and_query(BIAS_DIM);
        let samples: Vec<f64> = (0..BIAS_SEEDS)
            .map(|seed| estimate(&matrix(seed), &residual, &query, &adjust))
            .collect();
        let (mean, variance) = mean_and_variance(&samples);
        let truth = dot(&to_f64(&residual), &to_f64(&query));
        let standard_error = (variance / samples.len() as f64).sqrt();
        println!(
            "⟨r, y⟩ = {truth:.6}, mean estimate {mean:.6}, bias {:.3e}, \
             standard error {standard_error:.3e}, z = {:.2}",
            mean - truth,
            (mean - truth) / standard_error
        );
        (mean - truth, standard_error)
    }

    fn gaussian(seed: u64) -> QjlMatrix {
        QjlMatrix::generate(BIAS_DIM as u32, BIAS_QJL_DIM, seed)
    }

    /// Observed with ⟨r, y⟩ = 0.175: bias −1.9e-4, standard error 1.1e-3
    /// (z = −0.18).
    #[test]
    fn estimate_is_unbiased() {
        let (bias, standard_error) = bias_and_standard_error(gaussian, |prepared| prepared);
        assert!(
            bias.abs() <= Z_TOLERANCE * standard_error,
            "bias {bias:e}, standard error {standard_error:e}"
        );
    }

    /// The bias check has teeth: without the √(π/2)/m factor the mean is
    /// m/√(π/2) ≈ 102 times ⟨r, y⟩, and with only 1/m it is √(2/π) ≈ 0.80
    /// times. Observed z-scores 158 and −41.
    #[test]
    fn a_missing_scale_factor_fails_the_bias_check() {
        for scale in [1.0, 1.0 / BIAS_QJL_DIM as f32] {
            let (bias, standard_error) = bias_and_standard_error(gaussian, |prepared| {
                PreparedQjlQuery { scale, ..prepared }
            });
            assert!(
                bias.abs() > Z_TOLERANCE * standard_error,
                "scale {scale}: bias {bias:e}, standard error {standard_error:e}"
            );
        }
    }

    /// And without Gaussian rows: the legacy codec's U[−1, 1] projection has
    /// entry variance 1/3, so the mean is about ⟨r, y⟩/√3. Observed
    /// z-score −110 (mean 0.103 against ⟨r, y⟩ = 0.175).
    #[test]
    fn uniform_s_fails_the_bias_check() {
        let uniform = |seed| {
            let projection = ProjectionMatrix::generate(BIAS_DIM as u32, BIAS_QJL_DIM, seed);
            with_values(BIAS_DIM as u32, BIAS_QJL_DIM, projection.values().to_vec())
        };
        let (bias, standard_error) = bias_and_standard_error(uniform, |prepared| prepared);
        assert!(
            bias.abs() > Z_TOLERANCE * standard_error,
            "bias {bias:e}, standard error {standard_error:e}"
        );
    }

    /// Observed variance over theoretical variance at each m, for d = `dim`
    /// and seeds `0..seeds`. Each seed generates the largest m once and takes
    /// the smaller ones as its leading rows, which is what `generate` returns
    /// for them (`entries_are_the_sampler_draws_row_by_row`).
    fn variance_ratios(dim: usize, seeds: u64, qjl_dims: &[u32]) -> Vec<f64> {
        let (residual, query) = residual_and_query(dim);
        let largest = *qjl_dims.iter().max().unwrap();
        let mut samples = vec![Vec::new(); qjl_dims.len()];
        for seed in 0..seeds {
            let full = QjlMatrix::generate(dim as u32, largest, seed);
            for (&qjl_dim, samples) in qjl_dims.iter().zip(&mut samples) {
                let rows = full.values[..qjl_dim as usize * dim].to_vec();
                let matrix = with_values(dim as u32, qjl_dim, rows);
                samples.push(estimate(&matrix, &residual, &query, |prepared| prepared));
            }
        }

        let gamma = f64::from(dot(&to_f64(&residual), &to_f64(&residual)).sqrt() as f32);
        let r_hat_dot_y = dot(&to_f64(&residual), &to_f64(&query)) / gamma;
        let y_norm_sq = dot(&to_f64(&query), &to_f64(&query));
        qjl_dims
            .iter()
            .zip(&samples)
            .map(|(&qjl_dim, samples)| {
                let (_, observed) = mean_and_variance(samples);
                let theory = gamma * gamma / f64::from(qjl_dim)
                    * (FRAC_PI_2 * y_norm_sq - r_hat_dot_y * r_hat_dot_y);
                println!("m = {qjl_dim}: variance {observed:.4e}, theory {theory:.4e}");
                observed / theory
            })
            .collect()
    }

    /// Checks observed/theoretical variance within `tolerance` at
    /// m = 128, 512 and 2048, and the 4× drop between consecutive m.
    fn assert_variance_matches_theory(dim: usize, seeds: u64, tolerance: f64) {
        let qjl_dims = [128, 512, 2048];
        let ratios = variance_ratios(dim, seeds, &qjl_dims);
        for (qjl_dim, ratio) in qjl_dims.iter().zip(&ratios) {
            assert!(
                (ratio - 1.0).abs() <= tolerance,
                "m = {qjl_dim}: observed/theory {ratio}"
            );
        }
        // Var(m)/Var(4m) is 4 in theory: the variance falls as 1/m.
        for pair in ratios.windows(2) {
            let drop = pair[0] / pair[1];
            assert!(
                (drop - 1.0).abs() <= tolerance,
                "Var(m)/(4·Var(4m)) = {drop}"
            );
        }
    }

    /// 500 seeds give each sample variance a relative standard error of
    /// √(2/499) ≈ 6.3%, so the tolerance is 4 of those, 25%. Dropping √(π/2)
    /// from the scale would move the variance by −36%. Observed
    /// variance/theory 0.91, 0.92 and 1.01, and Var(m)/(4·Var(4m)) 0.99 and
    /// 0.91.
    #[test]
    fn estimate_variance_matches_theory_and_falls_as_one_over_m() {
        assert_variance_matches_theory(8, 500, 0.25);
    }

    /// 10 000 seeds: relative standard error 1.4%, tolerance 6%. Observed
    /// variance/theory 1.030, 1.015 and 1.002.
    #[test]
    #[ignore = "24 s under --release: cargo test --release --lib qjl -- --ignored"]
    fn estimate_variance_matches_theory_with_more_seeds() {
        assert_variance_matches_theory(64, 10_000, 0.06);
    }
}
