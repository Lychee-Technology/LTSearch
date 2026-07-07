use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::indexing::{BuildIndexRequest, LocalIndexBuilder};
use ltsearch::models::{Document, FilterValue, SearchRequest, WalOperation, WalRecord};
use ltsearch::query::{KeywordSearcher, QueryRouter, VectorSearcher};
use ltsearch::storage::{
    version_manifest_key, ActiveManifest, ManifestHead, ManifestStore, ManifestStoreError,
};
use serde_json::{json, Value};

type EmbeddingResponses = VecDeque<(String, Result<Vec<f32>, EmbeddingError>)>;

#[derive(Clone, Debug)]
struct StubEmbeddingGenerator {
    responses: Arc<Mutex<EmbeddingResponses>>,
}

impl StubEmbeddingGenerator {
    fn from_results(pairs: Vec<(&str, Result<Vec<f32>, EmbeddingError>)>) -> Self {
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
        embedding
    }
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
        StubEmbeddingGenerator::from_results(vec![
            ("hybrid rust retrieval", Ok(vec![0.9, 0.1, 0.0])),
            ("keyword heavy rust text", Ok(vec![0.6, 0.2, 0.0])),
        ]),
    );

    builder
        .build(&BuildIndexRequest {
            version_id: 14,
            created_at: 1_700_000_069_000,
            embedding_dim: 3,
            records: vec![
                upsert_record_with_metadata(
                    "event-1",
                    "doc-rust-hybrid",
                    "hybrid rust retrieval",
                    None,
                    HashMap::from([
                        ("lang".into(), json!("rust")),
                        ("tier".into(), json!("gold")),
                    ]),
                    1_700_000_000_100,
                ),
                upsert_record_with_metadata(
                    "event-2",
                    "doc-rust-keyword",
                    "keyword heavy rust text",
                    None,
                    HashMap::from([
                        ("lang".into(), json!("rust")),
                        ("tier".into(), json!("silver")),
                    ]),
                    1_700_000_000_200,
                ),
                upsert_record_with_metadata(
                    "event-3",
                    "doc-python-noise",
                    "python async web server",
                    Some(vec![0.0, 0.0, 1.0]),
                    HashMap::from([
                        ("lang".into(), json!("python")),
                        ("tier".into(), json!("gold")),
                    ]),
                    1_700_000_000_300,
                ),
            ],
        })
        .unwrap()
}

#[test]
fn build_to_hybrid_query_flow_returns_built_version_and_unique_results() {
    let root = temp_fixture_dir("query-build-flow-hybrid");
    let built = build_real_artifacts(&root);
    let active_manifest = active_manifest_from_built(&built);

    let manifest_store = FixedManifestStore::new(active_manifest.clone());
    let router = QueryRouter::new(
        manifest_store.clone(),
        StubEmbeddingGenerator::from_results(vec![("rust retrieval", Ok(vec![0.9, 0.1, 0.0]))]),
        KeywordSearcher::new(manifest_store.clone(), &root),
        VectorSearcher::new(manifest_store, &root),
    );

    let response = router
        .search(&SearchRequest {
            query: "rust retrieval".into(),
            top_k: 2,
            filters: None,
            include_metadata: true,
            corpus_weights: None,
        })
        .unwrap();

    assert_eq!(response.index_version, 14);
    // top_k=2 → retrieval window 6; all 3 indexed dynamic docs fit and are
    // returned (the old top_k truncation would have dropped the lowest-ranked).
    assert_eq!(response.dynamic_chunks.len(), 3);
    assert_eq!(response.dynamic_count, 3);
    let unique_ids: std::collections::HashSet<_> = response
        .dynamic_chunks
        .iter()
        .map(|result| result.doc_id.as_str())
        .collect();
    assert_eq!(unique_ids.len(), 3);
    assert!(response
        .dynamic_chunks
        .iter()
        .any(|result| result.doc_id == "doc-rust-hybrid"));
    assert!(response
        .dynamic_chunks
        .iter()
        .any(|result| result.doc_id == "doc-rust-keyword"));
    assert!(response
        .dynamic_chunks
        .iter()
        .all(|result| result.metadata.is_some()));
}

#[test]
fn build_to_query_flow_falls_back_to_keyword_only_when_query_embedding_fails() {
    let root = temp_fixture_dir("query-build-flow-keyword-fallback");
    let built = build_real_artifacts(&root);
    let active_manifest = active_manifest_from_built(&built);

    let manifest_store = FixedManifestStore::new(active_manifest);
    let router = QueryRouter::new(
        manifest_store.clone(),
        StubEmbeddingGenerator::from_results(vec![
            (
                "rust retrieval",
                Err(EmbeddingError::Generation {
                    message: "simulated query embedding failure".into(),
                }),
            ),
            (
                "rust retrieval",
                Err(EmbeddingError::Generation {
                    message: "simulated query embedding failure".into(),
                }),
            ),
        ]),
        KeywordSearcher::new(manifest_store.clone(), &root),
        VectorSearcher::new(manifest_store, &root),
    );

    let response = router
        .search(&SearchRequest {
            query: "rust retrieval".into(),
            top_k: 2,
            filters: Some(HashMap::from([(
                "lang".into(),
                FilterValue::StringEquals("rust".into()),
            )])),
            include_metadata: true,
            corpus_weights: None,
        })
        .unwrap();

    assert_eq!(response.index_version, 14);
    assert!(!response.dynamic_chunks.is_empty());
    assert!(response
        .dynamic_chunks
        .iter()
        .all(|result| result.doc_id.starts_with("doc-rust")));
    assert!(response.dynamic_chunks.iter().all(|result| {
        result
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("lang"))
            == Some(&json!("rust"))
    }));
}
