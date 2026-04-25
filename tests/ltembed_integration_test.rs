#![cfg(feature = "ltembed")]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::embedding::{
    EmbeddingGenerator, LTEmbedConfig, LTEmbedEmbeddingGenerator, LTEmbedPoolingMode,
};
use ltsearch::indexing::{BuildIndexRequest, LocalIndexBuilder};
use ltsearch::models::{Document, SearchRequest, WalOperation, WalRecord};
use ltsearch::query::{KeywordSearcher, QueryRouter, VectorSearcher};
use ltsearch::storage::{
    version_manifest_key, ActiveManifest, ManifestHead, ManifestStore, ManifestStoreError,
};

#[derive(Debug, Clone)]
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

fn ltembed_config(assets_dir: &Path, prefix: &str) -> LTEmbedConfig {
    LTEmbedConfig {
        model_path: assets_dir.join("model.safetensors").display().to_string(),
        config_path: assets_dir.join("config.json").display().to_string(),
        tokenizer_path: assets_dir.join("tokenizer.json").display().to_string(),
        pooling: LTEmbedPoolingMode::Mean,
        prefix: Some(prefix.into()),
    }
}

fn upsert_record(
    event_id: &str,
    doc_id: &str,
    text: &str,
    embedding: Option<Vec<f32>>,
    metadata: HashMap<String, serde_json::Value>,
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

#[test]
fn ltembed_end_to_end_build_and_hybrid_query_flow() {
    let Some(assets_dir) = maybe_ltembed_assets_dir() else {
        eprintln!("Skipping: LTEmbed assets not found in sibling checkout");
        return;
    };

    let root = temp_fixture_dir("ltembed-end-to-end-flow");
    let build_generator =
        LTEmbedEmbeddingGenerator::from_config(&ltembed_config(&assets_dir, "passage:"))
            .expect("expected LTEmbed build generator to bootstrap");
    let builder = LocalIndexBuilder::new(&root, build_generator);

    let built = builder
        .build(&BuildIndexRequest {
            version_id: 9,
            created_at: 1_700_000_029_000,
            embedding_dim: 384,
            records: vec![
                upsert_record(
                    "event-1",
                    "doc-rust-hybrid",
                    "rust hybrid retrieval with vector and keyword signals",
                    None,
                    HashMap::from([("lang".into(), serde_json::json!("rust"))]),
                    1_700_000_001_100,
                ),
                upsert_record(
                    "event-2",
                    "doc-rust-keyword",
                    "rust keyword search with tantivy ranking",
                    None,
                    HashMap::from([("lang".into(), serde_json::json!("rust"))]),
                    1_700_000_001_200,
                ),
                upsert_record(
                    "event-3",
                    "doc-python-noise",
                    "python web framework and api server",
                    None,
                    HashMap::from([("lang".into(), serde_json::json!("python"))]),
                    1_700_000_001_300,
                ),
            ],
        })
        .expect("expected LTEmbed-backed builder to succeed");

    let active_manifest = ActiveManifest {
        head: ManifestHead {
            version_id: built.manifest.version_id,
            manifest_path: version_manifest_key(built.manifest.version_id),
            updated_at: built.manifest.created_at,
        },
        manifest: built.manifest.clone(),
    };

    let query_generator =
        LTEmbedEmbeddingGenerator::from_config(&ltembed_config(&assets_dir, "query:"))
            .expect("expected LTEmbed query generator to bootstrap");
    let manifest_store = FixedManifestStore::new(active_manifest.clone());
    let keyword_searcher = KeywordSearcher::new(manifest_store.clone(), &root);
    let vector_searcher = VectorSearcher::new(manifest_store.clone(), &root);
    let router = QueryRouter::new(
        manifest_store,
        query_generator,
        keyword_searcher,
        vector_searcher,
    );

    let response = router
        .search(&SearchRequest {
            query: "rust retrieval".into(),
            top_k: 3,
            filters: None,
            include_metadata: true,
            corpus_weights: None,
        })
        .expect("expected LTEmbed end-to-end search to succeed");

    assert_eq!(response.index_version, 9);
    assert_eq!(built.manifest.embedding_dim, 384);
    assert!(!response.results.is_empty());
    assert!(response
        .results
        .iter()
        .any(|result| result.doc_id == "doc-rust-hybrid"));
    assert!(response
        .results
        .iter()
        .any(|result| result.doc_id == "doc-rust-keyword"));
    assert!(response
        .results
        .iter()
        .all(|result| result.metadata.is_some()));
    assert!(response
        .results
        .iter()
        .all(|result| !result.doc_id.is_empty()));

    let query_embedding =
        LTEmbedEmbeddingGenerator::from_config(&ltembed_config(&assets_dir, "query:"))
            .expect("expected LTEmbed query generator to bootstrap")
            .generate("rust retrieval")
            .expect("expected LTEmbed query embedding to be generated");
    assert_eq!(query_embedding.len(), built.manifest.embedding_dim);

    let keyword_results =
        KeywordSearcher::new(FixedManifestStore::new(active_manifest.clone()), &root)
            .search_active_manifest(&active_manifest, "rust", 3)
            .expect("expected keyword search over LTEmbed-built artifacts to succeed");
    assert!(!keyword_results.is_empty());

    let vector_results =
        VectorSearcher::new(FixedManifestStore::new(active_manifest.clone()), &root)
            .search_active_manifest(&active_manifest, &query_embedding, 3)
            .expect("expected vector search over LTEmbed-built artifacts to succeed");
    assert!(!vector_results.is_empty());
}
