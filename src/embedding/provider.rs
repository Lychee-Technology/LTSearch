use std::env;

use thiserror::Error;

use crate::embedding::{EmbeddingError, EmbeddingGenerator};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProvider {
    Fixed,
    LTEmbed,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EmbeddingProviderError {
    #[error("{message}")]
    Config { message: String },
}

pub fn required_provider_from_env(
    provider_var: &str,
) -> Result<EmbeddingProvider, EmbeddingProviderError> {
    let provider = env::var(provider_var).map_err(|_| EmbeddingProviderError::Config {
        message: format!("missing {provider_var}"),
    })?;
    parse_provider(provider_var, &provider)
}

pub fn provider_from_env_or_default(
    provider_var: &str,
    default: EmbeddingProvider,
) -> Result<EmbeddingProvider, EmbeddingProviderError> {
    match env::var(provider_var) {
        Ok(provider) => parse_provider(provider_var, &provider),
        Err(_) => Ok(default),
    }
}

pub fn fixed_generator_from_env(
    embedding_var: &str,
    dim_var: Option<&str>,
) -> Result<FixedEmbeddingGenerator, EmbeddingProviderError> {
    let embedding_str = env::var(embedding_var).map_err(|_| EmbeddingProviderError::Config {
        message: format!("missing {embedding_var}"),
    })?;
    let embedding = parse_fixed_embedding(&embedding_str, embedding_var)?;

    if let Some(dim_var) = dim_var {
        let dim: usize = env::var(dim_var)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);

        if dim > 0 && embedding.len() != dim {
            return Err(EmbeddingProviderError::Config {
                message: format!(
                    "{embedding_var} dimension {} does not match {dim_var} {dim}",
                    embedding.len()
                ),
            });
        }
    }

    Ok(FixedEmbeddingGenerator::new(embedding))
}

fn parse_provider(
    provider_var: &str,
    provider: &str,
) -> Result<EmbeddingProvider, EmbeddingProviderError> {
    match provider {
        "fixed" => Ok(EmbeddingProvider::Fixed),
        "ltembed" => Ok(EmbeddingProvider::LTEmbed),
        _ => Err(EmbeddingProviderError::Config {
            message: format!("unsupported {provider_var}: {provider}"),
        }),
    }
}

fn parse_fixed_embedding(
    value: &str,
    embedding_var: &str,
) -> Result<Vec<f32>, EmbeddingProviderError> {
    let mut embedding = Vec::new();

    for part in value.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return Err(EmbeddingProviderError::Config {
                message: format!("{embedding_var} must be a comma-separated list of numbers"),
            });
        }

        let parsed = trimmed
            .parse::<f32>()
            .map_err(|_| EmbeddingProviderError::Config {
                message: format!("{embedding_var} must be a comma-separated list of numbers"),
            })?;
        if !parsed.is_finite() {
            return Err(EmbeddingProviderError::Config {
                message: format!("{embedding_var} must contain only finite numbers"),
            });
        }

        embedding.push(parsed);
    }

    if embedding.is_empty() {
        return Err(EmbeddingProviderError::Config {
            message: format!("{embedding_var} must not be empty"),
        });
    }

    Ok(embedding)
}

#[derive(Debug, Clone)]
pub struct FixedEmbeddingGenerator {
    embedding: Vec<f32>,
}

impl FixedEmbeddingGenerator {
    pub fn new(embedding: Vec<f32>) -> Self {
        Self { embedding }
    }
}

impl EmbeddingGenerator for FixedEmbeddingGenerator {
    fn generate(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }
}
