pub mod assets;
pub mod header;
pub mod meta;
pub mod mmap_index;
pub mod record;
pub mod static_builder;
pub mod static_source;
pub mod turbo_codec;

pub struct ModuleBoundary;

pub use assets::{AssetError, CentroidTable, ProjectionMatrix};
pub use header::{KnownRecordLayout, TurboHeader, TURBO_MAGIC};
pub use meta::{CorpusTypeId, MetaRecord, META_RECORD_SIZE};
pub use mmap_index::MmapIndex;
pub use record::{TurboRecord512, TurboRecordRef, TurboRecordSlice, TypedTurboRecordRef};
pub use static_builder::{StaticChunk, StaticIndexBuildResult, StaticIndexBuilder};
pub use static_source::{load_static_chunks_from_s3, StaticSourceConfig, TurboBuildConfig};
pub use turbo_codec::{
    encode_vector, score_query_against_record, score_query_against_record_512,
    EncodedTurboVector,
};
