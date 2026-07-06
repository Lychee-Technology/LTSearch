use std::env;
use std::fmt;

use ltembed::engine::{EmbeddingInput, EmbeddingInputKind, OnnxEngine, OnnxEngineConfig};
use ltembed::error::LTEmbedError;

use crate::embedding::{EmbeddingError, EmbeddingGenerator, EmbeddingProviderError};

/// Filesystem locations of an LTEmbed ort bundle: `bundle_dir` holds
/// `tokenizer.json` + `build-info.json` (and optionally `libonnxruntime.so`),
/// while `model_path` points at the `model.ort` weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LTEmbedConfig {
    pub bundle_dir: String,
    pub model_path: String,
}

pub trait LTEmbedEngine: Send + Sync {
    fn embed(&self, input: EmbeddingInput<'_>) -> Result<Vec<f32>, LTEmbedError>;
}

impl LTEmbedEngine for OnnxEngine {
    fn embed(&self, input: EmbeddingInput<'_>) -> Result<Vec<f32>, LTEmbedError> {
        OnnxEngine::embed(self, input)
    }
}

/// Prefixing (`Query: ` / `Document: `), pooling, and Matryoshka truncation
/// are owned by the LTEmbed engine; this generator only tags each text with
/// the input kind of its side (build = Document, query = Query).
pub struct LTEmbedEmbeddingGenerator<E = OnnxEngine> {
    engine: E,
    input_kind: EmbeddingInputKind,
}

impl<E> fmt::Debug for LTEmbedEmbeddingGenerator<E>
where
    E: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LTEmbedEmbeddingGenerator")
            .field("engine", &self.engine)
            .field("input_kind", &self.input_kind)
            .finish()
    }
}

impl LTEmbedEmbeddingGenerator<OnnxEngine> {
    pub fn from_config(
        config: &LTEmbedConfig,
        input_kind: EmbeddingInputKind,
    ) -> Result<Self, EmbeddingError> {
        let engine = OnnxEngine::from_bundle_dir(
            &config.bundle_dir,
            &config.model_path,
            OnnxEngineConfig::default(),
        )
        .map_err(|error| EmbeddingError::Generation {
            message: format!("LTEmbed bootstrap failed: {error}"),
        })?;

        Ok(Self { engine, input_kind })
    }
}

pub fn ltembed_config_from_env(
    bundle_var: &str,
    model_var: &str,
) -> Result<LTEmbedConfig, EmbeddingProviderError> {
    let bundle_dir = env::var(bundle_var).map_err(|_| EmbeddingProviderError::Config {
        message: format!("missing {bundle_var}"),
    })?;
    let model_path = env::var(model_var).map_err(|_| EmbeddingProviderError::Config {
        message: format!("missing {model_var}"),
    })?;

    Ok(LTEmbedConfig {
        bundle_dir,
        model_path,
    })
}

impl<E> LTEmbedEmbeddingGenerator<E>
where
    E: LTEmbedEngine,
{
    pub fn new_for_tests(engine: E, input_kind: EmbeddingInputKind) -> Self {
        Self { engine, input_kind }
    }
}

impl<E> EmbeddingGenerator for LTEmbedEmbeddingGenerator<E>
where
    E: LTEmbedEngine,
{
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        let input = EmbeddingInput {
            text: query,
            kind: self.input_kind,
        };
        self.engine
            .embed(input)
            .map_err(|error| EmbeddingError::Generation {
                message: format!("LTEmbed embedding failed: {error}"),
            })
    }
}
