//! Shared composition-root helpers for the Lambda and CLI binaries.
//!
//! Every binary wires the same AWS clients and embedding generators from
//! environment variables; this module owns that scaffolding so the binaries
//! stay thin shells over the library handlers.

use std::env;

use thiserror::Error;

use crate::embedding::{
    fixed_generator_from_env, provider_from_env_or_default, EmbeddingGenerator, EmbeddingProvider,
};
#[cfg(feature = "ltembed")]
use crate::embedding::{ltembed_config_from_env, LTEmbedEmbeddingGenerator};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BootstrapError {
    #[error("missing required environment variable {name}")]
    MissingEnv { name: &'static str },
    #[error("{message}")]
    Embedding { message: String },
}

fn required_env(name: &'static str) -> Result<String, BootstrapError> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(BootstrapError::MissingEnv { name }),
    }
}

/// Configuration for the write Lambda. All fields are required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteConfig {
    pub s3_bucket: String,
    pub sqs_queue_url: String,
}

impl WriteConfig {
    pub fn from_env() -> Result<Self, BootstrapError> {
        Ok(Self {
            s3_bucket: required_env("LTSEARCH_WRITE_S3_BUCKET")?,
            sqs_queue_url: required_env("LTSEARCH_WRITE_SQS_QUEUE_URL")?,
        })
    }
}

/// Configuration for the index-builder Lambda.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildConfig {
    pub s3_bucket: String,
    pub artifact_root: String,
}

impl BuildConfig {
    pub fn from_env() -> Result<Self, BootstrapError> {
        Ok(Self {
            s3_bucket: required_env("LTSEARCH_BUILD_S3_BUCKET")?,
            artifact_root: env::var("LTSEARCH_BUILD_ARTIFACT_ROOT")
                .unwrap_or_else(|_| "/tmp/ltsearch".into()),
        })
    }
}

/// Builds an S3 client, honouring the `AWS_ENDPOINT_URL_S3` override used by
/// Moto/LocalStack test environments (which also require path-style access).
pub fn s3_client_from_env(config: &aws_config::SdkConfig) -> aws_sdk_s3::Client {
    match env::var("AWS_ENDPOINT_URL_S3") {
        Ok(endpoint_url) => {
            let s3_config = aws_sdk_s3::config::Builder::from(config)
                .endpoint_url(endpoint_url)
                .force_path_style(true)
                .build();
            aws_sdk_s3::Client::from_conf(s3_config)
        }
        Err(_) => aws_sdk_s3::Client::new(config),
    }
}

/// Builds an SQS client, honouring the `AWS_ENDPOINT_URL_SQS` override.
pub fn sqs_client_from_env(config: &aws_config::SdkConfig) -> aws_sdk_sqs::Client {
    match env::var("AWS_ENDPOINT_URL_SQS") {
        Ok(endpoint_url) => {
            let sqs_config = aws_sdk_sqs::config::Builder::from(config)
                .endpoint_url(endpoint_url)
                .build();
            aws_sdk_sqs::Client::from_conf(sqs_config)
        }
        Err(_) => aws_sdk_sqs::Client::new(config),
    }
}

/// Reads the build-side embedding provider selection
/// (`LTSEARCH_BUILD_EMBEDDING_PROVIDER`, defaulting to `fixed`).
pub fn build_embedding_provider_from_env() -> Result<EmbeddingProvider, BootstrapError> {
    provider_from_env_or_default(
        "LTSEARCH_BUILD_EMBEDDING_PROVIDER",
        EmbeddingProvider::Fixed,
    )
    .map_err(|error| BootstrapError::Embedding {
        message: error.to_string(),
    })
}

/// Constructs the build-side embedding generator from the
/// `LTSEARCH_BUILD_*` environment variables.
pub fn build_embedding_generator_from_env(
    provider: EmbeddingProvider,
) -> Result<Box<dyn EmbeddingGenerator>, BootstrapError> {
    match provider {
        EmbeddingProvider::Fixed => fixed_generator_from_env(
            "LTSEARCH_BUILD_FIXED_EMBEDDING",
            Some("LTSEARCH_BUILD_EMBEDDING_DIM"),
        )
        .map(|generator| Box::new(generator) as Box<dyn EmbeddingGenerator>)
        .map_err(|error| BootstrapError::Embedding {
            message: error.to_string(),
        }),
        #[cfg(feature = "ltembed")]
        EmbeddingProvider::LTEmbed => ltembed_config_from_env(
            "LTSEARCH_BUILD_LTEMBED_MODEL_PATH",
            "LTSEARCH_BUILD_LTEMBED_CONFIG_PATH",
            "LTSEARCH_BUILD_LTEMBED_TOKENIZER_PATH",
            "LTSEARCH_BUILD_LTEMBED_POOLING",
            "LTSEARCH_BUILD_LTEMBED_PREFIX",
        )
        .map_err(|error| BootstrapError::Embedding {
            message: error.to_string(),
        })
        .and_then(|config| {
            LTEmbedEmbeddingGenerator::from_config(&config)
                .map(|generator| Box::new(generator) as Box<dyn EmbeddingGenerator>)
                .map_err(|error| BootstrapError::Embedding {
                    message: error.to_string(),
                })
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static BOOTSTRAP_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        BOOTSTRAP_ENV_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn endpoint_overrides_are_applied_without_panicking() {
        let _guard = env_guard();
        std::env::set_var("AWS_ENDPOINT_URL_S3", "http://localhost:5000");
        std::env::set_var("AWS_ENDPOINT_URL_SQS", "http://localhost:5000");

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .load()
                .await;
            let _ = s3_client_from_env(&config);
            let _ = sqs_client_from_env(&config);
        });

        std::env::remove_var("AWS_ENDPOINT_URL_S3");
        std::env::remove_var("AWS_ENDPOINT_URL_SQS");
    }

    #[test]
    fn write_config_requires_bucket_and_queue_url() {
        let _guard = env_guard();
        std::env::remove_var("LTSEARCH_WRITE_S3_BUCKET");
        std::env::remove_var("LTSEARCH_WRITE_SQS_QUEUE_URL");

        let error = WriteConfig::from_env().unwrap_err();
        assert_eq!(
            error,
            BootstrapError::MissingEnv {
                name: "LTSEARCH_WRITE_S3_BUCKET"
            }
        );

        std::env::set_var("LTSEARCH_WRITE_S3_BUCKET", "bucket");
        let error = WriteConfig::from_env().unwrap_err();
        assert_eq!(
            error,
            BootstrapError::MissingEnv {
                name: "LTSEARCH_WRITE_SQS_QUEUE_URL"
            }
        );

        std::env::set_var("LTSEARCH_WRITE_SQS_QUEUE_URL", "http://queue");
        let config = WriteConfig::from_env().unwrap();
        assert_eq!(config.s3_bucket, "bucket");
        assert_eq!(config.sqs_queue_url, "http://queue");

        std::env::remove_var("LTSEARCH_WRITE_S3_BUCKET");
        std::env::remove_var("LTSEARCH_WRITE_SQS_QUEUE_URL");
    }

    #[test]
    fn build_config_requires_bucket_and_defaults_artifact_root() {
        let _guard = env_guard();
        std::env::remove_var("LTSEARCH_BUILD_S3_BUCKET");
        std::env::remove_var("LTSEARCH_BUILD_ARTIFACT_ROOT");

        let error = BuildConfig::from_env().unwrap_err();
        assert_eq!(
            error,
            BootstrapError::MissingEnv {
                name: "LTSEARCH_BUILD_S3_BUCKET"
            }
        );

        std::env::set_var("LTSEARCH_BUILD_S3_BUCKET", "bucket");
        let config = BuildConfig::from_env().unwrap();
        assert_eq!(config.artifact_root, "/tmp/ltsearch");

        std::env::remove_var("LTSEARCH_BUILD_S3_BUCKET");
    }

    #[cfg(feature = "ltembed")]
    mod ltembed {
        use std::path::{Path, PathBuf};

        use super::*;

        fn maybe_ltembed_assets_dir() -> Option<PathBuf> {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .map(|ancestor| ancestor.join("LTEmbed/assets"))
                .find(|candidate| {
                    candidate.join("config.json").exists()
                        && candidate.join("tokenizer.json").exists()
                        && candidate.join("model.safetensors").exists()
                })
        }

        #[test]
        fn ltembed_provider_reports_missing_model_path() {
            let _guard = env_guard();
            std::env::remove_var("LTSEARCH_BUILD_FIXED_EMBEDDING");
            std::env::remove_var("LTSEARCH_BUILD_EMBEDDING_DIM");
            std::env::remove_var("LTSEARCH_BUILD_LTEMBED_MODEL_PATH");
            std::env::remove_var("LTSEARCH_BUILD_LTEMBED_CONFIG_PATH");
            std::env::remove_var("LTSEARCH_BUILD_LTEMBED_TOKENIZER_PATH");
            std::env::remove_var("LTSEARCH_BUILD_LTEMBED_POOLING");
            std::env::remove_var("LTSEARCH_BUILD_LTEMBED_PREFIX");

            let error = match build_embedding_generator_from_env(EmbeddingProvider::LTEmbed) {
                Ok(_) => panic!("expected LTEmbed bootstrap to fail without model path"),
                Err(error) => error,
            };
            assert_eq!(
                error.to_string(),
                "missing LTSEARCH_BUILD_LTEMBED_MODEL_PATH"
            );
        }

        #[test]
        fn ltembed_provider_reports_unsupported_pooling_mode() {
            let _guard = env_guard();
            std::env::set_var(
                "LTSEARCH_BUILD_LTEMBED_MODEL_PATH",
                "/tmp/model.safetensors",
            );
            std::env::set_var("LTSEARCH_BUILD_LTEMBED_CONFIG_PATH", "/tmp/config.json");
            std::env::set_var(
                "LTSEARCH_BUILD_LTEMBED_TOKENIZER_PATH",
                "/tmp/tokenizer.json",
            );
            std::env::set_var("LTSEARCH_BUILD_LTEMBED_POOLING", "median");
            std::env::remove_var("LTSEARCH_BUILD_LTEMBED_PREFIX");

            let error = match build_embedding_generator_from_env(EmbeddingProvider::LTEmbed) {
                Ok(_) => panic!("expected LTEmbed bootstrap to reject pooling mode"),
                Err(error) => error,
            };
            assert_eq!(
                error.to_string(),
                "unsupported LTEmbed pooling mode: median"
            );
        }

        #[test]
        fn ltembed_provider_builds_embedding_generator_when_assets_are_available() {
            let _guard = env_guard();
            let Some(assets_dir) = maybe_ltembed_assets_dir() else {
                eprintln!("Skipping: LTEmbed assets not found in sibling checkout");
                return;
            };

            std::env::set_var(
                "LTSEARCH_BUILD_LTEMBED_MODEL_PATH",
                assets_dir.join("model.safetensors"),
            );
            std::env::set_var(
                "LTSEARCH_BUILD_LTEMBED_CONFIG_PATH",
                assets_dir.join("config.json"),
            );
            std::env::set_var(
                "LTSEARCH_BUILD_LTEMBED_TOKENIZER_PATH",
                assets_dir.join("tokenizer.json"),
            );
            std::env::set_var("LTSEARCH_BUILD_LTEMBED_POOLING", "mean");
            std::env::set_var("LTSEARCH_BUILD_LTEMBED_PREFIX", "passage:");

            let generator = build_embedding_generator_from_env(EmbeddingProvider::LTEmbed)
                .expect("expected LTEmbed bootstrap to construct generator");
            let embedding = generator
                .generate("rust search document")
                .expect("expected LTEmbed generator to produce an embedding");

            assert_eq!(embedding.len(), 384);
        }
    }
}
