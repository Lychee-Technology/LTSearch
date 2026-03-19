pub mod generator;
pub mod ltembed;
pub mod provider;

pub struct ModuleBoundary;

pub use generator::{EmbeddingError, EmbeddingGenerator};
pub use ltembed::{
    LTEmbedConfig, LTEmbedEmbeddingGenerator, LTEmbedEngine, LTEmbedPoolingMode,
    LTEmbedPoolingModeParseError,
};
pub use provider::{
    fixed_generator_from_env, provider_from_env_or_default, required_provider_from_env,
    EmbeddingProvider, EmbeddingProviderError, FixedEmbeddingGenerator,
};
