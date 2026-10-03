//! The TurboQuant_prod codec, [`TurboCodecId::TurboQuantProdV1`].
//!
//! # Encoding
//!
//! For a document vector x ∈ ℝ^d, [`TurboQuantProdV1::encode`] computes
//!
//! 1. `norm = ‖x‖` and the unit vector `u = x/‖x‖` (see "Norm" below);
//! 2. `x′ = Π·u` with the seeded [`Rotation`];
//! 3. `idx_i = Q(x′_i)` with the committed 2-bit [`LloydMaxCodebook`];
//! 4. the residual `r′ = x′ − c[idx]` and `γ = ‖r′‖`;
//! 5. the signs `z = sign(S·r′)` with the m×d [`QjlMatrix`].
//!
//! A query y scores that code as
//!
//! ```text
//! ‖x‖ · (⟨y′, c[idx]⟩ + γ·√(π/2)/m·⟨z, S·y′⟩),   y′ = Π·y.
//! ```
//!
//! Π is orthogonal, so `⟨y′, c[idx]⟩ + ⟨y′, r′⟩ = ⟨y′, x′⟩ = ⟨y, u⟩`, and the
//! QJL term estimates `⟨y′, r′⟩` without bias over the draw of S (see
//! [`super::qjl`]). The score is therefore an unbiased estimate of ⟨y, x⟩,
//! up to f32 rounding.
//!
//! # Residual space
//!
//! The paper takes the residual in the original space, `r = u − Πᵀ·c[idx]`,
//! and stores `sign(S·r)`. This codec sketches `r′ = Π·r` in the rotated
//! space instead, which saves an inverse rotation per document. The
//! estimator has the same distribution. `‖r′‖ = ‖r‖` because Π is
//! orthogonal. `S·r′ = (S·Π)·r` and `⟨z, S·y′⟩ = ⟨z, (S·Π)·y⟩`, and for any
//! fixed orthogonal Π the rows of S·Π are N(0, I) whenever those of S are.
//! S comes from its own sampler stream, independent of Π, so this is the
//! paper's estimator with the Gaussian matrix S·Π in place of S. A test
//! checks γ against the original-space ‖r‖; #167 checks bias and variance
//! against the theory.
//!
//! # Norm
//!
//! The codebook is optimized for the coordinates of a rotated unit vector,
//! but not every embedding source normalizes: `FixedEmbeddingGenerator` and
//! the test fixtures produce non-unit vectors, and v3 never checked. The
//! codec therefore follows [`NormalizeAndStore`]: it encodes `u = x/‖x‖`
//! and stores ‖x‖ as an f32 next to the code. #166 decides where the record
//! keeps it; #165 proposed the 4 reserved bytes. The score is linear in
//! ‖x‖, so this costs one multiply per record and keeps the estimate
//! unbiased for any input norm, up to the f32 rounding of ‖x‖. Queries are
//! not normalized: the score is linear in y too.
//!
//! ‖x‖ and `u` are computed in f64, then rounded to f32. A zero vector
//! encodes with norm 0, so it scores 0 against every query whatever its
//! other fields hold. Non-finite input is rejected, and so is a vector
//! whose norm overflows f32.
//!
//! [`NormalizeAndStore`]: super::NormPolicy::NormalizeAndStore
//!
//! # Codec definition
//!
//! A [`TurboQuantConfig`] names the rotation and the QJL matrix through
//! their seeds and `generator_version`. It doesn't name the codebook or
//! the arithmetic above: `TurboQuantProdV1` means the committed d = 512,
//! b = 2 codebook, f32 rotation and projection sums in index order, f64 norms,
//! and the bit layout of [`EncodedTurboProd`]. Encoded bytes are release
//! bytes (#166), so changing any of these for an unchanged config needs a new
//! [`TurboCodecId`]. A golden digest of encoded bytes fails on such a change,
//! and [`TurboQuantProdV1::from_assets`] rejects any other codebook.
//!
//! # Scoring
//!
//! [`TurboQuantProdV1::prepare_query`] does all the per-query work: y′ = Π·y,
//! the table `lut[i][k] = y′_i·c_k`, `p = S·y′`, and the scale √(π/2)/m.
//! The [`PreparedTurboProdQuery`] holds no matrix. Each record costs d table
//! lookups and m signed adds, both sequential f32 sums, and no allocation.

use std::fmt;

use super::codec_config::{TurboCodecId, TurboQuantConfig, TurboQuantConfigError};
use super::qjl::{PreparedQjlQuery, QjlMatrix};
use super::{LloydMaxCodebook, Rotation};

/// The index width this codec packs: 4 coordinates per byte, coordinate i
/// at bits `2·(i mod 4)` of byte `i / 4`, as in the legacy codec.
const IDX_BITS: u8 = 2;
const IDX_MASK: u8 = (1 << IDX_BITS) - 1;
const CENTROIDS: usize = 1 << IDX_BITS;
const COORDINATES_PER_BYTE: usize = 8 / IDX_BITS as usize;

/// A TurboQuant_prod codec: a validated config and the three assets it names.
#[derive(Debug, Clone, PartialEq)]
pub struct TurboQuantProdV1 {
    config: TurboQuantConfig,
    rotation: Rotation,
    codebook: LloydMaxCodebook,
    qjl: QjlMatrix,
}

impl TurboQuantProdV1 {
    /// Generates the assets `config` names: the builder path. Under the
    /// materialization contract (see [`super::codec_config`]) the query side
    /// loads them with [`from_assets`](Self::from_assets) instead.
    pub fn generate(config: TurboQuantConfig) -> Result<Self, TurboProdError> {
        check_config(&config)?;
        Ok(Self {
            rotation: Rotation::generate(config.dim, config.mse_seed),
            codebook: committed_codebook(&config)?,
            qjl: QjlMatrix::generate(config.dim, config.qjl_dim, config.qjl_seed),
            config,
        })
    }

    /// Assembles the codec from stored assets: the loader path (#166). The
    /// rotation and the QJL matrix must be the ones `config` names, by
    /// shape, seed and `generator_version`, so a QJL matrix with m and d
    /// swapped is rejected here. The codebook must be the committed one (see
    /// "Codec definition" in the module docs).
    pub fn from_assets(
        config: TurboQuantConfig,
        rotation: Rotation,
        codebook: LloydMaxCodebook,
        qjl: QjlMatrix,
    ) -> Result<Self, TurboProdError> {
        check_config(&config)?;
        let checks = [
            ("rotation", "dim", config.dim.into(), rotation.dim().into()),
            ("rotation", "seed", config.mse_seed, rotation.seed()),
            (
                "rotation",
                "generator_version",
                config.generator_version.into(),
                rotation.generator_version().into(),
            ),
            ("codebook", "dim", config.dim.into(), codebook.dim().into()),
            (
                "codebook",
                "bits",
                config.mse_bits.into(),
                codebook.bits().into(),
            ),
            ("qjl", "dim", config.dim.into(), qjl.dim().into()),
            (
                "qjl",
                "qjl_dim",
                config.qjl_dim.into(),
                qjl.qjl_dim().into(),
            ),
            ("qjl", "seed", config.qjl_seed, qjl.seed()),
            (
                "qjl",
                "generator_version",
                config.generator_version.into(),
                qjl.generator_version().into(),
            ),
        ];
        for (asset, field, expected, actual) in checks {
            if expected != actual {
                return Err(TurboProdError::AssetMismatch {
                    asset,
                    field,
                    expected,
                    actual,
                });
            }
        }
        if codebook != committed_codebook(&config)? {
            return Err(TurboProdError::NotTheCommittedCodebook);
        }

        Ok(Self {
            config,
            rotation,
            codebook,
            qjl,
        })
    }

    /// Encodes a document vector; see the module docs.
    pub fn encode(&self, vector: &[f32]) -> Result<EncodedTurboProd, TurboProdError> {
        self.check_len(vector.len())?;
        if !vector.iter().all(|value| value.is_finite()) {
            return Err(TurboProdError::NonFiniteInput);
        }
        let norm = vector
            .iter()
            .map(|&value| f64::from(value) * f64::from(value))
            .sum::<f64>()
            .sqrt();
        if !(norm as f32).is_finite() {
            return Err(TurboProdError::NormOverflow);
        }

        let unit: Vec<f32> = if norm == 0.0 {
            vec![0.0; vector.len()]
        } else {
            vector
                .iter()
                .map(|&value| (f64::from(value) / norm) as f32)
                .collect()
        };
        let mut residual = vec![0.0; unit.len()];
        self.rotation
            .rotate(&unit, &mut residual)
            .expect("the vector's length was checked against dim");

        let mut idx = vec![0; self.idx_len()];
        for (i, value) in residual.iter_mut().enumerate() {
            let index = self.codebook.encode(*value);
            idx[i / COORDINATES_PER_BYTE] |=
                index << (usize::from(IDX_BITS) * (i % COORDINATES_PER_BYTE));
            *value -= self.codebook.decode(index);
        }
        let gamma = residual
            .iter()
            .map(|&value| f64::from(value) * f64::from(value))
            .sum::<f64>()
            .sqrt();

        let mut signs = vec![0; self.qjl.signs_len()];
        self.qjl
            .encode(&residual, &mut signs)
            .expect("the residual has length dim");

        Ok(EncodedTurboProd {
            idx,
            signs,
            gamma: gamma as f32,
            norm: norm as f32,
        })
    }

    /// Computes everything [`PreparedTurboProdQuery::score`] needs from
    /// `query`. The query is not normalized.
    pub fn prepare_query(&self, query: &[f32]) -> Result<PreparedTurboProdQuery, TurboProdError> {
        self.check_len(query.len())?;
        let mut rotated = vec![0.0; query.len()];
        self.rotation
            .rotate(query, &mut rotated)
            .expect("the query's length was checked against dim");

        let centroids = self.codebook.centroids();
        let lut = rotated
            .iter()
            .map(|&value| std::array::from_fn(|k| value * centroids[k]))
            .collect();
        let qjl = self
            .qjl
            .prepare_query(&rotated)
            .expect("the rotated query has length dim");
        Ok(PreparedTurboProdQuery { lut, qjl })
    }

    pub fn config(&self) -> &TurboQuantConfig {
        &self.config
    }

    pub fn rotation(&self) -> &Rotation {
        &self.rotation
    }

    pub fn codebook(&self) -> &LloydMaxCodebook {
        &self.codebook
    }

    pub fn qjl(&self) -> &QjlMatrix {
        &self.qjl
    }

    /// Bytes per index code, `d/4`.
    pub fn idx_len(&self) -> usize {
        (self.config.dim as usize).div_ceil(COORDINATES_PER_BYTE)
    }

    /// Bytes per sign code, `⌈m/8⌉`.
    pub fn signs_len(&self) -> usize {
        self.qjl.signs_len()
    }

    fn check_len(&self, len: usize) -> Result<(), TurboProdError> {
        let expected = self.config.dim as usize;
        if len != expected {
            return Err(TurboProdError::DimensionMismatch {
                expected,
                actual: len,
            });
        }
        Ok(())
    }
}

fn check_config(config: &TurboQuantConfig) -> Result<(), TurboProdError> {
    config.validate()?;
    if config.codec_id != TurboCodecId::TurboQuantProdV1 {
        return Err(TurboProdError::NotTurboQuantProd {
            codec_id: config.codec_id,
        });
    }
    if config.mse_bits != IDX_BITS {
        return Err(TurboProdError::UnsupportedMseBits {
            mse_bits: config.mse_bits,
        });
    }
    Ok(())
}

fn committed_codebook(config: &TurboQuantConfig) -> Result<LloydMaxCodebook, TurboProdError> {
    LloydMaxCodebook::committed(config.dim, config.mse_bits).ok_or(
        TurboProdError::NoCommittedCodebook {
            dim: config.dim,
            mse_bits: config.mse_bits,
        },
    )
}

/// One encoded document.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EncodedTurboProd {
    /// The codebook index of each rotated coordinate, 2 bits each,
    /// LSB-first: coordinate i is bits `2·(i mod 4)` of byte `i / 4`.
    pub idx: Vec<u8>,
    /// `sign(S·r′)`, bit j LSB-first in byte `j / 8`, set for ≥ 0.
    pub signs: Vec<u8>,
    /// γ = ‖r′‖.
    pub gamma: f32,
    /// ‖x‖.
    pub norm: f32,
}

impl EncodedTurboProd {
    pub fn code(&self) -> TurboProdCode<'_> {
        TurboProdCode {
            idx: &self.idx,
            signs: &self.signs,
            gamma: self.gamma,
            norm: self.norm,
        }
    }
}

/// A borrowed code, what [`PreparedTurboProdQuery::score`] reads, so a scan
/// can score records in place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurboProdCode<'a> {
    pub idx: &'a [u8],
    pub signs: &'a [u8],
    pub gamma: f32,
    pub norm: f32,
}

/// A query prepared for scoring TurboQuant_prod codes; see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedTurboProdQuery {
    /// `lut[i][k] = y′_i·c_k`.
    lut: Vec<[f32; CENTROIDS]>,
    qjl: PreparedQjlQuery,
}

impl PreparedTurboProdQuery {
    /// The estimate of ⟨y, x⟩. Allocation-free.
    ///
    /// # Panics
    ///
    /// As [`score_breakdown`](Self::score_breakdown).
    pub fn score(&self, code: TurboProdCode<'_>) -> f32 {
        self.score_breakdown(code).total()
    }

    /// The terms of [`score`](Self::score). Allocation-free.
    ///
    /// # Panics
    ///
    /// If `code.idx` isn't `d/4` bytes or `code.signs` isn't `⌈m/8⌉` bytes
    /// for the codec this query was prepared with.
    pub fn score_breakdown(&self, code: TurboProdCode<'_>) -> TurboProdScoreBreakdown {
        assert_eq!(
            code.idx.len(),
            self.lut.len().div_ceil(COORDINATES_PER_BYTE),
            "index code length doesn't match dim {}",
            self.lut.len()
        );

        let mut mse_term = 0.0;
        for (products, &byte) in self.lut.chunks(COORDINATES_PER_BYTE).zip(code.idx) {
            for (slot, products) in products.iter().enumerate() {
                let index = (byte >> (usize::from(IDX_BITS) * slot)) & IDX_MASK;
                mse_term += products[usize::from(index)];
            }
        }

        TurboProdScoreBreakdown {
            mse_term,
            qjl_term: self.qjl.estimate(code.signs, code.gamma),
            norm: code.norm,
        }
    }
}

/// The terms of a TurboQuant_prod score.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TurboProdScoreBreakdown {
    /// `⟨y′, c[idx]⟩`. Alone, times `norm`, it is the MSE-only estimate,
    /// which is biased.
    pub mse_term: f32,
    /// `γ·√(π/2)/m·⟨z, S·y′⟩`, the unbiased estimate of `⟨y′, r′⟩`.
    pub qjl_term: f32,
    /// ‖x‖.
    pub norm: f32,
}

impl TurboProdScoreBreakdown {
    pub fn total(self) -> f32 {
        self.norm * (self.mse_term + self.qjl_term)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurboProdError {
    Config(TurboQuantConfigError),
    NotTurboQuantProd {
        codec_id: TurboCodecId,
    },
    /// The index packing has 2 bits per coordinate.
    UnsupportedMseBits {
        mse_bits: u8,
    },
    NoCommittedCodebook {
        dim: u32,
        mse_bits: u8,
    },
    AssetMismatch {
        asset: &'static str,
        field: &'static str,
        expected: u64,
        actual: u64,
    },
    /// The codebook has the config's dim and bits but other values.
    NotTheCommittedCodebook,
    DimensionMismatch {
        expected: usize,
        actual: usize,
    },
    NonFiniteInput,
    NormOverflow,
}

impl From<TurboQuantConfigError> for TurboProdError {
    fn from(error: TurboQuantConfigError) -> Self {
        Self::Config(error)
    }
}

impl fmt::Display for TurboProdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(f, "invalid codec config: {error}"),
            Self::NotTurboQuantProd { codec_id } => write!(
                f,
                "codec {codec_id} is not {}",
                TurboCodecId::TurboQuantProdV1
            ),
            Self::UnsupportedMseBits { mse_bits } => write!(
                f,
                "{} packs {IDX_BITS}-bit indices, got mse_bits {mse_bits}",
                TurboCodecId::TurboQuantProdV1
            ),
            Self::NoCommittedCodebook { dim, mse_bits } => write!(
                f,
                "no committed codebook for dim {dim} and mse_bits {mse_bits}"
            ),
            Self::AssetMismatch {
                asset,
                field,
                expected,
                actual,
            } => write!(
                f,
                "{asset} asset {field} mismatch: config expects {expected}, asset has {actual}"
            ),
            Self::NotTheCommittedCodebook => write!(
                f,
                "codebook asset is not the committed codebook {} is defined with",
                TurboCodecId::TurboQuantProdV1
            ),
            Self::DimensionMismatch { expected, actual } => {
                write!(f, "dimension mismatch: expected {expected}, got {actual}")
            }
            Self::NonFiniteInput => write!(f, "vector contains a non-finite value"),
            Self::NormOverflow => write!(f, "vector norm overflows f32"),
        }
    }
}

impl std::error::Error for TurboProdError {}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::index::codec_config::NormPolicy;
    use crate::index::gaussian::GAUSSIAN_GENERATOR_VERSION;
    use crate::index::release_manifest::sha256_hex;
    use crate::index::{fill_standard_normal, LloydMaxSolution, TurboRecord512};

    /// sha256 over `idx ‖ signs ‖ gamma ‖ norm` (f32 little-endian) of
    /// `golden_vectors()` encoded with `codec()`, captured on x86_64.
    const GOLDEN_ENCODE_SHA256: &str =
        "e76be1d67a8358d7c135ba9c6e60854c02da1427668c24c0c8527c7aad37206d";

    fn config() -> TurboQuantConfig {
        TurboQuantConfig {
            codec_id: TurboCodecId::TurboQuantProdV1,
            mse_seed: 163,
            qjl_seed: 165,
            generator_version: GAUSSIAN_GENERATOR_VERSION,
            norm_policy: NormPolicy::NormalizeAndStore,
            ..TurboQuantConfig::legacy_v1()
        }
    }

    fn codec() -> &'static TurboQuantProdV1 {
        static CODEC: OnceLock<TurboQuantProdV1> = OnceLock::new();
        CODEC.get_or_init(|| TurboQuantProdV1::generate(config()).unwrap())
    }

    /// `count` standard-normal vectors of length d.
    fn gaussian_vectors(count: usize, stream_id: u64) -> Vec<Vec<f32>> {
        let mut draws = vec![0.0; count * 512];
        fill_standard_normal(0x165, stream_id, &mut draws);
        draws
            .chunks_exact(512)
            .map(|chunk| chunk.iter().map(|&value| value as f32).collect())
            .collect()
    }

    /// Unnormalized Gaussian vectors (norm about √512), the constant
    /// fixture embedding, and one negative, one-hot vector.
    fn golden_vectors() -> Vec<Vec<f32>> {
        let mut vectors = gaussian_vectors(6, 0);
        vectors.push(vec![0.1; 512]);
        let mut one_hot = vec![0.0; 512];
        one_hot[17] = -3.0;
        vectors.push(one_hot);
        vectors
    }

    fn encoded_bytes(encoded: &EncodedTurboProd) -> Vec<u8> {
        [
            &encoded.idx[..],
            &encoded.signs,
            &encoded.gamma.to_le_bytes(),
            &encoded.norm.to_le_bytes(),
        ]
        .concat()
    }

    #[test]
    fn encode_is_pinned() {
        let bytes: Vec<u8> = golden_vectors()
            .iter()
            .flat_map(|vector| encoded_bytes(&codec().encode(vector).unwrap()))
            .collect();
        assert_eq!(sha256_hex(&bytes), GOLDEN_ENCODE_SHA256);
    }

    #[test]
    fn production_codes_fit_the_512_dim_record() {
        let encoded = codec().encode(&golden_vectors()[0]).unwrap();
        assert_eq!((codec().idx_len(), codec().signs_len()), (128, 64));
        // With the norm in what are reserved bytes today (#166 decides).
        let record = TurboRecord512 {
            doc_id: 0,
            idx: encoded.idx.as_slice().try_into().unwrap(),
            qjl: encoded.signs.as_slice().try_into().unwrap(),
            gamma: encoded.gamma,
            _reserved: encoded.norm.to_le_bytes(),
        };
        assert_eq!(f32::from_le_bytes(record._reserved), encoded.norm);
    }

    #[test]
    fn scaling_the_input_by_a_power_of_two_only_scales_the_norm() {
        let codec = codec();
        let query = &gaussian_vectors(1, 1)[0];
        let prepared = codec.prepare_query(query).unwrap();
        for vector in golden_vectors() {
            let encoded = codec.encode(&vector).unwrap();
            for factor in [0.25f32, 4.0, 1024.0] {
                let scaled: Vec<f32> = vector.iter().map(|value| value * factor).collect();
                let scaled_encoded = codec.encode(&scaled).unwrap();
                // x/‖x‖ is exactly the same vector, so only the norm moves,
                // and by exactly the factor.
                assert_eq!(scaled_encoded.idx, encoded.idx);
                assert_eq!(scaled_encoded.signs, encoded.signs);
                assert_eq!(scaled_encoded.gamma, encoded.gamma);
                assert_eq!(scaled_encoded.norm, encoded.norm * factor);
                assert_eq!(
                    prepared.score(scaled_encoded.code()),
                    prepared.score(encoded.code()) * factor
                );
            }
        }
    }

    #[test]
    fn a_zero_vector_scores_zero() {
        let encoded = codec().encode(&[0.0; 512]).unwrap();
        assert_eq!(encoded.norm, 0.0);
        for query in gaussian_vectors(4, 2) {
            let prepared = codec().prepare_query(&query).unwrap();
            assert_eq!(prepared.score(encoded.code()), 0.0);
        }
    }

    #[test]
    fn score_is_norm_times_the_sum_of_its_terms() {
        let query = &gaussian_vectors(1, 3)[0];
        let prepared = codec().prepare_query(query).unwrap();
        let encoded = codec().encode(&golden_vectors()[6]).unwrap();
        let breakdown = prepared.score_breakdown(encoded.code());
        assert_eq!(breakdown.norm, encoded.norm);
        assert_eq!(
            prepared.score(encoded.code()),
            encoded.norm * (breakdown.mse_term + breakdown.qjl_term)
        );
    }

    #[test]
    fn encode_rejects_non_finite_input_and_norm_overflow() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut vector = vec![0.1; 512];
            vector[300] = bad;
            assert_eq!(codec().encode(&vector), Err(TurboProdError::NonFiniteInput));
        }
        // Every value is finite, but ‖x‖ = √512·f32::MAX isn't.
        assert_eq!(
            codec().encode(&[f32::MAX; 512]),
            Err(TurboProdError::NormOverflow)
        );
    }

    #[test]
    fn encode_and_prepare_reject_the_wrong_length() {
        for len in [511, 513, 1024] {
            let mismatch = TurboProdError::DimensionMismatch {
                expected: 512,
                actual: len,
            };
            assert_eq!(codec().encode(&vec![0.1; len]), Err(mismatch.clone()));
            assert_eq!(codec().prepare_query(&vec![0.1; len]), Err(mismatch));
        }
    }

    #[test]
    fn from_assets_accepts_the_stored_assets_of_its_config() {
        let codec = codec();
        let loaded = TurboQuantProdV1::from_assets(
            config(),
            Rotation::from_bytes(&codec.rotation().to_bytes()).unwrap(),
            LloydMaxCodebook::committed(512, 2).unwrap(),
            QjlMatrix::from_bytes(&codec.qjl().to_bytes()).unwrap(),
        )
        .unwrap();
        assert_eq!(&loaded, codec);
    }

    #[test]
    fn from_assets_rejects_assets_that_config_does_not_name() {
        let codec = codec();
        let load = |rotation: Rotation, qjl: QjlMatrix| {
            TurboQuantProdV1::from_assets(config(), rotation, codec.codebook().clone(), qjl)
        };
        let mismatch = |asset, field, expected, actual| {
            Err(TurboProdError::AssetMismatch {
                asset,
                field,
                expected,
                actual,
            })
        };
        let rotation = || codec.rotation().clone();
        let qjl = || codec.qjl().clone();
        let small = Rotation::generate(16, 163);

        assert_eq!(load(small, qjl()), mismatch("rotation", "dim", 512, 16));
        // The QJL matrix's seed in the rotation's place: same shape, wrong Π.
        let rotation_bytes = codec.rotation().to_bytes();
        let mut reseeded = rotation_bytes.clone();
        reseeded[8..16].copy_from_slice(&165u64.to_le_bytes());
        assert_eq!(
            load(Rotation::from_bytes(&reseeded).unwrap(), qjl()),
            mismatch("rotation", "seed", 163, 165)
        );
        let mut old_generator = rotation_bytes;
        old_generator[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            load(Rotation::from_bytes(&old_generator).unwrap(), qjl()),
            mismatch("rotation", "generator_version", 1, 0)
        );

        assert_eq!(
            load(rotation(), QjlMatrix::generate(512, 512, 163)),
            mismatch("qjl", "seed", 165, 163)
        );
        assert_eq!(
            load(rotation(), QjlMatrix::generate(512, 256, 165)),
            mismatch("qjl", "qjl_dim", 512, 256)
        );
        let mut old_generator = codec.qjl().to_bytes();
        old_generator[20..24].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            load(rotation(), QjlMatrix::from_bytes(&old_generator).unwrap()),
            mismatch("qjl", "generator_version", 1, 0)
        );

        let solved_3_bit = LloydMaxSolution::solve(512, 3).to_codebook();
        assert_eq!(
            TurboQuantProdV1::from_assets(config(), rotation(), solved_3_bit, qjl()),
            mismatch("codebook", "bits", 2, 3)
        );
        let small_codebook = LloydMaxSolution::solve(16, 2).to_codebook();
        assert_eq!(
            TurboQuantProdV1::from_assets(config(), rotation(), small_codebook, qjl()),
            mismatch("codebook", "dim", 512, 16)
        );
        // Right dim and bits, but an outer centroid one f32 ulp further out.
        let mut centroids = codec.codebook().centroids().to_vec();
        centroids[3] = centroids[3].next_up();
        let edited = LloydMaxCodebook::from_centroids(512, 2, centroids);
        assert_eq!(
            TurboQuantProdV1::from_assets(config(), rotation(), edited, qjl()),
            Err(TurboProdError::NotTheCommittedCodebook)
        );
    }

    #[test]
    fn a_qjl_matrix_with_m_and_d_swapped_is_rejected() {
        // m = 1024 rows of d = 512, against a 512-row matrix of width 1024.
        let config = TurboQuantConfig {
            qjl_dim: 1024,
            ..config()
        };
        assert_eq!(
            TurboQuantProdV1::from_assets(
                config,
                codec().rotation().clone(),
                codec().codebook().clone(),
                QjlMatrix::generate(1024, 512, 165),
            ),
            Err(TurboProdError::AssetMismatch {
                asset: "qjl",
                field: "dim",
                expected: 512,
                actual: 1024,
            })
        );
    }

    #[test]
    #[should_panic(expected = "sign code length doesn't match qjl_dim 512")]
    fn scoring_a_code_from_another_qjl_dim_panics() {
        let narrow = TurboQuantProdV1::from_assets(
            TurboQuantConfig {
                qjl_dim: 256,
                ..config()
            },
            codec().rotation().clone(),
            codec().codebook().clone(),
            QjlMatrix::generate(512, 256, 165),
        )
        .unwrap();
        let encoded = narrow.encode(&golden_vectors()[0]).unwrap();
        let prepared = codec().prepare_query(&golden_vectors()[1]).unwrap();
        prepared.score(encoded.code());
    }

    #[test]
    #[should_panic(expected = "index code length doesn't match dim 512")]
    fn scoring_a_truncated_index_code_panics() {
        let encoded = codec().encode(&golden_vectors()[0]).unwrap();
        let prepared = codec().prepare_query(&golden_vectors()[1]).unwrap();
        prepared.score(TurboProdCode {
            idx: &encoded.idx[..64],
            ..encoded.code()
        });
    }

    #[test]
    fn constructors_reject_configs_this_codec_does_not_implement() {
        let legacy = TurboQuantConfig::legacy_v1();
        assert_eq!(
            TurboQuantProdV1::generate(legacy),
            Err(TurboProdError::NotTurboQuantProd {
                codec_id: TurboCodecId::Legacy3BitV1
            })
        );
        let as_is = TurboQuantConfig {
            norm_policy: NormPolicy::AsIs,
            ..config()
        };
        assert_eq!(
            TurboQuantProdV1::generate(as_is),
            Err(TurboProdError::Config(
                TurboQuantConfigError::UnsupportedNormPolicy {
                    codec_id: TurboCodecId::TurboQuantProdV1,
                    norm_policy: NormPolicy::AsIs,
                }
            ))
        );
        // Valid for the config, but not packable in 2-bit indices.
        for mse_bits in [1, 3, 4] {
            let config = TurboQuantConfig {
                mse_bits,
                ..config()
            };
            assert_eq!(config.validate(), Ok(()));
            assert_eq!(
                TurboQuantProdV1::generate(config),
                Err(TurboProdError::UnsupportedMseBits { mse_bits })
            );
        }
        let small_assets = || {
            (
                Rotation::generate(16, 163),
                LloydMaxSolution::solve(16, 2).to_codebook(),
                QjlMatrix::generate(16, 16, 165),
            )
        };
        let (rotation, codebook, qjl) = small_assets();
        assert_eq!(
            TurboQuantProdV1::from_assets(legacy, rotation, codebook, qjl),
            Err(TurboProdError::NotTurboQuantProd {
                codec_id: TurboCodecId::Legacy3BitV1
            })
        );
        let (rotation, codebook, qjl) = small_assets();
        assert_eq!(
            TurboQuantProdV1::from_assets(
                TurboQuantConfig {
                    dim: 16,
                    ..config()
                },
                rotation,
                codebook,
                qjl
            ),
            Err(TurboProdError::Config(
                TurboQuantConfigError::UnsupportedDim {
                    codec_id: TurboCodecId::TurboQuantProdV1,
                    dim: 16,
                }
            ))
        );
    }
}
