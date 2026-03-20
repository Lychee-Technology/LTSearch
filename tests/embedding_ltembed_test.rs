#![cfg(feature = "ltembed")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ltembed::error::LTEmbedError;
use ltsearch::embedding::{
    EmbeddingError, EmbeddingGenerator, LTEmbedConfig, LTEmbedEmbeddingGenerator, LTEmbedEngine,
    LTEmbedPoolingMode,
};

#[derive(Debug)]
enum StubResult {
    Success(Vec<f32>),
    Failure(String),
}

#[derive(Debug)]
struct StubEngine {
    seen_inputs: Arc<Mutex<Vec<String>>>,
    result: StubResult,
}

impl StubEngine {
    fn success(seen_inputs: Arc<Mutex<Vec<String>>>, embedding: Vec<f32>) -> Self {
        Self {
            seen_inputs,
            result: StubResult::Success(embedding),
        }
    }

    fn failure(seen_inputs: Arc<Mutex<Vec<String>>>, error: LTEmbedError) -> Self {
        Self {
            seen_inputs,
            result: StubResult::Failure(error.to_string()),
        }
    }
}

impl LTEmbedEngine for StubEngine {
    fn embed(&self, text: &str) -> Result<Vec<f32>, LTEmbedError> {
        self.seen_inputs.lock().unwrap().push(text.to_string());
        match &self.result {
            StubResult::Success(embedding) => Ok(embedding.clone()),
            StubResult::Failure(message) => Err(LTEmbedError::Inference(message.clone())),
        }
    }
}

fn temp_path(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("ltsearch-{name}-{}-{unique}", std::process::id()))
}

#[test]
fn ltembed_generator_applies_prefix_before_embedding() {
    let seen_inputs = Arc::new(Mutex::new(Vec::new()));
    let generator = LTEmbedEmbeddingGenerator::new_for_tests(
        StubEngine::success(seen_inputs.clone(), vec![0.1, 0.2, 0.3]),
        Some("query:".into()),
    );

    let embedding = generator.generate("hello world").unwrap();

    assert_eq!(embedding, vec![0.1, 0.2, 0.3]);
    assert_eq!(
        seen_inputs.lock().unwrap().as_slice(),
        ["query: hello world"]
    );
}

#[test]
fn ltembed_generator_leaves_text_unchanged_without_prefix() {
    let seen_inputs = Arc::new(Mutex::new(Vec::new()));
    let generator = LTEmbedEmbeddingGenerator::new_for_tests(
        StubEngine::success(seen_inputs.clone(), vec![0.4, 0.5, 0.6]),
        None,
    );

    let embedding = generator.generate("plain text").unwrap();

    assert_eq!(embedding, vec![0.4, 0.5, 0.6]);
    assert_eq!(seen_inputs.lock().unwrap().as_slice(), ["plain text"]);
}

#[test]
fn ltembed_generator_maps_engine_errors_to_embedding_error() {
    let seen_inputs = Arc::new(Mutex::new(Vec::new()));
    let generator = LTEmbedEmbeddingGenerator::new_for_tests(
        StubEngine::failure(
            seen_inputs,
            LTEmbedError::Inference("bad hidden state".into()),
        ),
        Some("query:".into()),
    );

    let error = generator.generate("broken input").unwrap_err();

    assert_eq!(
        error,
        EmbeddingError::Generation {
            message:
                "LTEmbed embedding failed: Inference failed: Inference failed: bad hidden state"
                    .into()
        }
    );
}

#[test]
fn pooling_mode_parses_supported_values() {
    assert_eq!(
        "mean".parse::<LTEmbedPoolingMode>().unwrap(),
        LTEmbedPoolingMode::Mean
    );
    assert_eq!(
        "cls".parse::<LTEmbedPoolingMode>().unwrap(),
        LTEmbedPoolingMode::Cls
    );
}

#[test]
fn pooling_mode_rejects_unsupported_values() {
    let error = "median".parse::<LTEmbedPoolingMode>().unwrap_err();

    assert_eq!(
        error.to_string(),
        "unsupported LTEmbed pooling mode: median"
    );
}

#[test]
fn ltembed_generator_from_config_maps_bootstrap_failures() {
    let config = LTEmbedConfig {
        model_path: temp_path("missing-model").display().to_string(),
        config_path: temp_path("missing-config").display().to_string(),
        tokenizer_path: temp_path("missing-tokenizer").display().to_string(),
        pooling: LTEmbedPoolingMode::Mean,
        prefix: Some("query:".into()),
    };

    let error = LTEmbedEmbeddingGenerator::from_config(&config).unwrap_err();

    assert_eq!(
        error,
        EmbeddingError::Generation {
            message: "LTEmbed bootstrap failed: I/O error: No such file or directory (os error 2)"
                .to_string()
        }
    );
}
