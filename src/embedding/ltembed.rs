use std::env;
use std::fmt;
use std::fs;
use std::str::FromStr;

use ltembed::engine::ZeroVecEngine;
use ltembed::error::LTEmbedError;
use ltembed::traits::pooling::{CLSPooling, MeanPooling, Pooling};
use thiserror::Error;

use crate::embedding::{EmbeddingError, EmbeddingGenerator, EmbeddingProviderError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LTEmbedConfig {
    pub model_path: String,
    pub config_path: String,
    pub tokenizer_path: String,
    pub pooling: LTEmbedPoolingMode,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LTEmbedPoolingMode {
    Mean,
    Cls,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unsupported LTEmbed pooling mode: {value}")]
pub struct LTEmbedPoolingModeParseError {
    value: String,
}

impl FromStr for LTEmbedPoolingMode {
    type Err = LTEmbedPoolingModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "mean" => Ok(Self::Mean),
            "cls" => Ok(Self::Cls),
            _ => Err(LTEmbedPoolingModeParseError {
                value: value.to_string(),
            }),
        }
    }
}

pub trait LTEmbedEngine: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>, LTEmbedError>;
}

impl LTEmbedEngine for ZeroVecEngine {
    fn embed(&self, text: &str) -> Result<Vec<f32>, LTEmbedError> {
        ZeroVecEngine::embed(self, text)
    }
}

pub struct LTEmbedEmbeddingGenerator<E = ZeroVecEngine> {
    engine: E,
    prefix: Option<String>,
}

impl<E> fmt::Debug for LTEmbedEmbeddingGenerator<E>
where
    E: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LTEmbedEmbeddingGenerator")
            .field("engine", &self.engine)
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl LTEmbedEmbeddingGenerator<ZeroVecEngine> {
    pub fn from_config(config: &LTEmbedConfig) -> Result<Self, EmbeddingError> {
        let config_json = fs::read_to_string(&config.config_path).map_err(|error| {
            EmbeddingError::Generation {
                message: format!("LTEmbed bootstrap failed: I/O error: {error}"),
            }
        })?;

        let engine = ZeroVecEngine::new(
            &config.model_path,
            &config_json,
            &config.tokenizer_path,
            config.pooling.build_pooling(),
        )
        .map_err(|error| EmbeddingError::Generation {
            message: format!("LTEmbed bootstrap failed: {error}"),
        })?;

        Ok(Self {
            engine,
            prefix: normalize_prefix(config.prefix.clone()),
        })
    }
}

pub fn ltembed_config_from_env(
    model_var: &str,
    config_var: &str,
    tokenizer_var: &str,
    pooling_var: &str,
    prefix_var: &str,
) -> Result<LTEmbedConfig, EmbeddingProviderError> {
    let model_path = env::var(model_var).map_err(|_| EmbeddingProviderError::Config {
        message: format!("missing {model_var}"),
    })?;
    let config_path = env::var(config_var).map_err(|_| EmbeddingProviderError::Config {
        message: format!("missing {config_var}"),
    })?;
    let tokenizer_path = env::var(tokenizer_var).map_err(|_| EmbeddingProviderError::Config {
        message: format!("missing {tokenizer_var}"),
    })?;
    let pooling = env::var(pooling_var)
        .map_err(|_| EmbeddingProviderError::Config {
            message: format!("missing {pooling_var}"),
        })?
        .parse::<LTEmbedPoolingMode>()
        .map_err(|error| EmbeddingProviderError::Config {
            message: error.to_string(),
        })?;
    let prefix = env::var(prefix_var).ok();

    Ok(LTEmbedConfig {
        model_path,
        config_path,
        tokenizer_path,
        pooling,
        prefix,
    })
}

impl<E> LTEmbedEmbeddingGenerator<E>
where
    E: LTEmbedEngine,
{
    pub fn new_for_tests(engine: E, prefix: Option<String>) -> Self {
        Self {
            engine,
            prefix: normalize_prefix(prefix),
        }
    }
}

impl<E> EmbeddingGenerator for LTEmbedEmbeddingGenerator<E>
where
    E: LTEmbedEngine,
{
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        let input = apply_prefix(query, self.prefix.as_deref());
        self.engine
            .embed(&input)
            .map_err(|error| EmbeddingError::Generation {
                message: format!("LTEmbed embedding failed: {error}"),
            })
    }
}

impl LTEmbedPoolingMode {
    fn build_pooling(self) -> Box<dyn Pooling> {
        match self {
            Self::Mean => Box::new(MeanPooling),
            Self::Cls => Box::new(CLSPooling),
        }
    }
}

fn normalize_prefix(prefix: Option<String>) -> Option<String> {
    prefix.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(format!("{trimmed} "))
        }
    })
}

fn apply_prefix(text: &str, prefix: Option<&str>) -> String {
    match prefix {
        Some(prefix) => format!("{prefix}{text}"),
        None => text.to_string(),
    }
}
