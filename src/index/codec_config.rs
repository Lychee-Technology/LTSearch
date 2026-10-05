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

use sha2::{Digest, Sha256};

use super::gaussian::GAUSSIAN_GENERATOR_VERSION;
use super::release_manifest::{CodecMetadata, V4CodecMetadata};

/// Bytes in a [`TurboQuantConfig::fingerprint`].
pub const CODEC_FINGERPRINT_LEN: usize = 8;

/// Prefix of the hash behind [`TurboQuantConfig::fingerprint`], so its input
/// can't be mistaken for that of another digest in a release.
const FINGERPRINT_DOMAIN: &[u8] = b"ltsearch/turbo-codec-fingerprint/v1";

/// Identifies a TurboQuant codec: its encoding, record layout and assets.
///
/// [`code`](Self::code) and [`name`](Self::name) are the stable identifiers
/// for persisted metadata: a v4 `turbo_static.bin` header stores the code and
/// a v4 release manifest stores the name. Never renumber or rename a variant;
/// add a new one instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurboCodecId {
    /// The v2/v3 codec in `turbo_codec.rs`: a 2-bit index into seeded random
    /// per-dimension centroids, plus one sign bit per row of a seeded uniform
    /// projection of the residual, scaled by the residual norm γ.
    Legacy3BitV1,
    /// TurboQuant_prod, implemented by
    /// [`TurboQuantProdV1`](super::TurboQuantProdV1): Haar rotation, a
    /// `mse_bits` [Lloyd-Max codebook](super::LloydMaxCodebook) and a
    /// `qjl_dim`-row Gaussian QJL sign sketch.
    TurboQuantProdV1,
}

impl TurboCodecId {
    /// Every codec, in code order.
    pub const ALL: [Self; 2] = [Self::Legacy3BitV1, Self::TurboQuantProdV1];

    /// Stable numeric identifier. 0 is reserved for "no codec id": the v2/v3
    /// header bytes a v4 header stores it in are zero.
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
    /// (`TurboRecord512` and `TurboProdRecord512`).
    const fn supports_dim(self, dim: u32) -> bool {
        match self {
            Self::Legacy3BitV1 | Self::TurboQuantProdV1 => dim == 512,
        }
    }

    /// The one norm policy each codec's encoder implements.
    const fn norm_policy(self) -> NormPolicy {
        match self {
            Self::Legacy3BitV1 => NormPolicy::AsIs,
            Self::TurboQuantProdV1 => NormPolicy::NormalizeAndStore,
        }
    }
}

impl fmt::Display for TurboCodecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How a codec treats the input vector's norm. Each codec implements exactly
/// one policy, which [`TurboQuantConfig::validate`] enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NormPolicy {
    /// Encode the vector as given: no normalization, no norm check, no stored
    /// norm. This is what the legacy codec does.
    AsIs,
    /// Encode x/‖x‖ and store ‖x‖ as an f32 with the code; the score is ‖x‖
    /// times the estimate for the unit vector. `TurboQuantProdV1` does this;
    /// [`super::turbo_prod`] says why.
    NormalizeAndStore,
}

impl NormPolicy {
    /// Every policy, in code order.
    pub const ALL: [Self; 2] = [Self::AsIs, Self::NormalizeAndStore];

    /// Stable numeric identifier, hashed into
    /// [`TurboQuantConfig::fingerprint`]. Never renumber a variant.
    pub const fn code(self) -> u8 {
        match self {
            Self::AsIs => 0,
            Self::NormalizeAndStore => 1,
        }
    }

    /// Stable string identifier, stored in a v4 release manifest. Never
    /// rename a variant.
    pub const fn name(self) -> &'static str {
        match self {
            Self::AsIs => "as_is",
            Self::NormalizeAndStore => "normalize_and_store",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|policy| policy.name() == name)
    }
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
    ///
    /// [`validate`](Self::validate) accepts any version, because a release
    /// built under an older generator must stay loadable. Only
    /// [`TurboQuantProdV1::generate`](super::TurboQuantProdV1::generate)
    /// requires the current one.
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

    /// The codec `static-build` writes a v4 release with.
    ///
    /// The seeds only name which matrices are drawn, so any pair works. 163
    /// and 165 are the pair the golden digests of [`Rotation`](super::Rotation),
    /// [`QjlMatrix`](super::QjlMatrix) and the encoder are captured with, so
    /// those tests pin this codec's assets and encoded bytes. Changing a
    /// field changes the release ID of every rebuild.
    pub const fn prod_v1() -> Self {
        Self {
            codec_id: TurboCodecId::TurboQuantProdV1,
            dim: 512,
            mse_bits: 2,
            qjl_dim: 512,
            mse_seed: 163,
            qjl_seed: 165,
            generator_version: GAUSSIAN_GENERATOR_VERSION,
            norm_policy: NormPolicy::NormalizeAndStore,
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
        if self.norm_policy != self.codec_id.norm_policy() {
            return Err(TurboQuantConfigError::UnsupportedNormPolicy {
                codec_id: self.codec_id,
                norm_policy: self.norm_policy,
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
    /// has one; v4 manifests record the whole config instead
    /// ([`to_v4_codec_metadata`](Self::to_v4_codec_metadata)).
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

    /// The v4 manifest's codec section: every field of this config. Only
    /// `TurboQuantProdV1` is written as a v4 release.
    pub fn to_v4_codec_metadata(&self) -> Result<V4CodecMetadata, TurboQuantConfigError> {
        self.validate()?;
        if self.codec_id != TurboCodecId::TurboQuantProdV1 {
            return Err(TurboQuantConfigError::NotAV4Codec {
                codec_id: self.codec_id,
            });
        }
        Ok(V4CodecMetadata {
            codec_id: self.codec_id.name().to_string(),
            dim: self.dim,
            mse_bits: self.mse_bits,
            qjl_dim: self.qjl_dim,
            rotation_seed: self.mse_seed,
            qjl_seed: self.qjl_seed,
            generator_version: self.generator_version,
            norm_policy: self.norm_policy.name().to_string(),
        })
    }

    /// The config a v4 manifest's codec section records, the inverse of
    /// [`to_v4_codec_metadata`](Self::to_v4_codec_metadata).
    pub fn from_v4_codec_metadata(codec: &V4CodecMetadata) -> Result<Self, TurboQuantConfigError> {
        let codec_id = TurboCodecId::from_name(&codec.codec_id).ok_or_else(|| {
            TurboQuantConfigError::UnknownCodecName {
                name: codec.codec_id.clone(),
            }
        })?;
        let norm_policy = NormPolicy::from_name(&codec.norm_policy).ok_or_else(|| {
            TurboQuantConfigError::UnknownNormPolicyName {
                name: codec.norm_policy.clone(),
            }
        })?;
        if codec_id != TurboCodecId::TurboQuantProdV1 {
            return Err(TurboQuantConfigError::NotAV4Codec { codec_id });
        }
        let config = Self {
            codec_id,
            dim: codec.dim,
            mse_bits: codec.mse_bits,
            qjl_dim: codec.qjl_dim,
            mse_seed: codec.rotation_seed,
            qjl_seed: codec.qjl_seed,
            generator_version: codec.generator_version,
            norm_policy,
        };
        config.validate()?;
        Ok(config)
    }

    /// Every field as fixed-width little-endian bytes, in declaration order,
    /// with the codec and the norm policy as their stable codes.
    fn canonical_bytes(&self) -> [u8; 34] {
        let mut bytes = [0; 34];
        bytes[0..4].copy_from_slice(&self.codec_id.code().to_le_bytes());
        bytes[4..8].copy_from_slice(&self.dim.to_le_bytes());
        bytes[8] = self.mse_bits;
        bytes[9..13].copy_from_slice(&self.qjl_dim.to_le_bytes());
        bytes[13..21].copy_from_slice(&self.mse_seed.to_le_bytes());
        bytes[21..29].copy_from_slice(&self.qjl_seed.to_le_bytes());
        bytes[29..33].copy_from_slice(&self.generator_version.to_le_bytes());
        bytes[33] = self.norm_policy.code();
        bytes
    }

    /// Ties encoded records to the codec that produced them: the first
    /// [`CODEC_FINGERPRINT_LEN`] bytes of
    ///
    /// ```text
    /// sha256(domain ‖ config ‖ for each asset file, by name:
    ///        len(name) as u64 LE ‖ name ‖ sha256(file bytes))
    /// ```
    ///
    /// where `config` is every field of `self` as fixed-width little-endian
    /// bytes. The order of `asset_files` doesn't matter as long as the names
    /// are distinct.
    ///
    /// A v4 `turbo_static.bin` header stores it, and the loader recomputes
    /// it from the asset files it finds next to the records, so records
    /// can't be scored with the assets of another build. It is 8 bytes
    /// because the header has 8 spare: enough to catch files that were
    /// paired or damaged by accident, not a file crafted to collide. The
    /// release manifest's per-file sha256 covers that.
    pub fn fingerprint(&self, asset_files: &[(&str, &[u8])]) -> [u8; CODEC_FINGERPRINT_LEN] {
        let mut sorted = asset_files.to_vec();
        sorted.sort_by_key(|(name, _)| *name);

        let mut hasher = Sha256::new();
        hasher.update(FINGERPRINT_DOMAIN);
        hasher.update(self.canonical_bytes());
        for (name, bytes) in sorted {
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update(Sha256::digest(bytes));
        }
        let digest = hasher.finalize();
        digest[..CODEC_FINGERPRINT_LEN]
            .try_into()
            .expect("a sha256 digest is longer than the fingerprint")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurboQuantConfigError {
    ZeroDim,
    UnsupportedMseBits {
        mse_bits: u8,
    },
    ZeroQjlDim,
    UnsupportedDim {
        codec_id: TurboCodecId,
        dim: u32,
    },
    UnsupportedNormPolicy {
        codec_id: TurboCodecId,
        norm_policy: NormPolicy,
    },
    UnsupportedLegacyLayout {
        mse_bits: u8,
        qjl_dim: u32,
    },
    NotAV3Codec {
        codec_id: TurboCodecId,
    },
    NotAV4Codec {
        codec_id: TurboCodecId,
    },
    /// A manifest names a codec this build doesn't know.
    UnknownCodecName {
        name: String,
    },
    /// A manifest names a norm policy this build doesn't know.
    UnknownNormPolicyName {
        name: String,
    },
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
            Self::UnsupportedNormPolicy {
                codec_id,
                norm_policy,
            } => write!(
                f,
                "codec {codec_id} requires norm policy {:?}, got {norm_policy:?}",
                codec_id.norm_policy()
            ),
            Self::UnsupportedLegacyLayout { mse_bits, qjl_dim } => write!(
                f,
                "codec {} requires mse_bits 2 and qjl_dim == dim, got mse_bits {mse_bits} and qjl_dim {qjl_dim}",
                TurboCodecId::Legacy3BitV1
            ),
            Self::NotAV3Codec { codec_id } => {
                write!(f, "codec {codec_id} has no v3 manifest representation")
            }
            Self::NotAV4Codec { codec_id } => {
                write!(f, "codec {codec_id} has no v4 manifest representation")
            }
            Self::UnknownCodecName { name } => write!(f, "unknown codec_id {name:?}"),
            Self::UnknownNormPolicyName { name } => write!(f, "unknown norm_policy {name:?}"),
        }
    }
}

impl std::error::Error for TurboQuantConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn prod() -> TurboQuantConfig {
        TurboQuantConfig::prod_v1()
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
    fn validate_rejects_a_norm_policy_the_codec_does_not_implement() {
        for (config, norm_policy) in [
            (TurboQuantConfig::legacy_v1(), NormPolicy::NormalizeAndStore),
            (prod(), NormPolicy::AsIs),
        ] {
            let config = TurboQuantConfig {
                norm_policy,
                ..config
            };
            assert_eq!(
                config.validate(),
                Err(TurboQuantConfigError::UnsupportedNormPolicy {
                    codec_id: config.codec_id,
                    norm_policy,
                })
            );
        }
        assert_eq!(
            TurboQuantConfig {
                norm_policy: NormPolicy::AsIs,
                ..prod()
            }
            .validate()
            .unwrap_err()
            .to_string(),
            "codec turbo_quant_prod_v1 requires norm policy NormalizeAndStore, got AsIs"
        );
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

    #[test]
    fn norm_policy_identifiers_are_pinned() {
        // Persisted identifiers: never change an existing row.
        let pinned = [
            (NormPolicy::AsIs, 0, "as_is"),
            (NormPolicy::NormalizeAndStore, 1, "normalize_and_store"),
        ];
        assert_eq!(NormPolicy::ALL.len(), pinned.len());
        for (policy, code, name) in pinned {
            assert_eq!(policy.code(), code);
            assert_eq!(policy.name(), name);
            assert_eq!(NormPolicy::from_name(name), Some(policy));
        }
        assert_eq!(NormPolicy::from_name("NormalizeAndStore"), None);
    }

    #[test]
    fn prod_v1_is_the_codec_v4_releases_are_built_with() {
        let prod = prod();
        assert_eq!(prod.validate(), Ok(()));
        assert_eq!(prod.total_bits(), 3.0);
        // The codec section of a default v4 manifest, and the codec input to
        // its release ID.
        assert_eq!(
            prod.to_v4_codec_metadata(),
            Ok(V4CodecMetadata {
                codec_id: "turbo_quant_prod_v1".to_string(),
                dim: 512,
                mse_bits: 2,
                qjl_dim: 512,
                rotation_seed: 163,
                qjl_seed: 165,
                generator_version: 1,
                norm_policy: "normalize_and_store".to_string(),
            })
        );
    }

    #[test]
    fn v4_codec_metadata_round_trips_every_field() {
        let config = TurboQuantConfig {
            mse_bits: 3,
            qjl_dim: 256,
            mse_seed: u64::MAX,
            qjl_seed: 1 << 40,
            generator_version: 7,
            ..prod()
        };
        let metadata = config.to_v4_codec_metadata().unwrap();
        assert_eq!(metadata.mse_bits, 3);
        assert_eq!(metadata.qjl_dim, 256);
        assert_eq!(metadata.rotation_seed, u64::MAX);
        assert_eq!(metadata.qjl_seed, 1 << 40);
        assert_eq!(metadata.generator_version, 7);
        assert_eq!(
            TurboQuantConfig::from_v4_codec_metadata(&metadata),
            Ok(config)
        );
    }

    #[test]
    fn only_a_valid_prod_config_has_v4_codec_metadata() {
        assert_eq!(
            TurboQuantConfig::legacy_v1().to_v4_codec_metadata(),
            Err(TurboQuantConfigError::NotAV4Codec {
                codec_id: TurboCodecId::Legacy3BitV1,
            })
        );
        let invalid_prod = TurboQuantConfig {
            mse_bits: 5,
            ..prod()
        };
        assert_eq!(
            invalid_prod.to_v4_codec_metadata(),
            Err(TurboQuantConfigError::UnsupportedMseBits { mse_bits: 5 })
        );
    }

    #[test]
    fn v4_codec_metadata_this_build_cannot_load_is_rejected() {
        let metadata = prod().to_v4_codec_metadata().unwrap();
        let parse = |edit: fn(&mut V4CodecMetadata)| {
            let mut metadata = metadata.clone();
            edit(&mut metadata);
            TurboQuantConfig::from_v4_codec_metadata(&metadata)
        };

        assert_eq!(
            parse(|m| m.codec_id = "turbo_quant_prod_v2".to_string()),
            Err(TurboQuantConfigError::UnknownCodecName {
                name: "turbo_quant_prod_v2".to_string(),
            })
        );
        assert_eq!(
            parse(|m| m.norm_policy = "renormalize".to_string()),
            Err(TurboQuantConfigError::UnknownNormPolicyName {
                name: "renormalize".to_string(),
            })
        );
        // Known names that are not a v4 release's.
        assert_eq!(
            parse(|m| m.codec_id = "legacy_3bit_v1".to_string()),
            Err(TurboQuantConfigError::NotAV4Codec {
                codec_id: TurboCodecId::Legacy3BitV1,
            })
        );
        assert_eq!(
            parse(|m| m.norm_policy = "as_is".to_string()),
            Err(TurboQuantConfigError::UnsupportedNormPolicy {
                codec_id: TurboCodecId::TurboQuantProdV1,
                norm_policy: NormPolicy::AsIs,
            })
        );
        // The parsed config is validated like any other.
        assert_eq!(
            parse(|m| m.dim = 384),
            Err(TurboQuantConfigError::UnsupportedDim {
                codec_id: TurboCodecId::TurboQuantProdV1,
                dim: 384,
            })
        );
        assert_eq!(
            parse(|m| m.mse_bits = 0),
            Err(TurboQuantConfigError::UnsupportedMseBits { mse_bits: 0 })
        );
    }

    const FINGERPRINT_ASSETS: [(&str, &[u8]); 3] = [
        ("rotation.bin", b"rotation"),
        ("codebook.bin", b"codebook"),
        ("qjl.bin", b"qjl"),
    ];

    #[test]
    fn fingerprint_is_pinned() {
        // Computed outside this crate from the formula in the docs of
        // `fingerprint`. Every v4 `turbo_static.bin` stores one, so a change
        // here makes every existing v4 release fail to load.
        assert_eq!(
            prod().fingerprint(&FINGERPRINT_ASSETS),
            [0x02, 0x92, 0x0C, 0x58, 0x6A, 0x70, 0x9E, 0x75]
        );
    }

    #[test]
    fn fingerprint_ignores_the_order_the_assets_are_given_in() {
        let mut reversed = FINGERPRINT_ASSETS;
        reversed.reverse();
        assert_eq!(
            prod().fingerprint(&reversed),
            prod().fingerprint(&FINGERPRINT_ASSETS)
        );
    }

    #[test]
    fn fingerprint_covers_every_config_field() {
        let prod = prod();
        let changed = [
            TurboQuantConfig {
                codec_id: TurboCodecId::Legacy3BitV1,
                ..prod
            },
            TurboQuantConfig { dim: 513, ..prod },
            TurboQuantConfig {
                mse_bits: 3,
                ..prod
            },
            TurboQuantConfig {
                qjl_dim: 513,
                ..prod
            },
            TurboQuantConfig {
                mse_seed: 164,
                ..prod
            },
            TurboQuantConfig {
                qjl_seed: 164,
                ..prod
            },
            TurboQuantConfig {
                generator_version: 2,
                ..prod
            },
            TurboQuantConfig {
                norm_policy: NormPolicy::AsIs,
                ..prod
            },
            // The two seeds swapped: each field has its own position.
            TurboQuantConfig {
                mse_seed: 165,
                qjl_seed: 163,
                ..prod
            },
        ];

        let mut fingerprints = vec![prod.fingerprint(&FINGERPRINT_ASSETS)];
        for config in changed {
            let fingerprint = config.fingerprint(&FINGERPRINT_ASSETS);
            assert!(!fingerprints.contains(&fingerprint), "{config:?}");
            fingerprints.push(fingerprint);
        }
    }

    #[test]
    fn fingerprint_covers_every_asset_name_and_byte() {
        let changed: [[(&str, &[u8]); 3]; 6] = [
            // One byte of one file.
            [
                ("rotation.bin", b"rotatioN"),
                ("codebook.bin", b"codebook"),
                ("qjl.bin", b"qjl"),
            ],
            // A file under another name.
            [
                ("rotation2.bin", b"rotation"),
                ("codebook.bin", b"codebook"),
                ("qjl.bin", b"qjl"),
            ],
            // Two files' contents exchanged.
            [
                ("rotation.bin", b"qjl"),
                ("codebook.bin", b"codebook"),
                ("qjl.bin", b"rotation"),
            ],
            // An empty file.
            [
                ("rotation.bin", b""),
                ("codebook.bin", b"codebook"),
                ("qjl.bin", b"qjl"),
            ],
            // A byte moved from one file's end to its name.
            [
                ("rotation.binr", b"otation"),
                ("codebook.bin", b"codebook"),
                ("qjl.bin", b"qjl"),
            ],
            // The same bytes under a name that sorts elsewhere.
            [
                ("a.bin", b"rotation"),
                ("codebook.bin", b"codebook"),
                ("qjl.bin", b"qjl"),
            ],
        ];

        let mut fingerprints = vec![prod().fingerprint(&FINGERPRINT_ASSETS)];
        for assets in changed {
            let fingerprint = prod().fingerprint(&assets);
            assert!(!fingerprints.contains(&fingerprint), "{assets:?}");
            fingerprints.push(fingerprint);
        }
        // A missing file.
        assert!(!fingerprints.contains(&prod().fingerprint(&FINGERPRINT_ASSETS[..2])));
    }
}
