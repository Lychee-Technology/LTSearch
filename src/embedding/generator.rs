use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EmbeddingError {
    #[error("embedding generation failed: {message}")]
    Generation { message: String },
}

pub trait EmbeddingGenerator: Send + Sync {
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError>;
}
