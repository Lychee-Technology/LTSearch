pub mod generator;
#[cfg(feature = "ltembed")]
pub mod ltembed;
#[cfg(all(feature = "aws", feature = "ltembed"))]
pub mod model_assets;
pub mod provider;

pub use generator::{EmbeddingError, EmbeddingGenerator};
#[cfg(feature = "ltembed")]
pub use ltembed::{
    ltembed_config_from_env, LTEmbedConfig, LTEmbedEmbeddingGenerator, LTEmbedEngine,
};
pub use provider::{
    fixed_generator_from_env, provider_from_env_or_default, required_provider_from_env,
    EmbeddingProvider, EmbeddingProviderError, FixedEmbeddingGenerator,
};
