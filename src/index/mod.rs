pub mod assets;
pub mod header;
pub mod meta;
pub mod meta_ext;
pub mod mmap_index;
pub mod record;
pub mod release_manifest;
pub mod static_builder;
pub mod static_source;
pub mod turbo_codec;

pub use assets::{AssetError, CentroidTable, ProjectionMatrix};
pub use header::{
    KnownRecordLayout, TurboHeader, TurboHeaderError, TURBO_MAGIC, TURBO_VERSION_V2,
    TURBO_VERSION_V3,
};
pub use meta::{CorpusTypeId, MetaRecord, META_RECORD_SIZE};
pub use meta_ext::{MetaExtRecord, META_EXT_RECORD_SIZE};
pub use mmap_index::MmapIndex;
pub use record::{TurboRecord512, TurboRecordRef, TurboRecordSlice, TypedTurboRecordRef};
pub use static_builder::{StaticChunk, StaticIndexBuildResult, StaticIndexBuilder};
#[cfg(feature = "aws")]
pub use static_source::load_static_chunks_from_s3;
pub use static_source::{parse_static_source_lines, StaticSourceConfig, TurboBuildConfig};
pub use turbo_codec::{
    encode_vector, score_query_against_record, score_query_against_record_512,
    score_query_against_record_512_breakdown, EncodedTurboVector, TurboScoreBreakdown,
};
