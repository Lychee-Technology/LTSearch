pub mod header;
pub mod meta;
pub mod mmap_index;
pub mod record;

pub struct ModuleBoundary;

pub use header::{TurboHeader, TURBO_MAGIC};
pub use meta::{CorpusTypeId, MetaRecord, META_RECORD_SIZE};
pub use mmap_index::MmapIndex;
pub use record::TurboRecordRef;
