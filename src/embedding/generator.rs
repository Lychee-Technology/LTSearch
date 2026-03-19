use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EmbeddingError {
    #[error("embedding generation failed: {message}")]
    Generation { message: String },
}

pub trait EmbeddingGenerator: Send + Sync {
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError>;
}

impl<T> EmbeddingGenerator for Box<T>
where
    T: EmbeddingGenerator + ?Sized,
{
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        (**self).generate(query)
    }
}
