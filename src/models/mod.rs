pub mod index;
pub mod search;
pub mod write;

pub struct ModuleBoundary;

pub use index::{CacheStats, Document, IndexCache, IndexManifest, ShardManifest};
pub use search::{
    CorpusType, CorpusWeights, FilterValue, SearchRequest, SearchResponse, SearchResult,
    SearchSource,
};
pub use write::{DeleteResponse, HealthStatus, IngestResponse, WalOperation, WalRecord};
