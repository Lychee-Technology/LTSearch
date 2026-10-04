//! Lloyd-Max scalar codebook for the TurboQuant_prod MSE stage.
//!
//! After a Haar rotation ([`Rotation`](super::Rotation)), every coordinate
//! of a unit vector in ℝ^d has the density
//!
//! ```text
//! f(x) = Γ(d/2) / (√π·Γ((d−1)/2)) · (1 − x²)^((d−3)/2),   x ∈ [−1, 1],
//! ```
//!
//! ≈ N(0, 1/d) at d = 512. TurboQuant quantizes every rotated coordinate
//! with one scalar quantizer optimized for f: a [`LloydMaxCodebook`] of
//! `2^bits` centroids shared by all coordinates. It is a distinct type from
//! the legacy codec's per-dimension [`CentroidTable`](super::CentroidTable),
//! so neither can be passed where the other is expected.
//!
//! # Committed codebook
//!
//! The production codebook, d = 512 and b = 2, is committed as constants
//! ([`LloydMaxCodebook::committed`]), so its values are visible in review and
//! no build runs the solver. A test re-derives them with
//! [`LloydMaxSolution::solve`]. A release stores the codebook it was encoded
//! with, as [`CODEBOOK_FILE`], so the query side never recomputes it.
//!
//! # File format
//!
//! All integers and values are little-endian.
//!
//! | offset | size      | field                                   |
//! |--------|-----------|-----------------------------------------|
//! | 0      | 4         | magic `TQCB`                            |
//! | 4      | 4         | `dim` (u32)                             |
//! | 8      | 4         | `bits` (u32)                            |
//! | 12     | 4·2^bits  | the centroids as f32, ascending         |
//!
//! The thresholds are not stored: they are a function of the centroids.
//!
//! # Solver
//!
//! [`LloydMaxSolution::solve`] runs Lloyd's algorithm on f in f64, for any
//! d ≥ 3 and b ∈ 1..=4. It starts from the N(0, 1) Lloyd-Max levels scaled
//! by 1/√d (by less when d is so small that the outermost would land beyond
//! 1), then alternates centroids ← E[X | X ∈ cell] and thresholds ←
//! midpoints of adjacent centroids, until no centroid moves by more than
//! 10⁻¹³/√d.
//!
//! f is log-concave, so its Lloyd-Max quantizer is unique (Fleischer 1964)
//! and Lloyd's algorithm converges to it from any start (Kieffer 1982); the
//! start only changes the iteration count. f is also even, so that unique
//! quantizer is symmetric, with a threshold at 0. The solver therefore
//! iterates on the positive cells only and mirrors them: the result is
//! exactly symmetric, not symmetric up to rounding.
//!
//! Integrals over a cell are taken in θ = asin x, where
//! `f(x)·dx = C·cos^(d−2)θ·dθ` is smooth for every d ≥ 3. f itself is not
//! smooth at ±1 for small d: at d = 4 it is proportional to √(1 − x²). Each
//! cell is split into 16 panels of 8-point Gauss–Legendre. Integration stops
//! at θ = 12/√(d−2) when that is below π/2: since ln cos θ ≤ −θ²/2, the
//! integrand there is below e⁻⁷² of its peak and the mass beyond is below
//! 10⁻³². Without the cut, at large d all of the outer cell's mass would sit
//! in the first of its panels. The normalization C = Γ(d/2)/(√π·Γ((d−1)/2))
//! is found the same way, by integrating cos^(d−2)θ: from ln Γ, the
//! difference of two values near 6·10⁶ at d = 2²⁰ loses 5·10⁻¹⁰ to
//! cancellation.
//!
//! # Reproducibility
//!
//! The solver uses only `+ − × ÷ sqrt` and pure-Rust `libm` functions
//! (`asin`, `sin`, `log1p`, `exp`), in a fixed order on one
//! thread, so `(dim, bits)` gives bit-identical output on every target with
//! IEEE 754 binary64 arithmetic, as for the [Gaussian
//! sampler](super::gaussian). A golden test pins the d = 512, b = 2 solution.

use std::f64::consts::FRAC_PI_2;

use super::assets::{parse_values, write_values, AssetError};

/// The codebook's file name in a v4 static release.
pub const CODEBOOK_FILE: &str = "codebook.bin";

const CODEBOOK_MAGIC: [u8; 4] = *b"TQCB";
const CODEBOOK_HEADER_SIZE: usize = 12;
/// The widest index a codebook file can declare. `TurboQuantConfig` accepts
/// the same range.
const MAX_BITS: u32 = 4;

/// Centroids of the committed codebook for d = 512, b = 2:
/// [`LloydMaxSolution::solve`]`(512, 2)` rounded to f32. The thresholds are
/// derived from them: −0.043350283, 0 and 0.043350287.
///
/// Changing these values changes every TurboQuant_prod index built
/// afterwards. The re-derivation test fails if they stop matching the solver.
const D512_B2_CENTROIDS: [f32; 4] = [-0.06669391, -0.02000666, 0.02000666, 0.06669391];

/// Positive levels of the N(0, 1) Lloyd-Max quantizer with `2^b` levels, for
/// b = 1..=4 (Max 1960), recomputed to 10 digits. The solver starts from
/// them.
const GAUSSIAN_POSITIVE_LEVELS: [&[f64]; 4] = [
    &[0.7978845608],
    &[0.4527800346, 1.510417608],
    &[0.2450941789, 0.7560052812, 1.343909279, 2.151945705],
    &[
        0.1283950299,
        0.3880482995,
        0.6567591185,
        0.9423404565,
        1.256231197,
        1.618046386,
        2.069017227,
        2.732589571,
    ],
];

/// Lloyd's algorithm stops once no centroid moves by more than this times
/// 1/√d, the coordinate's standard deviation.
const CONVERGENCE_TOLERANCE: f64 = 1e-13;
/// Lloyd's algorithm always converges on f, so reaching this is a bug. The
/// slowest case tested, d = 8 and b = 4, takes 673 iterations.
const MAX_ITERATIONS: u32 = 10_000;

/// Positive nodes of the 8-point Gauss–Legendre rule on [−1, 1], each with
/// its weight. The rule uses every node with both signs.
const GAUSS_LEGENDRE_8: [(f64, f64); 4] = [
    (0.1834346424956498, 0.362683783378362),
    (0.525532409916329, 0.31370664587788727),
    (0.7966664774136267, 0.22238103445337448),
    (0.9602898564975363, 0.10122853629037626),
];
const PANELS_PER_CELL: u32 = 16;
/// Integration stops at θ = this / √(d−2); see the module docs.
const THETA_CUTOFF_SCALE: f64 = 12.0;

/// A scalar quantizer shared by every coordinate: `2^bits` ascending
/// centroids and the `2^bits − 1` thresholds between them.
///
/// Threshold k is the smallest f32 at or above the exact midpoint of
/// centroids k and k + 1, which makes [`encode`](Self::encode) return the
/// nearest centroid for every f32. Rounding the midpoint to nearest instead
/// can land one f32 below it, and that f32 would encode to the farther
/// centroid. The price is that the thresholds of a symmetric codebook are
/// symmetric only to within one f32: the committed one's are −0.043350283,
/// 0 and 0.043350287, because its midpoint falls between those two
/// magnitudes.
#[derive(Debug, Clone, PartialEq)]
pub struct LloydMaxCodebook {
    /// The d whose coordinate density the centroids are optimized for.
    dim: u32,
    bits: u8,
    centroids: Vec<f32>,
    thresholds: Vec<f32>,
}

impl LloydMaxCodebook {
    /// The committed codebook for `(dim, bits)`. Only (512, 2) is committed;
    /// other pairs return `None`.
    pub fn committed(dim: u32, bits: u8) -> Option<Self> {
        match (dim, bits) {
            (512, 2) => Some(Self::from_centroids(dim, bits, D512_B2_CENTROIDS.to_vec())),
            _ => None,
        }
    }

    pub(super) fn from_centroids(dim: u32, bits: u8, centroids: Vec<f32>) -> Self {
        debug_assert_eq!(centroids.len(), 1 << bits);
        debug_assert!(centroids.is_sorted_by(|lower, upper| lower < upper));
        let thresholds = centroids
            .windows(2)
            .map(|pair| threshold_between(pair[0], pair[1]))
            .collect();
        Self {
            dim,
            bits,
            centroids,
            thresholds,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CODEBOOK_HEADER_SIZE + self.centroids.len() * 4);
        out.extend_from_slice(&CODEBOOK_MAGIC);
        out.extend_from_slice(&self.dim.to_le_bytes());
        out.extend_from_slice(&u32::from(self.bits).to_le_bytes());
        write_values(&mut out, &self.centroids);
        out
    }

    /// Parses the file format in the module docs. It checks that the bytes
    /// are a codebook: `2^bits` finite, strictly ascending centroids. Whether
    /// it is the codebook a codec expects is for the loader to check
    /// ([`TurboQuantProdV1::from_assets`](super::TurboQuantProdV1::from_assets)).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AssetError> {
        let Some((header, data)) = bytes.split_first_chunk::<CODEBOOK_HEADER_SIZE>() else {
            return Err(AssetError::InvalidSize {
                minimum: CODEBOOK_HEADER_SIZE,
                actual: bytes.len(),
            });
        };

        let magic: [u8; 4] = header[0..4].try_into().unwrap();
        if magic != CODEBOOK_MAGIC {
            return Err(AssetError::InvalidMagic {
                expected: CODEBOOK_MAGIC,
                actual: magic,
            });
        }
        let dim = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if dim == 0 {
            return Err(AssetError::InvalidDim);
        }
        let bits = u32::from_le_bytes(header[8..12].try_into().unwrap());
        if !(1..=MAX_BITS).contains(&bits) {
            return Err(AssetError::UnsupportedCodebookBits { bits });
        }
        let centroids = parse_values(data, 1 << bits, 1)?;
        let ascending = centroids.is_sorted_by(|lower, upper| lower < upper);
        if !ascending || centroids.iter().any(|centroid| !centroid.is_finite()) {
            return Err(AssetError::CentroidsNotAscending);
        }

        Ok(Self::from_centroids(dim, bits as u8, centroids))
    }

    /// The index of the cell `x` falls in: the number of thresholds at or
    /// below `x`. For every f32 that is the index of the centroid nearest
    /// `x`, the upper one if `x` is exactly halfway.
    ///
    /// Every threshold is compared, with no search or early exit, so the
    /// cost doesn't depend on `x`. Values beyond the outer thresholds,
    /// infinities included, map to the outer cells. NaN compares below every
    /// threshold and maps to 0; callers reject non-finite input.
    pub fn encode(&self, x: f32) -> u8 {
        self.thresholds
            .iter()
            .map(|&threshold| u8::from(x >= threshold))
            .sum()
    }

    /// The centroid of cell `index`.
    ///
    /// # Panics
    ///
    /// If `index >= 2^bits`.
    pub fn decode(&self, index: u8) -> f32 {
        self.centroids[usize::from(index)]
    }

    pub fn dim(&self) -> u32 {
        self.dim
    }

    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// The `2^bits` centroids, ascending.
    pub fn centroids(&self) -> &[f32] {
        &self.centroids
    }

    /// The `2^bits − 1` thresholds, ascending.
    pub fn thresholds(&self) -> &[f32] {
        &self.thresholds
    }
}

/// The smallest f32 at or above the midpoint of `lower` and `upper`.
fn threshold_between(lower: f32, upper: f32) -> f32 {
    // f64 holds the sum of two f32 values exactly when their magnitudes are
    // within a factor of 2²⁷ of each other, as adjacent centroids are, so
    // this is the exact midpoint.
    let midpoint = midpoint(f64::from(lower), f64::from(upper));
    let nearest = midpoint as f32;
    if f64::from(nearest) < midpoint {
        nearest.next_up()
    } else {
        nearest
    }
}

/// The f64 Lloyd-Max quantizer for one coordinate of a uniformly random unit
/// vector in ℝ^dim; see the module docs. Offline only.
#[derive(Debug, Clone, PartialEq)]
pub struct LloydMaxSolution {
    dim: u32,
    bits: u8,
    centroids: Vec<f64>,
    thresholds: Vec<f64>,
    coordinate_mse: f64,
    iterations: u32,
}

impl LloydMaxSolution {
    /// Runs Lloyd's algorithm for `(dim, bits)`; see the module docs.
    ///
    /// # Panics
    ///
    /// If `dim < 3` or `bits` is outside 1..=4.
    pub fn solve(dim: u32, bits: u8) -> Self {
        assert!(dim >= 3, "dim must be at least 3, got {dim}");
        assert!((1..=4).contains(&bits), "bits must be in 1..=4, got {bits}");
        let density = CoordinateDensity::new(dim);
        let sigma = 1.0 / f64::from(dim).sqrt();
        let tolerance = CONVERGENCE_TOLERANCE * sigma;

        // Centroids of the cells in [0, 1], ascending. Scaling the outermost
        // Gaussian level to at most 1 keeps every starting threshold inside
        // the support; after one iteration every centroid is a cell mean.
        let gaussian = GAUSSIAN_POSITIVE_LEVELS[usize::from(bits) - 1];
        let scale = sigma.min(1.0 / gaussian[gaussian.len() - 1]);
        let mut positive: Vec<f64> = gaussian.iter().map(|level| level * scale).collect();
        let mut iterations = 0;
        loop {
            let next: Vec<f64> = density
                .positive_cells(&positive)
                .windows(2)
                .map(|cell| {
                    density.integrate(cell[0], cell[1], |x| x)
                        / density.integrate(cell[0], cell[1], |_| 1.0)
                })
                .collect();
            let shift = next
                .iter()
                .zip(&positive)
                .map(|(new, old)| (new - old).abs())
                .fold(0.0, f64::max);
            positive = next;
            iterations += 1;
            if shift <= tolerance {
                break;
            }
            assert!(
                iterations < MAX_ITERATIONS,
                "Lloyd iteration for dim {dim}, bits {bits} did not converge"
            );
        }

        // Each half of the line contributes the same squared error.
        let coordinate_mse = 2.0
            * density
                .positive_cells(&positive)
                .windows(2)
                .zip(&positive)
                .map(|(cell, &centroid)| {
                    density.integrate(cell[0], cell[1], |x| (x - centroid) * (x - centroid))
                })
                .sum::<f64>();

        let centroids: Vec<f64> = positive
            .iter()
            .rev()
            .map(|&centroid| -centroid)
            .chain(positive.iter().copied())
            .collect();
        let thresholds = centroids
            .windows(2)
            .map(|pair| midpoint(pair[0], pair[1]))
            .collect();
        Self {
            dim,
            bits,
            centroids,
            thresholds,
            coordinate_mse,
            iterations,
        }
    }

    pub fn dim(&self) -> u32 {
        self.dim
    }

    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// The `2^bits` centroids, ascending.
    pub fn centroids(&self) -> &[f64] {
        &self.centroids
    }

    /// The `2^bits − 1` midpoints between adjacent centroids.
    pub fn thresholds(&self) -> &[f64] {
        &self.thresholds
    }

    /// E[(X − Q(X))²] for one coordinate X ~ f.
    pub fn coordinate_mse(&self) -> f64 {
        self.coordinate_mse
    }

    /// E‖x − Q(x)‖² for a uniformly random unit vector x ∈ ℝ^dim, with Q
    /// applied to every coordinate: `dim · coordinate_mse`. This is
    /// TurboQuant's D_mse (Theorem 1).
    pub fn distortion(&self) -> f64 {
        f64::from(self.dim) * self.coordinate_mse
    }

    /// Lloyd iterations run, including the last, which moved no centroid by
    /// more than the tolerance.
    pub fn iterations(&self) -> u32 {
        self.iterations
    }

    /// The codebook with these centroids rounded to f32.
    pub fn to_codebook(&self) -> LloydMaxCodebook {
        LloydMaxCodebook::from_centroids(
            self.dim,
            self.bits,
            self.centroids
                .iter()
                .map(|&centroid| centroid as f32)
                .collect(),
        )
    }
}

fn midpoint(lower: f64, upper: f64) -> f64 {
    (lower + upper) / 2.0
}

/// The density f of one coordinate of a uniformly random unit vector in
/// ℝ^d, integrated in θ = asin x as the module docs describe.
struct CoordinateDensity {
    /// (d − 2)/2: `f(x)·dx = C·cos^(d−2)θ·dθ = C·(1 − x²)^((d−2)/2)·dθ`.
    half_exponent: f64,
    /// C, f's normalization constant, computed with the same quadrature as
    /// every other integral.
    normalization: f64,
    /// The θ at which integration stops.
    theta_max: f64,
}

impl CoordinateDensity {
    fn new(dim: u32) -> Self {
        let d = f64::from(dim);
        let mut density = Self {
            half_exponent: (d - 2.0) / 2.0,
            normalization: 1.0,
            theta_max: (THETA_CUTOFF_SCALE / (d - 2.0).sqrt()).min(FRAC_PI_2),
        };
        density.normalization = 1.0 / (2.0 * density.integrate(0.0, density.theta_max, |_| 1.0));
        density
    }

    /// The θ bounds of the cells in [0, 1] whose centroids are `positive`:
    /// 0, then the midpoints between adjacent centroids, then the cutoff.
    fn positive_cells(&self, positive: &[f64]) -> Vec<f64> {
        let inner = positive
            .windows(2)
            .map(|pair| libm::asin(midpoint(pair[0], pair[1])));
        std::iter::once(0.0)
            .chain(inner)
            .chain(std::iter::once(self.theta_max))
            .collect()
    }

    /// `∫ g(x)·f(x) dx` over x from `sin θ_lo` to `sin θ_hi`.
    fn integrate(&self, theta_lo: f64, theta_hi: f64, g: impl Fn(f64) -> f64) -> f64 {
        let half_width = (theta_hi - theta_lo) / f64::from(2 * PANELS_PER_CELL);
        let mut sum = 0.0;
        for panel in 0..PANELS_PER_CELL {
            let center = theta_lo + f64::from(2 * panel + 1) * half_width;
            for (node, weight) in GAUSS_LEGENDRE_8 {
                for theta in [center - node * half_width, center + node * half_width] {
                    let x = libm::sin(theta);
                    // cos^(d−2)θ = (1 − x²)^((d−2)/2). pow(cos θ, d − 2) would
                    // multiply cos θ's rounding error near 1 by d − 2: 10⁻¹⁰
                    // relative noise at d = 2²⁰, too much to converge.
                    let density = libm::exp(self.half_exponent * libm::log1p(-x * x));
                    sum += weight * g(x) * density;
                }
            }
        }
        self.normalization * half_width * sum
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;
    use std::sync::OnceLock;

    use super::*;
    use crate::index::gaussian::StandardNormalStream;

    /// Five standard errors.
    const Z_TOLERANCE: f64 = 5.0;

    fn committed() -> LloydMaxCodebook {
        LloydMaxCodebook::committed(512, 2).unwrap()
    }

    fn solution_512_2() -> &'static LloydMaxSolution {
        static SOLUTION: OnceLock<LloydMaxSolution> = OnceLock::new();
        SOLUTION.get_or_init(|| LloydMaxSolution::solve(512, 2))
    }

    /// The spacing of f32 values at `x`'s magnitude.
    fn ulp(x: f32) -> f64 {
        let magnitude = x.abs();
        f64::from(magnitude.next_up()) - f64::from(magnitude)
    }

    /// C = Γ(d/2)/(√π·Γ((d−1)/2)) without ln Γ or quadrature: C = 1/2 at
    /// d = 3 and 2/π at d = 4, and Γ(z + 1) = z·Γ(z) gives
    /// C(d + 2) = C(d)·d/(d − 1).
    fn normalization_by_recurrence(dim: u32) -> f64 {
        let (mut d, mut c) = if dim % 2 == 1 {
            (3, 0.5)
        } else {
            (4, 2.0 / PI)
        };
        while d < dim {
            c *= f64::from(d) / f64::from(d - 1);
            d += 2;
        }
        c
    }

    fn relative_error(actual: f64, expected: f64) -> f64 {
        ((actual - expected) / expected).abs()
    }

    /// The index of the centroid nearest `x`, the upper one on a tie.
    ///
    /// x is at least as near c as a lower centroid b exactly when
    /// 2x ≥ b + c. Both sides are exact in f64, unlike |x − c|, which rounds
    /// to the same value for both signs of c once x is tiny.
    fn nearest_centroid(codebook: &LloydMaxCodebook, x: f32) -> u8 {
        let centroids = codebook.centroids();
        let mut nearest = 0;
        for (index, &centroid) in centroids.iter().enumerate().skip(1) {
            if 2.0 * f64::from(x) >= f64::from(centroids[nearest]) + f64::from(centroid) {
                nearest = index;
            }
        }
        nearest as u8
    }

    #[test]
    fn committed_codebook_is_sorted_and_symmetric() {
        let codebook = committed();
        assert_eq!((codebook.dim(), codebook.bits()), (512, 2));
        let c = codebook.centroids();
        assert_eq!(c, [-0.06669391, -0.02000666, 0.02000666, 0.06669391]);
        assert!(c.is_sorted_by(|lower, upper| lower < upper));
        assert_eq!((c[0], c[1]), (-c[3], -c[2]));
        // The midpoint of c[2] and c[3] lies exactly halfway between two
        // f32 values, so the lower threshold's magnitude is the upper's
        // predecessor; see LloydMaxCodebook.
        let t = codebook.thresholds();
        assert_eq!(t, [-0.043350283, 0.0, 0.043350287]);
        assert_eq!(t[0], -t[2].next_down());
        assert_eq!(
            midpoint(f64::from(t[2].next_down()), f64::from(t[2])),
            midpoint(f64::from(c[2]), f64::from(c[3]))
        );
    }

    #[test]
    fn committed_returns_none_for_other_parameters() {
        assert_eq!(LloydMaxCodebook::committed(512, 1), None);
        assert_eq!(LloydMaxCodebook::committed(512, 3), None);
        assert_eq!(LloydMaxCodebook::committed(256, 2), None);
    }

    /// The Lloyd centroid condition, checked directly rather than through the
    /// solver's loop: the mean of each committed cell under f is the
    /// committed centroid, to within 1 f32 ulp. Observed: 0.47 ulp (inner)
    /// and 0.10 ulp (outer), from the f32 rounding of the centroids and of
    /// the threshold between them.
    #[test]
    fn committed_centroids_are_their_cells_conditional_means() {
        let codebook = committed();
        let density = CoordinateDensity::new(512);
        let threshold = libm::asin(f64::from(codebook.thresholds()[2]));
        for (index, (lo, hi)) in [(2, (0.0, threshold)), (3, (threshold, density.theta_max))] {
            let mean = density.integrate(lo, hi, |x| x) / density.integrate(lo, hi, |_| 1.0);
            let centroid = codebook.decode(index);
            let error_ulps = (mean - f64::from(centroid)).abs() / ulp(centroid);
            assert!(error_ulps <= 1.0, "cell {index}: {error_ulps} ulp");
        }
    }

    /// The Lloyd midpoint condition, with the rounding the type documents:
    /// each threshold is the exact midpoint rounded up to f32, so it is
    /// less than 1 ulp above the midpoint.
    #[test]
    fn thresholds_are_the_smallest_f32_at_or_above_each_midpoint() {
        let codebooks = [1, 2, 3, 4]
            .map(|bits| LloydMaxSolution::solve(512, bits).to_codebook())
            .into_iter()
            .chain([committed()]);
        for codebook in codebooks {
            for (pair, &threshold) in codebook.centroids().windows(2).zip(codebook.thresholds()) {
                let midpoint = midpoint(f64::from(pair[0]), f64::from(pair[1]));
                assert!(
                    f64::from(threshold) >= midpoint && f64::from(threshold.next_down()) < midpoint,
                    "bits {}: threshold {threshold:?} for midpoint {midpoint}",
                    codebook.bits()
                );
            }
        }
    }

    /// The f64 solution rounds to the committed centroids, and sits well
    /// inside each f32 rounding interval: 0.07 ulp (inner) and 0.18 ulp
    /// (outer) from the committed value. The solver's 10⁻¹³ relative error is
    /// about 10⁻⁶ ulp, nowhere near flipping a rounding.
    #[test]
    fn solver_reproduces_the_committed_codebook() {
        let solution = solution_512_2();
        assert_eq!(solution.to_codebook(), committed());
        for (&solved, &committed) in solution.centroids().iter().zip(committed().centroids()) {
            let offset_ulps = (solved - f64::from(committed)).abs() / ulp(committed);
            assert!(
                offset_ulps <= 0.25,
                "{solved} vs {committed}: {offset_ulps} ulp"
            );
        }
    }

    /// Reference: an independent mpmath implementation at 40 digits
    /// (adaptive quadrature in x, Lloyd iterated to 10⁻³⁵), posted on the PR
    /// for #164. Observed relative errors: centroids 1.0e-13 (inner) and
    /// 6.2e-14 (outer), threshold 7.0e-14; the distortion and the b = 1
    /// centroid round to the reference's f64, and the b = 1 distortion is off
    /// by 4.6e-16.
    #[test]
    fn solver_matches_a_high_precision_reference() {
        let solution = solution_512_2();
        let [_, _, inner, outer] = solution.centroids() else {
            panic!("expected 4 centroids");
        };
        assert!(relative_error(*inner, 0.02000666024359961) <= 1e-12);
        assert!(relative_error(*outer, 0.06669390810809411) <= 1e-12);
        assert!(relative_error(solution.thresholds()[2], 0.04335028417584686) <= 1e-12);
        assert_eq!(solution.thresholds()[1], 0.0);
        assert!(relative_error(solution.distortion(), 0.1171107148530206) <= 1e-12);

        let one_bit = LloydMaxSolution::solve(512, 1);
        assert!(relative_error(one_bit.centroids()[1], 0.0352790708647002) <= 1e-12);
        assert!(relative_error(one_bit.distortion(), 0.36275822536881225) <= 1e-12);
    }

    /// N(0, 1) Lloyd-Max levels scaled by 1/√512: ±0.020010240 and
    /// ±0.066751658. f has compact support and lighter tails than the
    /// Gaussian, so both committed levels sit slightly inside them: by
    /// 0.018% (inner) and 0.087% (outer).
    #[test]
    fn codebook_is_close_to_the_scaled_gaussian_quantizer() {
        let sigma = 1.0 / 512f64.sqrt();
        let codebook = committed();
        for (&level, &centroid) in GAUSSIAN_POSITIVE_LEVELS[1]
            .iter()
            .zip(&codebook.centroids()[2..])
        {
            let relative_offset = f64::from(centroid) / (level * sigma) - 1.0;
            assert!(
                (-2e-3..0.0).contains(&relative_offset),
                "{centroid} vs {}: {relative_offset}",
                level * sigma
            );
        }
    }

    /// Theorem 1: 4⁻ᵇ ≤ D_mse ≤ (√3·π/2)·4⁻ᵇ. Observed at d = 512: 0.3628,
    /// 0.1171, 0.03440 and 0.009454 for b = 1..4, against the paper's 0.36,
    /// 0.117, 0.03 and 0.009.
    #[test]
    fn distortion_is_within_theorem_1_bounds() {
        for bits in 1..=4 {
            let distortion = LloydMaxSolution::solve(512, bits).distortion();
            let lower = 4f64.powi(-i32::from(bits));
            let upper = 3f64.sqrt() * PI / 2.0 * lower;
            assert!(
                lower <= distortion && distortion <= upper,
                "bits {bits}: {distortion} outside [{lower}, {upper}]"
            );
        }
        assert!((solution_512_2().distortion() - 0.117).abs() < 5e-4);
    }

    /// Mean ‖x − Q(x)‖² over `vectors` uniformly random unit vectors in
    /// ℝ^512, Q being the committed codebook applied to every coordinate,
    /// with its standard error.
    fn empirical_distortion(vectors: usize) -> (f64, f64) {
        let codebook = committed();
        let mut normals = StandardNormalStream::new(0x6c6c_6f79_645f_6d61, 0);
        let mut vector = [0.0; 512];
        let (mut sum, mut sum_of_squares) = (0.0, 0.0);
        for _ in 0..vectors {
            normals.fill(&mut vector);
            let norm = vector.iter().map(|x| x * x).sum::<f64>().sqrt();
            let error: f64 = vector
                .iter()
                .map(|&x| {
                    let x = x / norm;
                    let quantized = f64::from(codebook.decode(codebook.encode(x as f32)));
                    (x - quantized) * (x - quantized)
                })
                .sum();
            sum += error;
            sum_of_squares += error * error;
        }
        let n = vectors as f64;
        let mean = sum / n;
        let variance = (sum_of_squares - n * mean * mean) / (n - 1.0);
        (mean, (variance / n).sqrt())
    }

    fn assert_empirical_distortion_matches(vectors: usize) {
        let predicted = solution_512_2().distortion();
        let (mean, standard_error) = empirical_distortion(vectors);
        let z = (mean - predicted) / standard_error;
        assert!(
            z.abs() < Z_TOLERANCE,
            "mean {mean} ± {standard_error} vs predicted {predicted}: z = {z}"
        );
        assert!(mean < 3f64.sqrt() * PI / 2.0 / 16.0, "mean {mean}");
    }

    /// Observed: mean 0.117079 ± 0.000079 against 0.117111 (z = −0.40), in
    /// 2 s under the dev profile.
    #[test]
    fn empirical_distortion_matches_the_prediction() {
        assert_empirical_distortion_matches(10_000);
    }

    /// Observed: mean 0.1171102 ± 0.0000078 (z = −0.06).
    #[test]
    #[ignore = "11 s under --release: cargo test --release --lib codebook -- --ignored"]
    fn empirical_distortion_matches_the_prediction_over_a_million_vectors() {
        assert_empirical_distortion_matches(1_000_000);
    }

    #[test]
    fn encode_returns_the_nearest_centroid() {
        let codebook = committed();
        let sigma = 1.0 / 512f64.sqrt();
        let mut normals = StandardNormalStream::new(0x6e65_6172_6573_7421, 0);
        let random = (0..10_000).map(|_| (normals.next().unwrap() * sigma) as f32);
        // Each threshold and its neighbors, where rounding could misplace a
        // boundary, and each centroid.
        let boundaries = codebook
            .thresholds()
            .iter()
            .flat_map(|&t| [t.next_down(), t, t.next_up()])
            .chain(codebook.centroids().iter().copied());
        for x in random.chain(boundaries) {
            let index = codebook.encode(x);
            assert_eq!(index, nearest_centroid(&codebook, x), "x = {x:?}");
            if x != 0.0 {
                assert_eq!(codebook.encode(-x), 3 - index, "x = {x:?}");
            }
        }
    }

    #[test]
    fn encode_maps_out_of_range_input_to_the_outer_cells() {
        let codebook = committed();
        for x in [-1.0, -0.5, f32::MIN, f32::NEG_INFINITY] {
            assert_eq!(codebook.encode(x), 0, "x = {x}");
        }
        for x in [1.0, 0.5, f32::MAX, f32::INFINITY] {
            assert_eq!(codebook.encode(x), 3, "x = {x}");
        }
        // Zero is a threshold, and ties go to the upper cell.
        assert_eq!(codebook.encode(0.0), 2);
        assert_eq!(codebook.encode(-0.0), 2);
        assert_eq!(codebook.encode(f32::NAN), 0);
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn decode_rejects_an_index_beyond_the_codebook() {
        committed().decode(4);
    }

    /// Captured on x86_64. Bit-identical output on aarch64 CI is the
    /// cross-target check.
    #[test]
    fn solver_output_is_deterministic_and_pinned() {
        let bits = |solution: &LloydMaxSolution| {
            let mut bits: Vec<u64> = solution.centroids().iter().map(|c| c.to_bits()).collect();
            bits.push(solution.coordinate_mse().to_bits());
            bits
        };
        let solution = solution_512_2();
        assert_eq!(bits(&LloydMaxSolution::solve(512, 2)), bits(solution));
        assert_eq!(
            bits(solution)[2..],
            [
                0x3f94_7ca0_3dcb_3714,
                0x3fb1_12da_1a2a_ad94,
                0x3f2d_faf7_c24e_0e40
            ]
        );
        assert_eq!(solution.iterations(), 44);
    }

    /// At d = 3, f is uniform on [−1, 1], so the Lloyd-Max quantizer is the
    /// uniform one: centroids (2k + 1)/K − 1 for K = 2^b levels, and squared
    /// error (2/K)²/12 per coordinate. Lloyd's algorithm converges slowly on
    /// a uniform density; the largest observed centroid error is 1.4e-12, at
    /// b = 4.
    #[test]
    fn solver_finds_the_uniform_quantizer_at_d_3() {
        for bits in 1..=4 {
            let solution = LloydMaxSolution::solve(3, bits);
            let levels = 1u32 << bits;
            for (k, &centroid) in solution.centroids().iter().enumerate() {
                let expected = f64::from(2 * k as u32 + 1) / f64::from(levels) - 1.0;
                assert!(
                    (centroid - expected).abs() <= 1e-11,
                    "bits {bits}: centroid {k} is {centroid}"
                );
            }
            let expected_mse = (2.0 / f64::from(levels)).powi(2) / 12.0;
            assert!(relative_error(solution.coordinate_mse(), expected_mse) <= 1e-12);
        }
    }

    /// At b = 1 the cells are the half-lines, so c = E[X | X > 0] =
    /// 2C/(d − 1) and the coordinate MSE is E[X²] − c² = 1/d − c². Observed
    /// relative errors: at most 2.0e-15 (c) and 6.1e-15 (MSE) for d ≤ 512;
    /// 6.5e-14 and 2.3e-13 at d = 2²⁰, where the recurrence for C has run
    /// 5·10⁵ steps.
    #[test]
    fn one_bit_centroid_matches_its_closed_form() {
        for dim in [3, 4, 5, 64, 512, 1 << 20] {
            let solution = LloydMaxSolution::solve(dim, 1);
            let d = f64::from(dim);
            let c = 2.0 * normalization_by_recurrence(dim) / (d - 1.0);
            assert_eq!(solution.centroids()[0], -solution.centroids()[1]);
            assert!(
                relative_error(solution.centroids()[1], c) <= 1e-12,
                "dim {dim}"
            );
            assert!(
                relative_error(solution.coordinate_mse(), 1.0 / d - c * c) <= 1e-12,
                "dim {dim}"
            );
        }
    }

    /// As d grows, f approaches N(0, 1/d) and √d·c approaches the Gaussian
    /// Lloyd-Max levels. At b = 2 the gap shrinks like 1/d: −1.8e-4 (inner)
    /// and −8.7e-4 (outer) relative at d = 512, −8.7e-8 and −4.2e-7 at
    /// d = 2²⁰. The largest gap at d = 2²⁰ over b = 1..4 is 2.0e-6 (b = 4,
    /// outermost).
    #[test]
    fn levels_approach_the_gaussian_quantizer_at_large_d() {
        let dim = 1 << 20;
        for bits in 1..=4 {
            let solution = LloydMaxSolution::solve(dim, bits);
            let positive = &solution.centroids()[1 << (bits - 1)..];
            for (&centroid, &level) in positive
                .iter()
                .zip(GAUSSIAN_POSITIVE_LEVELS[usize::from(bits) - 1])
            {
                let scaled = centroid * f64::from(dim).sqrt();
                assert!(
                    relative_error(scaled, level) <= 1e-5,
                    "bits {bits}: {scaled} vs {level}"
                );
            }
        }
    }

    /// Observed: C within 2.2e-15 of the recurrence for d ≤ 512 and 6.5e-14
    /// at d = 2²⁰, where the recurrence has run 5·10⁵ steps; variance within
    /// 5e-16 of 1/d.
    #[test]
    fn density_has_the_closed_form_normalization_and_variance_one_over_d() {
        for dim in [3, 4, 5, 64, 512, 1 << 20] {
            let density = CoordinateDensity::new(dim);
            let normalization = normalization_by_recurrence(dim);
            assert!(
                relative_error(density.normalization, normalization) <= 1e-12,
                "dim {dim}: C = {} vs {normalization}",
                density.normalization
            );
            let variance = 2.0 * density.integrate(0.0, density.theta_max, |x| x * x);
            assert!(
                relative_error(variance, 1.0 / f64::from(dim)) <= 1e-14,
                "dim {dim}: variance {variance}"
            );
        }
    }

    #[test]
    fn bytes_round_trip_in_the_documented_layout() {
        let codebook = committed();
        let bytes = codebook.to_bytes();
        assert_eq!(bytes.len(), 12 + 4 * 4);
        assert_eq!(&bytes[0..4], b"TQCB");
        assert_eq!(bytes[4..8], 512u32.to_le_bytes());
        assert_eq!(bytes[8..12], 2u32.to_le_bytes());
        // Ascending: the most negative centroid comes first.
        assert_eq!(bytes[12..16], (-0.06669391f32).to_le_bytes());
        assert_eq!(bytes[24..28], 0.06669391f32.to_le_bytes());

        // The thresholds aren't stored; parsing derives the same ones.
        let parsed = LloydMaxCodebook::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, codebook);
        assert_eq!(parsed.thresholds(), codebook.thresholds());

        for bits in [1, 3, 4] {
            let codebook = LloydMaxSolution::solve(512, bits).to_codebook();
            let bytes = codebook.to_bytes();
            assert_eq!(bytes.len(), 12 + 4 * (1 << bits));
            assert_eq!(LloydMaxCodebook::from_bytes(&bytes), Ok(codebook));
        }
    }

    #[test]
    fn from_bytes_rejects_malformed_input() {
        let bytes = committed().to_bytes();
        let with_field = |offset: usize, value: &[u8]| {
            let mut edited = bytes.clone();
            edited[offset..offset + value.len()].copy_from_slice(value);
            LloydMaxCodebook::from_bytes(&edited)
        };
        let wrong_value_count = |actual_values| {
            Err(AssetError::InvalidLayout {
                expected_values: 4,
                actual_values,
            })
        };

        assert_eq!(
            LloydMaxCodebook::from_bytes(&bytes[..11]),
            Err(AssetError::InvalidSize {
                minimum: 12,
                actual: 11
            })
        );
        assert_eq!(
            with_field(0, b"TQRT"),
            Err(AssetError::InvalidMagic {
                expected: *b"TQCB",
                actual: *b"TQRT"
            })
        );
        assert_eq!(
            with_field(4, &0u32.to_le_bytes()),
            Err(AssetError::InvalidDim)
        );
        for bits in [0u32, 5, 8, u32::MAX] {
            assert_eq!(
                with_field(8, &bits.to_le_bytes()),
                Err(AssetError::UnsupportedCodebookBits { bits })
            );
        }
        // 2 centroids declared, 4 present.
        assert_eq!(
            with_field(8, &1u32.to_le_bytes()),
            Err(AssetError::InvalidLayout {
                expected_values: 2,
                actual_values: 4
            })
        );
        assert_eq!(
            LloydMaxCodebook::from_bytes(&bytes[..12]),
            wrong_value_count(0)
        );
        assert_eq!(
            LloydMaxCodebook::from_bytes(&bytes[..bytes.len() - 4]),
            wrong_value_count(3)
        );
        assert_eq!(
            LloydMaxCodebook::from_bytes(&bytes[..bytes.len() - 1]),
            wrong_value_count(3)
        );
        assert_eq!(
            LloydMaxCodebook::from_bytes(&[&bytes[..], &[0; 4]].concat()),
            wrong_value_count(5)
        );

        // Centroids that `encode` could not use: out of order, repeated, or
        // not finite.
        let inner = (-0.02000666f32).to_le_bytes();
        for (offset, value) in [
            (12, 0.5f32.to_le_bytes()),
            (12, inner),
            (24, f32::NAN.to_le_bytes()),
            (24, f32::INFINITY.to_le_bytes()),
            (12, f32::NEG_INFINITY.to_le_bytes()),
        ] {
            assert_eq!(
                with_field(offset, &value),
                Err(AssetError::CentroidsNotAscending),
                "centroid at {offset} set to {:?}",
                f32::from_le_bytes(value)
            );
        }
    }

    #[test]
    #[should_panic(expected = "dim must be at least 3, got 2")]
    fn solve_rejects_dim_below_3() {
        LloydMaxSolution::solve(2, 2);
    }

    #[test]
    #[should_panic(expected = "bits must be in 1..=4, got 0")]
    fn solve_rejects_zero_bits() {
        LloydMaxSolution::solve(512, 0);
    }

    #[test]
    #[should_panic(expected = "bits must be in 1..=4, got 5")]
    fn solve_rejects_more_than_4_bits() {
        LloydMaxSolution::solve(512, 5);
    }
}
