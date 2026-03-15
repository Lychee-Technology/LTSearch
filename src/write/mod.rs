pub mod wal;

pub struct ModuleBoundary;

pub use wal::{segment_key, WalStorage, WriteAheadLog};
