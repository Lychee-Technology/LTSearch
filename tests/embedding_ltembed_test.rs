#![cfg(feature = "ltembed")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ltembed::engine::{EmbeddingInput, EmbeddingInputKind};
use ltembed::error::{InferenceError, LTEmbedError};
use ltsearch::embedding::{
    EmbeddingError, EmbeddingGenerator, LTEmbedConfig, LTEmbedEmbeddingGenerator, LTEmbedEngine,
};

#[derive(Debug, Clone, PartialEq)]
struct SeenInput {
    text: String,
    kind: EmbeddingInputKind,
}

#[derive(Debug)]
enum StubResult {
    Success(Vec<f32>),
    Failure,
}

#[derive(Debug)]
struct StubEngine {
    seen_inputs: Arc<Mutex<Vec<SeenInput>>>,
    result: StubResult,
}

impl StubEngine {
    fn success(seen_inputs: Arc<Mutex<Vec<SeenInput>>>, embedding: Vec<f32>) -> Self {
        Self {
            seen_inputs,
            result: StubResult::Success(embedding),
        }
    }

    fn failure(seen_inputs: Arc<Mutex<Vec<SeenInput>>>) -> Self {
        Self {
            seen_inputs,
            result: StubResult::Failure,
        }
    }
}

impl LTEmbedEngine for StubEngine {
    fn embed(&self, input: EmbeddingInput<'_>) -> Result<Vec<f32>, LTEmbedError> {
        self.seen_inputs.lock().unwrap().push(SeenInput {
            text: input.text.to_string(),
            kind: input.kind,
        });
        match &self.result {
            StubResult::Success(embedding) => Ok(embedding.clone()),
            StubResult::Failure => Err(LTEmbedError::Inference(InferenceError::Internal(
                "bad hidden state".into(),
            ))),
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
fn ltembed_generator_passes_query_kind_without_manual_prefix() {
    let seen_inputs = Arc::new(Mutex::new(Vec::new()));
    let generator = LTEmbedEmbeddingGenerator::new_for_tests(
        StubEngine::success(seen_inputs.clone(), vec![0.1, 0.2, 0.3]),
        EmbeddingInputKind::Query,
    );

    let embedding = generator.generate("hello world").unwrap();

    assert_eq!(embedding, vec![0.1, 0.2, 0.3]);
    // Prefixing is owned by the LTEmbed engine — the generator must pass the
    // text through untouched and tag it with the configured input kind.
    assert_eq!(
        seen_inputs.lock().unwrap().as_slice(),
        [SeenInput {
            text: "hello world".into(),
            kind: EmbeddingInputKind::Query,
        }]
    );
}

#[test]
fn ltembed_generator_passes_document_kind_for_build_side() {
    let seen_inputs = Arc::new(Mutex::new(Vec::new()));
    let generator = LTEmbedEmbeddingGenerator::new_for_tests(
        StubEngine::success(seen_inputs.clone(), vec![0.4, 0.5, 0.6]),
        EmbeddingInputKind::Document,
    );

    let embedding = generator.generate("chunk text").unwrap();

    assert_eq!(embedding, vec![0.4, 0.5, 0.6]);
    assert_eq!(
        seen_inputs.lock().unwrap().as_slice(),
        [SeenInput {
            text: "chunk text".into(),
            kind: EmbeddingInputKind::Document,
        }]
    );
}

#[test]
fn ltembed_generator_maps_engine_errors_to_embedding_error() {
    let seen_inputs = Arc::new(Mutex::new(Vec::new()));
    let generator = LTEmbedEmbeddingGenerator::new_for_tests(
        StubEngine::failure(seen_inputs),
        EmbeddingInputKind::Query,
    );

    let error = generator.generate("broken input").unwrap_err();

    let EmbeddingError::Generation { message } = error;
    assert!(
        message.starts_with("LTEmbed embedding failed:"),
        "unexpected message: {message}"
    );
    assert!(
        message.contains("bad hidden state"),
        "unexpected message: {message}"
    );
}

#[test]
fn ltembed_generator_from_config_maps_bootstrap_failures() {
    let config = LTEmbedConfig {
        bundle_dir: temp_path("missing-bundle").display().to_string(),
        model_path: temp_path("missing-model.ort").display().to_string(),
    };

    let error =
        LTEmbedEmbeddingGenerator::from_config(&config, EmbeddingInputKind::Query).unwrap_err();

    let EmbeddingError::Generation { message } = error;
    assert!(
        message.starts_with("LTEmbed bootstrap failed:"),
        "unexpected message: {message}"
    );
}
