use std::env;
use std::fmt;

use ltembed::engine::{EmbeddingEngine, EmbeddingInput, EmbeddingInputKind, EngineConfig};
use ltembed::error::LTEmbedError;

use crate::embedding::{EmbeddingError, EmbeddingGenerator, EmbeddingProviderError};

/// Filesystem location of an LTEmbed GGUF bundle: `bundle_dir` holds
/// `model.gguf` + `tokenizer.json` + `build-info.json`. llama.cpp is statically
/// linked into the binary, so the bundle carries no runtime library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LTEmbedConfig {
    pub bundle_dir: String,
}

pub trait LTEmbedEngine: Send + Sync {
    fn embed(&self, input: EmbeddingInput<'_>) -> Result<Vec<f32>, LTEmbedError>;
}

impl LTEmbedEngine for EmbeddingEngine {
    fn embed(&self, input: EmbeddingInput<'_>) -> Result<Vec<f32>, LTEmbedError> {
        EmbeddingEngine::embed(self, input)
    }
}

/// Prefixing (`Query: ` / `Document: `), pooling, and Matryoshka truncation
/// are owned by the LTEmbed engine; this generator only tags each text with
/// the input kind of its side (build = Document, query = Query).
pub struct LTEmbedEmbeddingGenerator<E = EmbeddingEngine> {
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

impl LTEmbedEmbeddingGenerator<EmbeddingEngine> {
    pub fn from_config(
        config: &LTEmbedConfig,
        input_kind: EmbeddingInputKind,
    ) -> Result<Self, EmbeddingError> {
        // 预检 bundle 目录：Lambda ZIP 部署下资产由 S3→/tmp 冷启动供给
        // （src/embedding/model_assets.rs），镜像/挂载部署则预置在容器内。
        // 目录缺失把供给这层原因直接说出来；目录存在但缺文件（model.gguf /
        // tokenizer.json / build-info.json）交给 LTEmbed 的 require_file，
        // 它返回带完整路径的 MissingFile。
        let bundle_dir = config.bundle_dir.as_str();
        if !std::path::Path::new(bundle_dir).exists() {
            return Err(EmbeddingError::Generation {
                message: format!(
                    "LTEmbed bundle dir not found at '{bundle_dir}' — model assets not provisioned \
                     (ZIP deployments download them from S3 at cold start: check \
                     LTSEARCH_*_LTEMBED_S3_BUCKET/_S3_PREFIX and startup logs; \
                     image/mount deployments must pre-place the bundle)"
                ),
            });
        }
        // EngineConfig::default() = 512 维 Matryoshka 截断 + L2 归一化；
        // from_gguf_bundle_dir 用单个 llama.cpp 线程（与此前 ORT intra_threads=1 一致）。
        let engine = EmbeddingEngine::from_gguf_bundle_dir(bundle_dir, EngineConfig::default())
            .map_err(|error| EmbeddingError::Generation {
                message: format!(
                    "LTEmbed bootstrap failed for bundle_dir '{}': {error} — \
                 verify it is a GGUF bundle (model.gguf + tokenizer.json + build-info.json) \
                 and is not corrupt",
                    config.bundle_dir
                ),
            })?;

        Ok(Self { engine, input_kind })
    }
}

pub fn ltembed_config_from_env(bundle_var: &str) -> Result<LTEmbedConfig, EmbeddingProviderError> {
    let bundle_dir = env::var(bundle_var).map_err(|_| EmbeddingProviderError::Config {
        message: format!("missing {bundle_var}"),
    })?;

    Ok(LTEmbedConfig { bundle_dir })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_reports_unprovisioned_assets() {
        let config = LTEmbedConfig {
            bundle_dir: "/tmp/ltembed-nonexistent-test".to_string(),
        };
        let Err(error) = LTEmbedEmbeddingGenerator::from_config(&config, EmbeddingInputKind::Query)
        else {
            panic!("missing bundle dir must fail");
        };
        let message = error.to_string();
        assert!(
            message.contains("/tmp/ltembed-nonexistent-test"),
            "{message}"
        );
        assert!(
            message.contains("model assets not provisioned"),
            "{message}"
        );
        assert!(message.contains("LTEMBED_S3_BUCKET"), "{message}");
    }
}
