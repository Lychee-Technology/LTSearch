//! Codec identity and parameters for TurboQuant static indexes.
//!
//! [`TurboQuantConfig`] names a codec ([`TurboCodecId`]) together with every
//! parameter that determines its encoded bytes. [`TurboQuantConfig::legacy_v1`]
//! is the single source of the legacy (v2/v3) codec's constants: both static
//! builders and the legacy scorer read them from there.
//!
//! # Materialization contract
//!
//! A codec's matrices are a function of its seeds and `generator_version`,
//! but only the builder runs a generator. It writes the generated matrices
//! into the release; the query side loads them and never regenerates. A
//! generator change therefore can't change how an existing release scores,
//! only what a rebuild of the same input produces. #156 is the precedent:
//! the rand 0.8 → 0.10 upgrade moved about 81% of the legacy generator's
//! values by up to 2.4e-7, so releases built before it kept scoring from
//! their stored assets, while rebuilding them yields different assets and a
//! different release ID.

use std::fmt;

use super::release_manifest::CodecMetadata;

/// Identifies a TurboQuant codec: its encoding, record layout and assets.
///
/// [`code`](Self::code) and [`name`](Self::name) are the stable identifiers
/// for persisted metadata (#166 decides where they are stored). Never
/// renumber or rename a variant; add a new one instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurboCodecId {
    /// The v2/v3 codec in `turbo_codec.rs`: a 2-bit index into seeded random
    /// per-dimension centroids, plus one sign bit per row of a seeded uniform
    /// projection of the residual, scaled by the residual norm γ.
    Legacy3BitV1,
    /// TurboQuant_prod (#165): Haar rotation, a `mse_bits` Lloyd-Max codebook
    /// and a `qjl_dim`-row Gaussian QJL sign sketch.
    TurboQuantProdV1,
}

impl TurboCodecId {
    /// Every codec, in code order.
    pub const ALL: [Self; 2] = [Self::Legacy3BitV1, Self::TurboQuantProdV1];

    /// Stable numeric identifier. 0 is reserved for "no codec id": the v2/v3
    /// header bytes a v4 header might store it in are zero.
    pub const fn code(self) -> u32 {
        match self {
            Self::Legacy3BitV1 => 1,
            Self::TurboQuantProdV1 => 2,
        }
    }

    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Legacy3BitV1),
            2 => Some(Self::TurboQuantProdV1),
            _ => None,
        }
    }

    /// Stable string identifier.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Legacy3BitV1 => "legacy_3bit_v1",
            Self::TurboQuantProdV1 => "turbo_quant_prod_v1",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|codec| codec.name() == name)
    }

    /// Both codecs have a single record layout, 512 dimensions wide
    /// (`TurboRecord512` today, a 512-dim v4 record in #166).
    const fn supports_dim(self, dim: u32) -> bool {
        match self {
            Self::Legacy3BitV1 | Self::TurboQuantProdV1 => dim == 512,
        }
    }
}

impl fmt::Display for TurboCodecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How a codec treats the input vector's norm. The TurboQuant_prod policy is
/// decided in #165; this only reserves the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NormPolicy {
    /// Encode the vector as given: no normalization, no norm check, no stored
    /// norm. This is what the legacy codec does.
    AsIs,
}

/// A codec and every parameter that determines its encoded bytes.
///
/// Fields are public so a config can be read back from persisted metadata;
/// call [`validate`](Self::validate) before building or loading with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurboQuantConfig {
    pub codec_id: TurboCodecId,
    /// Embedding dimension d.
    pub dim: u32,
    /// Bits per dimension of the MSE stage's codebook index, so each
    /// dimension has `2^mse_bits` centroids.
    pub mse_bits: u8,
    /// m, the number of QJL sign bits (projection rows) per record. Separate
    /// from `dim` even though both codecs use m = d today.
    pub qjl_dim: u32,
    /// Seeds the MSE stage's random asset: the Haar
    /// [`Rotation`](super::Rotation) for `TurboQuantProdV1` and the
    /// per-dimension centroid table for `Legacy3BitV1`, which has no rotation.
    pub mse_seed: u64,
    /// Seeds the QJL projection: Gaussian rows for `TurboQuantProdV1` (#165)
    /// and the uniform `ProjectionMatrix` for `Legacy3BitV1`.
    pub qjl_seed: u64,
    /// Version of the generator that turned the seeds into the stored
    /// matrices. For `TurboQuantProdV1` this is the
    /// [`GAUSSIAN_GENERATOR_VERSION`](super::gaussian::GAUSSIAN_GENERATOR_VERSION)
    /// the builder ran. 0 means the legacy uniform generators
    /// (`CentroidTable::generate`, `ProjectionMatrix::generate`), which
    /// predate versioning.
    pub generator_version: u32,
    pub norm_policy: NormPolicy,
}

impl TurboQuantConfig {
    /// The codec every v2 and v3 static index is built with.
    pub const fn legacy_v1() -> Self {
        Self {
            codec_id: TurboCodecId::Legacy3BitV1,
            dim: 512,
            mse_bits: 2,
            qjl_dim: 512,
            mse_seed: 7,
            qjl_seed: 11,
            generator_version: 0,
            norm_policy: NormPolicy::AsIs,
        }
    }

    /// `2^mse_bits`, the number of centroids per dimension. Panics if
    /// `mse_bits >= 32`; [`validate`](Self::validate) only accepts 1..=4.
    pub const fn centroids_per_dim(&self) -> u32 {
        match 1u32.checked_shl(self.mse_bits as u32) {
            Some(count) => count,
            None => panic!("mse_bits must be below 32"),
        }
    }

    /// Bits per dimension: `mse_bits + qjl_dim / dim`. Derived rather than
    /// stored so it can't disagree with the other fields.
    pub fn total_bits(&self) -> f64 {
        f64::from(self.mse_bits) + f64::from(self.qjl_dim) / f64::from(self.dim)
    }

    pub fn validate(&self) -> Result<(), TurboQuantConfigError> {
        if self.dim == 0 {
            return Err(TurboQuantConfigError::ZeroDim);
        }
        if !(1..=4).contains(&self.mse_bits) {
            return Err(TurboQuantConfigError::UnsupportedMseBits {
                mse_bits: self.mse_bits,
            });
        }
        if self.qjl_dim == 0 {
            return Err(TurboQuantConfigError::ZeroQjlDim);
        }
        if !self.codec_id.supports_dim(self.dim) {
            return Err(TurboQuantConfigError::UnsupportedDim {
                codec_id: self.codec_id,
                dim: self.dim,
            });
        }
        // The legacy record layout and scorer hard-code a 2-bit index and
        // one sign bit per dimension.
        if self.codec_id == TurboCodecId::Legacy3BitV1
            && (self.mse_bits != 2 || self.qjl_dim != self.dim)
        {
            return Err(TurboQuantConfigError::UnsupportedLegacyLayout {
                mse_bits: self.mse_bits,
                qjl_dim: self.qjl_dim,
            });
        }
        Ok(())
    }

    /// The v3 manifest's codec section for this config. Only `Legacy3BitV1`
    /// has one; v4 manifests (#166) record the whole config instead.
    pub fn to_v3_codec_metadata(&self) -> Result<CodecMetadata, TurboQuantConfigError> {
        self.validate()?;
        if self.codec_id != TurboCodecId::Legacy3BitV1 {
            return Err(TurboQuantConfigError::NotAV3Codec {
                codec_id: self.codec_id,
            });
        }
        Ok(CodecMetadata {
            dim: self.dim,
            centroids_per_dim: self.centroids_per_dim(),
            centroids_seed: self.mse_seed,
            projection_seed: self.qjl_seed,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurboQuantConfigError {
    ZeroDim,
    UnsupportedMseBits { mse_bits: u8 },
    ZeroQjlDim,
    UnsupportedDim { codec_id: TurboCodecId, dim: u32 },
    UnsupportedLegacyLayout { mse_bits: u8, qjl_dim: u32 },
    NotAV3Codec { codec_id: TurboCodecId },
}

impl fmt::Display for TurboQuantConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDim => write!(f, "dim must be positive"),
            Self::UnsupportedMseBits { mse_bits } => {
                write!(f, "mse_bits must be in 1..=4, got {mse_bits}")
            }
            Self::ZeroQjlDim => write!(f, "qjl_dim must be positive"),
            Self::UnsupportedDim { codec_id, dim } => {
                write!(f, "codec {codec_id} does not support dim {dim}")
            }
            Self::UnsupportedLegacyLayout { mse_bits, qjl_dim } => write!(
                f,
                "codec {} requires mse_bits 2 and qjl_dim == dim, got mse_bits {mse_bits} and qjl_dim {qjl_dim}",
                TurboCodecId::Legacy3BitV1
            ),
            Self::NotAV3Codec { codec_id } => {
                write!(f, "codec {codec_id} has no v3 manifest representation")
            }
        }
    }
}

impl std::error::Error for TurboQuantConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::gaussian::GAUSSIAN_GENERATOR_VERSION;

    fn prod() -> TurboQuantConfig {
        TurboQuantConfig {
            codec_id: TurboCodecId::TurboQuantProdV1,
            mse_seed: 163,
            qjl_seed: 165,
            generator_version: GAUSSIAN_GENERATOR_VERSION,
            ..TurboQuantConfig::legacy_v1()
        }
    }

    #[test]
    fn legacy_v1_is_the_codec_v3_releases_were_built_with() {
        let legacy = TurboQuantConfig::legacy_v1();
        assert_eq!(legacy.validate(), Ok(()));
        assert_eq!(legacy.total_bits(), 3.0);
        // The codec section of every v3 manifest, and the codec input to
        // every v3 release ID, built so far.
        assert_eq!(
            legacy.to_v3_codec_metadata(),
            Ok(CodecMetadata {
                dim: 512,
                centroids_per_dim: 4,
                centroids_seed: 7,
                projection_seed: 11,
            })
        );
    }

    #[test]
    fn validate_rejects_zero_dim() {
        for config in [TurboQuantConfig::legacy_v1(), prod()] {
            let config = TurboQuantConfig { dim: 0, ..config };
            assert_eq!(config.validate(), Err(TurboQuantConfigError::ZeroDim));
        }
    }

    #[test]
    fn validate_rejects_mse_bits_outside_1_to_4() {
        for mse_bits in [0, 5, 8, u8::MAX] {
            let config = TurboQuantConfig { mse_bits, ..prod() };
            assert_eq!(
                config.validate(),
                Err(TurboQuantConfigError::UnsupportedMseBits { mse_bits })
            );
        }
        for mse_bits in 1..=4 {
            let config = TurboQuantConfig { mse_bits, ..prod() };
            assert_eq!(config.validate(), Ok(()), "mse_bits {mse_bits}");
            assert_eq!(config.centroids_per_dim(), 1 << mse_bits);
        }
    }

    #[test]
    fn validate_rejects_zero_qjl_dim() {
        for config in [TurboQuantConfig::legacy_v1(), prod()] {
            let config = TurboQuantConfig {
                qjl_dim: 0,
                ..config
            };
            assert_eq!(config.validate(), Err(TurboQuantConfigError::ZeroQjlDim));
        }
    }

    #[test]
    fn validate_rejects_unsupported_codec_dim_pairs() {
        for config in [TurboQuantConfig::legacy_v1(), prod()] {
            for dim in [1, 256, 511, 513, 1024] {
                let config = TurboQuantConfig {
                    dim,
                    qjl_dim: dim,
                    ..config
                };
                assert_eq!(
                    config.validate(),
                    Err(TurboQuantConfigError::UnsupportedDim {
                        codec_id: config.codec_id,
                        dim,
                    })
                );
            }
        }
    }

    #[test]
    fn validate_rejects_legacy_parameters_its_record_layout_cannot_hold() {
        for (mse_bits, qjl_dim) in [(1, 512), (3, 512), (2, 256), (2, 1024)] {
            let config = TurboQuantConfig {
                mse_bits,
                qjl_dim,
                ..TurboQuantConfig::legacy_v1()
            };
            assert_eq!(
                config.validate(),
                Err(TurboQuantConfigError::UnsupportedLegacyLayout { mse_bits, qjl_dim })
            );
        }
    }

    #[test]
    fn qjl_dim_is_independent_of_dim() {
        for (qjl_dim, total_bits) in [(256, 2.5), (512, 3.0), (1024, 4.0)] {
            let config = TurboQuantConfig { qjl_dim, ..prod() };
            assert_eq!(config.validate(), Ok(()), "qjl_dim {qjl_dim}");
            assert_eq!(config.total_bits(), total_bits);
        }
    }

    #[test]
    fn only_a_valid_legacy_config_has_v3_codec_metadata() {
        assert_eq!(
            prod().to_v3_codec_metadata(),
            Err(TurboQuantConfigError::NotAV3Codec {
                codec_id: TurboCodecId::TurboQuantProdV1,
            })
        );
        let invalid_legacy = TurboQuantConfig {
            mse_bits: 3,
            ..TurboQuantConfig::legacy_v1()
        };
        assert_eq!(
            invalid_legacy.to_v3_codec_metadata(),
            Err(TurboQuantConfigError::UnsupportedLegacyLayout {
                mse_bits: 3,
                qjl_dim: 512,
            })
        );
    }

    #[test]
    fn codec_identifiers_are_pinned() {
        // Persisted identifiers: never change an existing row.
        let pinned = [
            (TurboCodecId::Legacy3BitV1, 1, "legacy_3bit_v1"),
            (TurboCodecId::TurboQuantProdV1, 2, "turbo_quant_prod_v1"),
        ];
        assert_eq!(TurboCodecId::ALL.len(), pinned.len());
        for (codec_id, code, name) in pinned {
            assert_eq!(codec_id.code(), code);
            assert_eq!(codec_id.name(), name);
            assert_eq!(TurboCodecId::from_code(code), Some(codec_id));
            assert_eq!(TurboCodecId::from_name(name), Some(codec_id));
        }
        assert_eq!(TurboCodecId::from_code(0), None);
        assert_eq!(TurboCodecId::from_code(3), None);
        assert_eq!(TurboCodecId::from_name("Legacy3BitV1"), None);
    }
}
