use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::indexing::{BuildIndexRequest, LocalIndexBuilder};
use ltsearch::models::{Document, SearchRequest, WalOperation, WalRecord};
use ltsearch::query::{KeywordSearcher, QueryRouter, VectorSearcher};
use ltsearch::storage::{
    version_manifest_key, ActiveManifest, LocalManifestStore, ManifestHead, ManifestStore,
    ManifestStoreError,
};
use serde_json::{json, Value};

type EmbeddingResponses = VecDeque<(String, Vec<f32>)>;

#[derive(Clone, Debug)]
struct StubEmbeddingGenerator {
    responses: Arc<Mutex<EmbeddingResponses>>,
}

impl StubEmbeddingGenerator {
    fn from_pairs(pairs: Vec<(&str, Vec<f32>)>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(
                pairs
                    .into_iter()
                    .map(|(text, embedding)| (text.to_string(), embedding))
                    .collect(),
            )),
        }
    }
}

impl EmbeddingGenerator for StubEmbeddingGenerator {
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        let (expected_query, embedding) = self.responses.lock().unwrap().pop_front().unwrap();
        assert_eq!(query, expected_query);
        Ok(embedding)
    }
}

#[derive(Clone, Debug)]
struct FixedEmbeddingGenerator {
    embedding: Vec<f32>,
}

#[derive(Clone, Debug)]
struct FixedManifestStore {
    active_manifest: ActiveManifest,
}

impl FixedManifestStore {
    fn new(active_manifest: ActiveManifest) -> Self {
        Self { active_manifest }
    }
}

impl ManifestStore for FixedManifestStore {
    fn load_head(&self) -> Result<ManifestHead, ManifestStoreError> {
        Ok(self.active_manifest.head.clone())
    }

    fn load_active_manifest(&self) -> Result<ActiveManifest, ManifestStoreError> {
        Ok(self.active_manifest.clone())
    }
}

impl EmbeddingGenerator for FixedEmbeddingGenerator {
    fn generate(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
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

fn upsert_record_with_metadata(
    event_id: &str,
    doc_id: &str,
    text: &str,
    embedding: Option<Vec<f32>>,
    metadata: HashMap<String, Value>,
    timestamp: i64,
) -> WalRecord {
    WalRecord {
        event_id: event_id.into(),
        doc_id: doc_id.into(),
        op: WalOperation::Upsert,
        document: Some(Document {
            doc_id: doc_id.into(),
            text: text.into(),
            embedding,
            metadata,
            timestamp,
        }),
        timestamp,
    }
}

fn active_manifest_from_built(built: &ltsearch::indexing::BuildIndexResult) -> ActiveManifest {
    ActiveManifest {
        head: ManifestHead {
            version_id: built.manifest.version_id,
            manifest_path: version_manifest_key(built.manifest.version_id),
            updated_at: built.manifest.created_at,
        },
        manifest: built.manifest.clone(),
    }
}

fn build_real_artifacts(root: &PathBuf) -> ltsearch::indexing::BuildIndexResult {
    let builder = LocalIndexBuilder::new(
        root,
        StubEmbeddingGenerator::from_pairs(vec![
            ("generated body", vec![0.3, 0.7, 0.0]),
            ("hybrid rust body", vec![0.9, 0.1, 0.0]),
        ]),
    );

    builder
        .build(&BuildIndexRequest {
            version_id: 13,
            created_at: 1_700_000_059_000,
            embedding_dim: 3,
            records: vec![
                upsert_record_with_metadata(
                    "event-1",
                    "doc-generated",
                    "generated body",
                    None,
                    HashMap::from([("lang".into(), json!("rust"))]),
                    1_700_000_000_100,
                ),
                upsert_record_with_metadata(
                    "event-2",
                    "doc-hybrid",
                    "hybrid rust body",
                    None,
                    HashMap::from([
                        ("lang".into(), json!("rust")),
                        ("tier".into(), json!("gold")),
                    ]),
                    1_700_000_000_200,
                ),
                upsert_record_with_metadata(
                    "event-3",
                    "doc-noise",
                    "python server noise",
                    Some(vec![0.0, 0.0, 1.0]),
                    HashMap::from([("lang".into(), json!("python"))]),
                    1_700_000_000_300,
                ),
            ],
        })
        .unwrap()
}

#[test]
fn keyword_searcher_reads_builder_generated_tantivy_index() {
    let root = temp_fixture_dir("search-built-artifacts-keyword");
    let built = build_real_artifacts(&root);
    let active_manifest = active_manifest_from_built(&built);

    let results = KeywordSearcher::new(LocalManifestStore::new(&root), &root)
        .search_active_manifest(&active_manifest, "hybrid", 2)
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].doc_id, "doc-hybrid");
}

#[test]
fn vector_searcher_reads_builder_generated_lance_table() {
    let root = temp_fixture_dir("search-built-artifacts-vector");
    let built = build_real_artifacts(&root);
    let active_manifest = active_manifest_from_built(&built);

    let results = VectorSearcher::new(LocalManifestStore::new(&root), &root)
        .search_active_manifest(&active_manifest, &[0.9, 0.1, 0.0], 3)
        .unwrap();

    assert!(!results.is_empty());
    assert_eq!(results[0].doc_id, "doc-hybrid");
}

#[test]
fn router_hybrid_searches_over_builder_generated_artifacts() {
    let root = temp_fixture_dir("search-built-artifacts-router");
    let built = build_real_artifacts(&root);
    let active_manifest = active_manifest_from_built(&built);

    let manifest_store = FixedManifestStore::new(active_manifest.clone());
    let router = QueryRouter::new(
        manifest_store.clone(),
        FixedEmbeddingGenerator {
            embedding: vec![0.9, 0.1, 0.0],
        },
        KeywordSearcher::new(manifest_store.clone(), &root),
        VectorSearcher::new(manifest_store, &root),
    );

    let response = router
        .search(&SearchRequest {
            query: "hybrid rust".into(),
            top_k: 2,
            filters: None,
            include_metadata: true,
            corpus_weights: None,
        })
        .unwrap();

    assert_eq!(response.index_version, built.manifest.version_id);
    assert!(!response.dynamic_chunks.is_empty());
    assert!(response
        .dynamic_chunks
        .iter()
        .any(|result| result.doc_id == "doc-hybrid"));
    assert!(response
        .dynamic_chunks
        .iter()
        .all(|result| result.metadata.is_some()));
}

#[test]
fn vector_searcher_rejects_dimension_mismatch_against_builder_manifest() {
    let root = temp_fixture_dir("search-built-artifacts-dim-mismatch");
    let built = build_real_artifacts(&root);
    let active_manifest = active_manifest_from_built(&built);

    let error = VectorSearcher::new(LocalManifestStore::new(&root), &root)
        .search_active_manifest(&active_manifest, &[1.0, 0.0], 2)
        .unwrap_err();

    assert_eq!(error.to_string(), "query_embedding has an invalid value");
}
