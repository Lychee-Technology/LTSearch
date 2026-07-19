use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::index::{MmapIndex, StaticChunk, StaticIndexBuilder, TurboHeader, TurboRecord512};
use ltsearch::models::{CorpusType, IndexManifest, ShardManifest};
use ltsearch::query::{ContextBuilder, StaticRetriever, TurboQuantSearcher};
use ltsearch::storage::{ActiveManifest, ManifestHead};
use serde_json::json;

fn stub_manifest() -> ActiveManifest {
    ActiveManifest {
        head: ManifestHead {
            version_id: 1,
            manifest_path: "m.json".into(),
            updated_at: 0,
        },
        manifest: IndexManifest {
            version_id: 1,
            created_at: 0,
            embedding_dim: 512,
            document_count: 0,
            num_shards: 0,
            shards: vec![ShardManifest {
                shard_id: 0,
                document_count: 0,
                lance_path: String::new(),
                tantivy_path: String::new(),
            }],
        },
    }
}

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
    let generator = StubEmbeddingGenerator::from_pairs(vec![("beta body", vec![0.0; 512])]);
    let builder = StaticIndexBuilder::new();

    let result = builder
        .build(
            &output,
            &[
                StaticChunk {
                    doc_id: "1001".into(),
                    text: "alpha body".into(),
                    metadata: HashMap::from([
                        ("lang".into(), json!("en")),
                        ("title".into(), json!("民法典")),
                    ]),
                    corpus_type: CorpusType::Legal,
                },
                StaticChunk {
                    doc_id: "1002".into(),
                    text: "beta body".into(),
                    metadata: HashMap::new(),
                    corpus_type: CorpusType::Rfc,
                },
            ],
            &[Some(vec![1.0; 512]), None],
            &generator,
        )
        .unwrap();

    assert_eq!(generator.calls(), vec!["beta body"]);
    assert_eq!(result.record_count, 2);
    assert_eq!(result.embedding_dim, 512);
    assert!(output.join("centroids.bin").is_file());
    assert!(output.join("projection.bin").is_file());
    assert!(output.join("turbo_static.bin").is_file());
    assert!(output.join("turbo_static_meta.bin").is_file());
    assert!(output.join("turbo_static_text.bin").is_file());
    assert!(output.join("turbo_static_title.bin").is_file());

    let index = MmapIndex::load(&output).unwrap();
    assert_eq!(index.record_count(), 2);
    assert_eq!(index.dim(), 512);
    assert_ne!(index.record(0).doc_id(), index.record(1).doc_id());
    assert_eq!(index.record(0).doc_id(), index.meta(0).doc_id);
    assert_eq!(index.record(1).doc_id(), index.meta(1).doc_id);
    assert_eq!(index.meta(0).corpus_type, 0);
    assert_eq!(index.meta(1).corpus_type, 2);
    assert_eq!(index.text(0), "alpha body");
    assert_eq!(index.text(1), "beta body");
    // Chunk 1001 carried metadata["title"]; chunk 1002 had none.
    assert_eq!(index.title(0), Some("民法典"));
    assert_eq!(index.title(1), None);
    assert_eq!(index.meta(0).title_len as usize, "民法典".len());
    assert_eq!(index.meta(1).title_len, 0);
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
    let embeddings = vec![Some(vec![0.2; 512])];

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
        "turbo_static_title.bin",
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
            &[Some(vec![1.0; 512]), Some(vec![0.0; 512])],
            &StubEmbeddingGenerator::from_pairs(vec![]),
        )
        .unwrap();

    let index = MmapIndex::load(&output).unwrap();
    assert_ne!(index.record(0).doc_id(), index.record(1).doc_id());
}

#[test]
fn static_index_builder_writes_aligned_turbo_record_512_layout() {
    let output = temp_fixture_dir("static-index-builder-aligned-layout");

    StaticIndexBuilder::new()
        .build(
            &output,
            &[StaticChunk {
                doc_id: "1001".into(),
                text: "alpha".into(),
                metadata: HashMap::new(),
                corpus_type: CorpusType::Legal,
            }],
            &[Some(vec![1.0; 512])],
            &StubEmbeddingGenerator::from_pairs(vec![]),
        )
        .unwrap();

    let bytes = fs::read(output.join("turbo_static.bin")).unwrap();
    assert_eq!(
        bytes.len(),
        TurboHeader::SIZE + std::mem::size_of::<TurboRecord512>()
    );
}

/// Full-stack acceptance: a title provided via `metadata["title"]` at build time
/// survives StaticIndexBuilder → MmapIndex → TurboQuantSearcher → ContextBuilder
/// and renders as `[法规 #1] <title>`, while an untitled chunk stays bare.
#[test]
fn static_title_flows_from_builder_into_assembled_context() {
    let output = temp_fixture_dir("static-index-builder-context-e2e");

    StaticIndexBuilder::new()
        .build(
            &output,
            &[
                StaticChunk {
                    doc_id: "law-1".into(),
                    text: "民法典正文".into(),
                    metadata: HashMap::from([("title".into(), json!("民法典"))]),
                    corpus_type: CorpusType::Legal,
                },
                StaticChunk {
                    doc_id: "law-2".into(),
                    text: "无标题条文".into(),
                    metadata: HashMap::new(),
                    corpus_type: CorpusType::Legal,
                },
            ],
            // Distinct embeddings so the query deterministically ranks law-1 first.
            &[Some(vec![0.5; 512]), Some(vec![-0.5; 512])],
            &StubEmbeddingGenerator::from_pairs(vec![]),
        )
        .unwrap();

    let index = Arc::new(MmapIndex::load(&output).unwrap());
    let searcher = TurboQuantSearcher::new(Arc::clone(&index));
    let results = searcher
        .search(&stub_manifest(), &vec![0.5; 512], 2)
        .unwrap();
    assert_eq!(results[0].doc_id, index.meta(0).doc_id.to_string());

    let context = ContextBuilder::build_context(&results, &[], "民法典是什么?");
    assert!(
        context.contains("[法规 #1] 民法典\n民法典正文"),
        "assembled context missing enriched title label:\n{context}"
    );
    assert!(
        context.contains("[法规 #2]\n无标题条文"),
        "untitled chunk should render a bare label:\n{context}"
    );
}
