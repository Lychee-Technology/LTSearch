use thiserror::Error;

pub struct ModuleBoundary;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("{field} is required")]
    Required { field: &'static str },
    #[error("{field} must be between {min} and {max}")]
    LengthOutOfRange {
        field: &'static str,
        min: usize,
        max: usize,
    },
    #[error("{field} must be between {min} and {max}")]
    RangeOutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
    },
    #[error("{field} must be positive")]
    MustBePositive { field: &'static str },
    #[error("{field} has an invalid value")]
    InvalidValue { field: &'static str },
    #[error("{field} must match {expected}")]
    Mismatch {
        field: &'static str,
        expected: &'static str,
    },
    #[error("{field} exceeds the maximum size of {max_bytes} bytes")]
    TooLarge {
        field: &'static str,
        max_bytes: usize,
    },
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("search execution failed: {message}")]
    Execution { message: String },
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("index operation failed: {message}")]
    Operation { message: String },
}

#[derive(Debug, Error)]
pub enum PublishError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("publish operation failed: {message}")]
    Operation { message: String },
}

#[derive(Debug, Error)]
pub enum IngestError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("ingest operation failed: {message}")]
    Operation { message: String },
}
