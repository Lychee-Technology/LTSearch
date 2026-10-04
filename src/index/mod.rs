pub mod assets;
pub mod codebook;
pub mod codec_config;
pub mod gaussian;
pub mod header;
pub mod lance_source;
pub mod meta;
pub mod meta_ext;
pub mod mmap_index;
pub mod qjl;
pub mod record;
pub mod release_manifest;
pub mod rotation;
pub mod static_builder;
pub mod static_release;
pub mod static_source;
pub mod turbo_codec;
pub mod turbo_prod;

pub use assets::{AssetError, CentroidTable, ProjectionMatrix};
pub use codebook::{LloydMaxCodebook, LloydMaxSolution, CODEBOOK_FILE};
pub use codec_config::{
    NormPolicy, TurboCodecId, TurboQuantConfig, TurboQuantConfigError, CODEC_FINGERPRINT_LEN,
};
pub use gaussian::{fill_standard_normal, StandardNormalStream, GAUSSIAN_GENERATOR_VERSION};
pub use header::{
    HeaderCodec, KnownRecordLayout, TurboHeader, TurboHeaderError, TURBO_MAGIC, TURBO_VERSION_V2,
    TURBO_VERSION_V3, TURBO_VERSION_V4,
};
pub use lance_source::{load_lance_snapshot, LanceSnapshot, LanceStaticSourceConfig};
pub use meta::{CorpusTypeId, MetaRecord, META_RECORD_SIZE};
pub use meta_ext::{MetaExtRecord, META_EXT_RECORD_SIZE};
pub use mmap_index::{IndexCodec, MmapIndex, MmapIndexError};
pub use qjl::{PreparedQjlQuery, QjlMatrix, QJL_FILE};
pub use record::{
    TurboProdRecord512, TurboRecord512, TurboRecordRef, TurboRecordSlice, TypedTurboRecordRef,
};
pub use release_manifest::{
    canonical_metadata_json, content_digest, derive_release_id, sha256_hex, CanonicalRow,
    CodecMetadata, EmbeddingProfile, InputFingerprint, ManifestCodec, OutputFile, ReleaseManifest,
    ReleaseSource, V4CodecMetadata, RELEASE_MANIFEST_FILE,
};
pub use rotation::{Rotation, ROTATION_FILE};
pub use static_builder::{StaticChunk, StaticIndexBuildResult, StaticIndexBuilder};
pub use static_release::{
    release_output_files, StaticReleaseBuilder, StaticReleaseFormat, V3_RELEASE_OUTPUT_FILES,
    V4_RELEASE_OUTPUT_FILES,
};
#[cfg(feature = "aws")]
pub use static_source::load_static_chunks_from_s3;
pub use static_source::{parse_static_source_lines, StaticSourceConfig, TurboBuildConfig};
pub use turbo_codec::{encode_vector, EncodedTurboVector, PreparedTurboQuery, TurboScoreBreakdown};
pub use turbo_prod::{
    EncodedTurboProd, PreparedTurboProdQuery, TurboProdCode, TurboProdError,
    TurboProdScoreBreakdown, TurboQuantProdV1,
};
