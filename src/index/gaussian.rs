//! Seeded standard-normal sampler for offline codec asset generation.
//!
//! This is the one N(0, 1) source shared by the TurboQuant_prod generators
//! ([`Rotation`](super::Rotation) and the QJL matrix in #165). Under the
//! materialization contract (see [`super::codec_config`]) only builders run
//! it; the query side loads the matrices it produced from the release.
//!
//! # Reproducibility guarantee
//!
//! For a given [`GAUSSIAN_GENERATOR_VERSION`], `(seed, stream_id)` determines
//! the output sequence bit for bit on every target with IEEE 754 binary64
//! arithmetic (x86_64 and aarch64 included; x87-only i586 is not):
//!
//! - Uniform words are the ChaCha8 keystream (64-bit block counter, 64-bit
//!   nonce) for the 32-byte key `seed.to_le_bytes()` followed by 24 zero
//!   bytes and the nonce `stream_id`, read as little-endian `u64`s from block
//!   0. The key is laid out here rather than through `seed_from_u64`, so the
//!   stream doesn't depend on `rand_core`'s seed expansion.
//! - Each pair of words `(w0, w1)` becomes two outputs by Box–Muller:
//!   `u = ((w >> 12) + 0.5) · 2⁻⁵²` maps each word exactly into (0, 1), then
//!   `r = sqrt(-2·ln u0)`, `θ = 2π·u1`, giving `(r·cos θ, r·sin θ)`. `ln` and
//!   `sincos` come from the pure-Rust `libm` crate, which has no
//!   architecture-specific code for them, and `sqrt` is correctly rounded
//!   everywhere. Rust never fuses or reorders float operations, so the
//!   platform C library, FMA support and CPU don't affect the result.
//! - Output `i` depends only on keystream words `2·⌊i/2⌋` and `2·⌊i/2⌋ + 1`,
//!   so a fill of `n` values is the length-`n` prefix of any longer fill, and
//!   filling through a [`StandardNormalStream`] in chunks of any sizes gives
//!   the same values as one call.
//!
//! Box–Muller rather than the Marsaglia polar method because it consumes a
//! fixed number of words per output, so the position of every value in the
//! keystream is known without replaying rejection loops. `rand_distr`'s
//! Ziggurat sampler was not used because its output may change across
//! `rand_distr` releases.
//!
//! Any change to the output, whether from this code or from a `rand_chacha`
//! or `libm` upgrade, must bump [`GAUSSIAN_GENERATOR_VERSION`]. The golden
//! tests below fail on such a change. The version also covers the matrix
//! generators built on this sampler, since a codec records a single
//! `generator_version`: a change to how
//! [`Rotation::generate`](super::Rotation::generate) turns draws into a
//! matrix bumps it too, and the rotation's own golden test catches that.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Version of the sampler's output sequence and of the generators built on
/// it. Bump it whenever any `(seed, stream_id)` would produce different
/// values, or a generator would turn the same seed into a different matrix.
pub const GAUSSIAN_GENERATOR_VERSION: u32 = 1;

/// Fills `out` with the first `out.len()` standard-normal values of the
/// `(seed, stream_id)` sequence.
pub fn fill_standard_normal(seed: u64, stream_id: u64, out: &mut [f64]) {
    StandardNormalStream::new(seed, stream_id).fill(out);
}

/// The `(seed, stream_id)` standard-normal sequence, consumed in order.
///
/// Use it to fill a matrix in pieces (for example row by row); the values
/// are the same as one [`fill_standard_normal`] call over the whole buffer.
/// As an [`Iterator`] it never ends.
#[derive(Debug, Clone)]
pub struct StandardNormalStream {
    rng: ChaCha8Rng,
    /// The `sin` half of the last Box–Muller pair, if not yet returned.
    pending: Option<f64>,
}

impl StandardNormalStream {
    pub fn new(seed: u64, stream_id: u64) -> Self {
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&seed.to_le_bytes());
        let mut rng = ChaCha8Rng::from_seed(key);
        rng.set_stream(stream_id);
        Self { rng, pending: None }
    }

    /// Writes the next `out.len()` values of the sequence into `out`.
    pub fn fill(&mut self, out: &mut [f64]) {
        for slot in out {
            *slot = self.next_value();
        }
    }

    fn next_value(&mut self) -> f64 {
        if let Some(value) = self.pending.take() {
            return value;
        }
        let u0 = unit_open(self.rng.next_u64());
        let u1 = unit_open(self.rng.next_u64());
        let radius = (-2.0 * libm::log(u0)).sqrt();
        let (sin, cos) = libm::sincos(std::f64::consts::TAU * u1);
        self.pending = Some(radius * sin);
        radius * cos
    }
}

impl Iterator for StandardNormalStream {
    type Item = f64;

    fn next(&mut self) -> Option<f64> {
        Some(self.next_value())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (usize::MAX, None)
    }
}

/// Maps a uniform `u64` exactly into the open interval (0, 1): the top 52
/// bits plus one half, scaled by 2⁻⁵². Every step is exact in f64, so the
/// smallest value is 2⁻⁵³ and `ln` never sees zero.
fn unit_open(word: u64) -> f64 {
    ((word >> 12) as f64 + 0.5) * (1.0 / (1u64 << 52) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::release_manifest::sha256_hex;

    // Captured on x86_64 when GAUSSIAN_GENERATOR_VERSION was 1, and matched
    // bit for bit by an independent Python ChaCha8 + Box–Muller written from
    // the module docs. CI runs these on aarch64.
    const GOLDEN_SEED_0_STREAM_0: [u64; 8] = [
        0xbfd9e4566e2a4b69, // -4.04561622222152695e-1
        0xbfdc124926ab051e, // -4.38616073381199345e-1
        0x3ff434cbfb000a63, // +1.26288984343468580e0
        0x3ff2684b1d1dc315, // +1.15046225904024380e0
        0x3ff57af21e1ad388, // +1.34251605758365322e0
        0x3feecab90b93d0e0, // +9.62246439563866574e-1
        0xbfcdc340f7e01d3a, // -2.32521172554905509e-1
        0x4009483fae83c4d1, // +3.16027771319986828e0
    ];
    const GOLDEN_SEED_5EED_STREAM_1: [u64; 8] = [
        0x40020943ac4d4849, // +2.25452360735747392e0
        0x3ff559998e705c6f, // +1.33437495842170128e0
        0x3ff7536d2249ac5f, // +1.45786775010744640e0
        0xbfd4eb5bfdf49fcf, // -3.26865194323997466e-1
        0x3fcfa689763c4fe2, // +2.47269804699157325e-1
        0x3ffa1ddd91d451c3, // +1.63229138340567270e0
        0x3fa0c48dd8183612, // +3.27495886123011642e-2
        0xbffa305429ad023b, // -1.63679901389708848e0
    ];
    /// sha256 of the little-endian bytes of the first 512 × 512 values of
    /// `(0x5EED, 0)`, the size of a d = 512 rotation or QJL matrix.
    const GOLDEN_SEED_5EED_STREAM_0_512X512_SHA256: &str =
        "cf32dc102c50b2cfb5d53b1037b8e5fb9bcd7cf5984405681301847a655c1443";

    fn bits(seed: u64, stream_id: u64, len: usize) -> Vec<u64> {
        let mut out = vec![0.0; len];
        fill_standard_normal(seed, stream_id, &mut out);
        out.into_iter().map(f64::to_bits).collect()
    }

    #[test]
    fn generator_version_matches_the_pinned_goldens() {
        // Bump the version together with the goldens below.
        assert_eq!(GAUSSIAN_GENERATOR_VERSION, 1);
    }

    #[test]
    fn first_values_are_pinned() {
        assert_eq!(bits(0, 0, 8), GOLDEN_SEED_0_STREAM_0);
        assert_eq!(bits(0x5EED, 1, 8), GOLDEN_SEED_5EED_STREAM_1);
    }

    #[test]
    fn matrix_sized_fill_is_pinned() {
        let mut values = vec![0.0; 512 * 512];
        fill_standard_normal(0x5EED, 0, &mut values);
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(sha256_hex(&bytes), GOLDEN_SEED_5EED_STREAM_0_512X512_SHA256);
    }

    #[test]
    fn output_is_independent_of_slice_length_and_chunking() {
        let full = bits(0x5EED, 0, 1001);
        for len in [0, 1, 2, 3, 64, 1000] {
            assert_eq!(bits(0x5EED, 0, len), full[..len], "prefix of length {len}");
        }

        // Chunk sizes of both parities, so chunks start on either half of a
        // Box–Muller pair.
        let mut stream = StandardNormalStream::new(0x5EED, 0);
        let mut chunked = Vec::new();
        for len in [1, 2, 3, 0, 5, 7, 983] {
            let mut chunk = vec![0.0; len];
            stream.fill(&mut chunk);
            chunked.extend(chunk.into_iter().map(f64::to_bits));
        }
        assert_eq!(chunked, full);

        let iterated: Vec<u64> = StandardNormalStream::new(0x5EED, 0)
            .take(1001)
            .map(f64::to_bits)
            .collect();
        assert_eq!(iterated, full);
    }

    #[test]
    fn seeds_and_streams_select_unrelated_sequences() {
        let sequences = [bits(0, 0, 256), bits(0, 1, 256), bits(1, 0, 256)];
        for (a, first) in sequences.iter().enumerate() {
            for second in &sequences[a + 1..] {
                assert!(first.iter().zip(second).all(|(x, y)| x != y));
            }
        }
    }

    /// 2²⁰ draws. Each tolerance is five standard errors of the statistic
    /// under N(0, 1); the comments give the values observed on the pinned
    /// sequence.
    #[test]
    fn moments_and_tails_match_the_standard_normal() {
        const N: usize = 1 << 20;
        let n = N as f64;
        let mut z = vec![0.0; N];
        fill_standard_normal(0x0161, 0, &mut z);

        // Observed -3.44e-4; standard error 1/√N.
        let mean = z.iter().sum::<f64>() / n;
        assert!(mean.abs() < 5.0 / n.sqrt(), "mean {mean}");

        // Observed 1.00103; standard error √(2/N).
        let variance = z.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        assert!(
            (variance - 1.0).abs() < 5.0 * (2.0 / n).sqrt(),
            "variance {variance}"
        );

        // Observed 2848 draws beyond 3 and 72 beyond 4, against expected
        // counts N·p with P(|Z| > 3) = 2.6998e-3 and P(|Z| > 4) = 6.3342e-5;
        // binomial standard error √(N·p·(1−p)).
        for (threshold, p) in [(3.0, 2.699_796e-3), (4.0, 6.334_248e-5)] {
            let count = z.iter().filter(|v| v.abs() > threshold).count() as f64;
            let expected = n * p;
            let tolerance = 5.0 * (n * p * (1.0 - p)).sqrt();
            assert!(
                (count - expected).abs() < tolerance,
                "|z| > {threshold}: {count} draws, expected {expected:.1} ± {tolerance:.1}"
            );
        }

        // Observed 9.26e-4. Catches a pairing bug, such as both halves of a
        // Box–Muller pair sharing one angle; standard error 1/√N.
        let lag1 = z.windows(2).map(|w| w[0] * w[1]).sum::<f64>() / (n - 1.0);
        assert!(lag1.abs() < 5.0 / n.sqrt(), "lag-1 correlation {lag1}");

        // Structural bound: u ≥ 2⁻⁵³, so |z| ≤ √(−2·ln 2⁻⁵³) ≈ 8.57.
        let bound = (106.0 * std::f64::consts::LN_2).sqrt();
        assert!(z.iter().all(|v| v.abs() <= bound));
    }

    #[test]
    fn unit_open_maps_extreme_words_inside_the_open_interval() {
        assert_eq!(unit_open(0), 2f64.powi(-53));
        assert_eq!(unit_open(u64::MAX), 1.0 - 2f64.powi(-53));
    }
}
