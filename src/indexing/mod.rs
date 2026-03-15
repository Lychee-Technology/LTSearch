pub mod builder;

pub struct ModuleBoundary;

pub use builder::{
    materialize_latest_snapshot, BuildIndexRequest, BuildIndexResult, LocalIndexBuilder,
};
