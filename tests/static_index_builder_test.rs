use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::index::{MmapIndex, StaticChunk, StaticIndexBuilder};
use ltsearch::models::CorpusType;
use serde_json::json;

type EmbeddingResponses = VecDeque<(String, Vec<f32>)>;

#[derive(Clone, Debug)]
struct StubEmbeddingGenerator {
    calls: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<EmbeddingResponses>>,
}

impl StubEmbeddingGenerator {
    fn from_pairs(pairs: Vec<(&str, Vec<f32>)>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(
                pairs
                    .into_iter()
                    .map(|(text, embedding)| (text.to_string(), embedding))
                    .collect(),
            )),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl EmbeddingGenerator for StubEmbeddingGenerator {
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.calls.lock().unwrap().push(query.to_string());
        let (expected_query, embedding) = self.responses.lock().unwrap().pop_front().unwrap();
        assert_eq!(query, expected_query);
        Ok(embedding)
    }
}

fn temp_fixture_dir(test_name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ltsearch-{test_name}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn static_index_builder_generates_missing_embeddings_and_writes_loadable_artifacts() {
    let output = temp_fixture_dir("static-index-builder-build");
    let generator = StubEmbeddingGenerator::from_pairs(vec![("beta body", vec![0.0, 1.0, 0.0])]);
    let builder = StaticIndexBuilder::new();

    let result = builder
        .build(
            &output,
            &[
                StaticChunk {
                    doc_id: "1001".into(),
                    text: "alpha body".into(),
                    metadata: HashMap::from([("lang".into(), json!("en"))]),
                    corpus_type: CorpusType::Legal,
                },
                StaticChunk {
                    doc_id: "1002".into(),
                    text: "beta body".into(),
                    metadata: HashMap::new(),
                    corpus_type: CorpusType::Rfc,
                },
            ],
            &[Some(vec![1.0, 0.0, 0.0]), None],
            &generator,
        )
        .unwrap();

    assert_eq!(generator.calls(), vec!["beta body"]);
    assert_eq!(result.record_count, 2);
    assert_eq!(result.embedding_dim, 3);
    assert!(output.join("centroids.bin").is_file());
    assert!(output.join("projection.bin").is_file());
    assert!(output.join("turbo_static.bin").is_file());
    assert!(output.join("turbo_static_meta.bin").is_file());
    assert!(output.join("turbo_static_text.bin").is_file());

    let index = MmapIndex::load(&output).unwrap();
    assert_eq!(index.record_count(), 2);
    assert_eq!(index.dim(), 3);
    assert_ne!(index.record(0).doc_id(), index.record(1).doc_id());
    assert_eq!(index.record(0).doc_id(), index.meta(0).doc_id);
    assert_eq!(index.record(1).doc_id(), index.meta(1).doc_id);
    assert_eq!(index.meta(0).corpus_type, 0);
    assert_eq!(index.meta(1).corpus_type, 2);
    assert_eq!(index.text(0), "alpha body");
    assert_eq!(index.text(1), "beta body");
}

#[test]
fn static_index_builder_is_deterministic_for_identical_inputs() {
    let output_a = temp_fixture_dir("static-index-builder-deterministic-a");
    let output_b = temp_fixture_dir("static-index-builder-deterministic-b");
    let chunks = vec![StaticChunk {
        doc_id: "2001".into(),
        text: "gamma body".into(),
        metadata: HashMap::new(),
        corpus_type: CorpusType::Contract,
    }];
    let embeddings = vec![Some(vec![0.2, 0.4, 0.6])];

    StaticIndexBuilder::new()
        .build(
            &output_a,
            &chunks,
            &embeddings,
            &StubEmbeddingGenerator::from_pairs(vec![]),
        )
        .unwrap();
    StaticIndexBuilder::new()
        .build(
            &output_b,
            &chunks,
            &embeddings,
            &StubEmbeddingGenerator::from_pairs(vec![]),
        )
        .unwrap();

    for file_name in [
        "centroids.bin",
        "projection.bin",
        "turbo_static.bin",
        "turbo_static_meta.bin",
        "turbo_static_text.bin",
    ] {
        assert_eq!(
            fs::read(output_a.join(file_name)).unwrap(),
            fs::read(output_b.join(file_name)).unwrap(),
            "mismatch for {file_name}"
        );
    }
}

#[test]
fn static_index_builder_hashes_string_doc_ids_stably_without_numeric_aliasing() {
    let output = temp_fixture_dir("static-index-builder-string-doc-ids");
    let builder = StaticIndexBuilder::new();

    builder
        .build(
            &output,
            &[
                StaticChunk {
                    doc_id: "1".into(),
                    text: "one".into(),
                    metadata: HashMap::new(),
                    corpus_type: CorpusType::Legal,
                },
                StaticChunk {
                    doc_id: "001".into(),
                    text: "zero zero one".into(),
                    metadata: HashMap::new(),
                    corpus_type: CorpusType::Contract,
                },
            ],
            &[Some(vec![1.0, 0.0, 0.0]), Some(vec![0.0, 1.0, 0.0])],
            &StubEmbeddingGenerator::from_pairs(vec![]),
        )
        .unwrap();

    let index = MmapIndex::load(&output).unwrap();
    assert_ne!(index.record(0).doc_id(), index.record(1).doc_id());
}
