use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::models::{IndexManifest, SearchRequest, ShardManifest};
use ltsearch::query::{KeywordSearcher, QueryRouter, VectorSearcher};
use ltsearch::storage::{
    version_manifest_key, ActiveManifest, ManifestHead, ManifestStore, ManifestStoreError,
    INDEX_HEAD_KEY,
};
use serde_json::json;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{Index, IndexWriter};
use tokio::runtime::Runtime;

// ---------------------------------------------------------------------------
// Test ManifestStore implementation
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Test EmbeddingGenerator implementations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FixedEmbeddingGenerator {
    embedding: Vec<f32>,
}

impl FixedEmbeddingGenerator {
    fn new(embedding: Vec<f32>) -> Self {
        Self { embedding }
    }
}

impl EmbeddingGenerator for FixedEmbeddingGenerator {
    fn generate(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }
}

#[derive(Debug, Clone)]
struct FailingEmbeddingGenerator;

impl EmbeddingGenerator for FailingEmbeddingGenerator {
    fn generate(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::Generation {
            message: "intentionally failing for test".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers (reused patterns from query_lambda_test.rs)
// ---------------------------------------------------------------------------

fn temp_fixture_dir(test_name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ltsearch-{test_name}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_fixture(root: &Path, relative_path: &str, contents: &str) {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn write_index(root: &Path, relative_path: &str, documents: &[(&str, &str)]) {
    let index_path = root.join(relative_path);
    fs::create_dir_all(&index_path).unwrap();

    let mut schema_builder = Schema::builder();
    let doc_id = schema_builder.add_text_field("doc_id", TEXT | STORED);
    let text = schema_builder.add_text_field("text", TEXT | STORED);
    let schema = schema_builder.build();

    let index = Index::create_in_dir(&index_path, schema).unwrap();
    let mut writer: IndexWriter = index.writer(15_000_000).unwrap();

    for (document_id, body) in documents {
        writer
            .add_document(doc!(doc_id => (*document_id).to_string(), text => (*body).to_string()))
            .unwrap();
    }

    writer.commit().unwrap();
    index
        .reader_builder()
        .try_into()
        .unwrap()
        .searcher()
        .search(
            &tantivy::query::AllQuery,
            &TopDocs::with_limit(documents.len().max(1)),
        )
        .unwrap();
}

fn write_lance_fixture(root: &Path, relative_path: &str, rows: &[serde_json::Value]) {
    let shard_dir = root.join(relative_path);
    fs::create_dir_all(&shard_dir).unwrap();

    let shard_dir_string = shard_dir.to_str().unwrap().to_string();
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("doc_id", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("metadata", DataType::Utf8, false),
        Field::new("timestamp", DataType::Int64, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), 3),
            true,
        ),
    ]));

    let doc_ids = StringArray::from(
        rows.iter()
            .map(|row| row["doc_id"].as_str())
            .collect::<Vec<_>>(),
    );
    let texts = StringArray::from(
        rows.iter()
            .map(|row| row["text"].as_str())
            .collect::<Vec<_>>(),
    );
    let metadata = StringArray::from(
        rows.iter()
            .map(|row| serde_json::to_string(row.get("metadata").unwrap_or(&json!({}))).unwrap())
            .collect::<Vec<_>>(),
    );
    let timestamps = Int64Array::from(vec![0_i64; rows.len()]);
    let embeddings = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter().map(|row| {
            row["embedding"].as_array().map(|embedding| {
                embedding
                    .iter()
                    .map(|value| Some(value.as_f64().unwrap() as f32))
                    .collect::<Vec<_>>()
            })
        }),
        3,
    );

    Runtime::new().unwrap().block_on(async move {
        let conn = lancedb::connect(&shard_dir_string).execute().await.unwrap();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(doc_ids),
                Arc::new(texts),
                Arc::new(metadata),
                Arc::new(timestamps),
                Arc::new(embeddings),
            ],
        )
        .unwrap();
        let batches = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), schema);

        conn.create_table("documents", batches)
            .execute()
            .await
            .unwrap();
    });
}

fn sample_head_json(version_id: u64) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "manifest_path": "{}",
  "updated_at": 1700000005000
}}"#,
        version_manifest_key(version_id)
    )
}

fn sample_manifest_json(version_id: u64, document_count: usize) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "created_at": 1700000000000,
  "embedding_dim": 3,
  "document_count": {document_count},
  "num_shards": 1,
  "shards": [
    {{
      "shard_id": 0,
      "document_count": {document_count},
      "lance_path": "s3://bucket/lance/v{version_id}/shard_0",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_0"
    }}
  ]
}}"#
    )
}

fn build_active_manifest(version_id: u64, document_count: usize) -> ActiveManifest {
    ActiveManifest {
        head: ManifestHead {
            version_id,
            manifest_path: version_manifest_key(version_id),
            updated_at: 1700000005000,
        },
        manifest: IndexManifest {
            version_id,
            created_at: 1700000000000,
            embedding_dim: 3,
            document_count,
            num_shards: 1,
            shards: vec![ShardManifest {
                shard_id: 0,
                document_count,
                lance_path: format!("s3://bucket/lance/v{version_id}/shard_0"),
                tantivy_path: format!("s3://bucket/index/v{version_id}/shard_0"),
            }],
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn query_flow_hybrid_retrieval_returns_correct_version_and_results() {
    let root = temp_fixture_dir("query-flow-hybrid");

    // Seed fixtures: _head, manifest, tantivy index, lancedb table with 3 documents
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(1));
    write_fixture(&root, &version_manifest_key(1), &sample_manifest_json(1, 3));

    write_index(
        &root,
        "index/v1/shard_0",
        &[
            ("doc-1", "rust hybrid search"),
            ("doc-2", "rust keyword matching"),
            ("doc-3", "python web framework"),
        ],
    );

    write_lance_fixture(
        &root,
        "lance/v1/shard_0",
        &[
            json!({"doc_id": "doc-1", "text": "rust hybrid search", "embedding": [0.9, 0.1, 0.0]}),
            json!({"doc_id": "doc-2", "text": "rust keyword matching", "embedding": [0.8, 0.1, 0.0]}),
            json!({"doc_id": "doc-3", "text": "python web framework", "embedding": [0.0, 0.0, 0.9]}),
        ],
    );

    // Construct QueryRouter with test implementations
    let active_manifest = build_active_manifest(1, 3);
    let manifest_store = FixedManifestStore::new(active_manifest);
    let embedding_generator = FixedEmbeddingGenerator::new(vec![0.9, 0.1, 0.0]);

    let keyword_searcher = KeywordSearcher::new(manifest_store.clone(), &root);
    let vector_searcher = VectorSearcher::new(manifest_store.clone(), &root);

    let router = QueryRouter::new(
        manifest_store,
        embedding_generator,
        keyword_searcher,
        vector_searcher,
    );

    let request = SearchRequest {
        query: "rust".into(),
        top_k: 2,
        filters: None,
        include_metadata: false,
        corpus_weights: None,
    };

    let response = router.search(&request).expect("search should succeed");

    // Assert: response.index_version == 1
    assert_eq!(response.index_version, 1);

    // Assert: response.dynamic_chunks.len() == 2
    assert_eq!(response.dynamic_chunks.len(), 2);

    // Assert: all doc_ids are unique
    let unique_ids: HashSet<&str> = response
        .dynamic_chunks
        .iter()
        .map(|r| r.doc_id.as_str())
        .collect();
    assert_eq!(unique_ids.len(), response.dynamic_chunks.len());

    // Assert: dynamic_count reflects available results
    assert_eq!(response.dynamic_count, response.dynamic_chunks.len());
}

#[test]
fn query_flow_keyword_only_fallback_when_vector_search_unavailable() {
    let root = temp_fixture_dir("query-flow-keyword-fallback");

    // Seed fixtures: _head, manifest, tantivy index (but NO lancedb data)
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(1));
    write_fixture(&root, &version_manifest_key(1), &sample_manifest_json(1, 3));

    write_index(
        &root,
        "index/v1/shard_0",
        &[
            ("doc-1", "rust hybrid search"),
            ("doc-2", "rust keyword matching"),
            ("doc-3", "python web framework"),
        ],
    );

    // Create the lance directory structure but leave it empty (no table created)
    let lance_dir = root.join("lance/v1/shard_0");
    fs::create_dir_all(&lance_dir).unwrap();

    // Construct QueryRouter with a FailingEmbeddingGenerator so it falls back to keyword-only
    let active_manifest = build_active_manifest(1, 3);
    let manifest_store = FixedManifestStore::new(active_manifest);
    let embedding_generator = FailingEmbeddingGenerator;

    let keyword_searcher = KeywordSearcher::new(manifest_store.clone(), &root);
    let vector_searcher = VectorSearcher::new(manifest_store.clone(), &root);

    let router = QueryRouter::new(
        manifest_store,
        embedding_generator,
        keyword_searcher,
        vector_searcher,
    );

    let request = SearchRequest {
        query: "rust".into(),
        top_k: 2,
        filters: None,
        include_metadata: false,
        corpus_weights: None,
    };

    let response = router
        .search(&request)
        .expect("keyword-only fallback search should succeed");

    // Assert: response.index_version is correct
    assert_eq!(response.index_version, 1);

    // Assert: results came back (keyword search should find "rust" in doc-1 and doc-2)
    assert!(
        !response.dynamic_chunks.is_empty(),
        "keyword fallback should return results"
    );
    assert_eq!(response.dynamic_chunks.len(), 2);

    // Assert: all doc_ids are unique
    let unique_ids: HashSet<&str> = response
        .dynamic_chunks
        .iter()
        .map(|r| r.doc_id.as_str())
        .collect();
    assert_eq!(unique_ids.len(), response.dynamic_chunks.len());

    // Assert: dynamic_count matches results length
    assert_eq!(response.dynamic_count, response.dynamic_chunks.len());

    // Assert: results are from keyword search (the "rust" docs)
    assert!(
        response.dynamic_chunks.iter().any(|r| r.doc_id == "doc-1"),
        "doc-1 should be in keyword results"
    );
    assert!(
        response.dynamic_chunks.iter().any(|r| r.doc_id == "doc-2"),
        "doc-2 should be in keyword results"
    );
}
