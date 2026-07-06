use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arrow_array::types::Float32Type;
use arrow_array::{FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use ltsearch::error::{SearchError, ValidationError};
use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, ProjectionMatrix, TurboHeader, TurboRecord512,
    META_RECORD_SIZE,
};
use ltsearch::models::{SearchRequest, SearchResponse};
use ltsearch::query_lambda::{
    bootstrap_query_handler_for_version_from_env, bootstrap_query_handler_from_env,
    handle_search_request,
};
use ltsearch::storage::{version_manifest_key, INDEX_HEAD_KEY};
use serde_json::json;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{Index, IndexWriter};
use tokio::runtime::Runtime;

static QUERY_LAMBDA_ENV_LOCK: Mutex<()> = Mutex::new(());

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
    write_lance_fixture_with_dim(root, relative_path, rows, 3);
}

fn write_lance_fixture_with_dim(
    root: &Path,
    relative_path: &str,
    rows: &[serde_json::Value],
    embedding_dim: i32,
) {
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
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                embedding_dim,
            ),
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
        embedding_dim,
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

fn sample_manifest_json(version_id: u64) -> String {
    sample_manifest_json_with_dim(version_id, 3)
}

fn sample_manifest_json_with_dim(version_id: u64, embedding_dim: usize) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "created_at": 1700000000000,
  "embedding_dim": {embedding_dim},
  "document_count": 2,
  "num_shards": 1,
  "shards": [
    {{
      "shard_id": 0,
      "document_count": 2,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_0",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_0"
    }}
  ]
}}"#
    )
}

fn centroid_table(dim: u32, centroids_per_dim: u32, values: &[f32]) -> CentroidTable {
    let mut bytes = Vec::with_capacity(8 + values.len() * 4);
    bytes.extend_from_slice(&dim.to_le_bytes());
    bytes.extend_from_slice(&centroids_per_dim.to_le_bytes());
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    CentroidTable::from_bytes(&bytes).unwrap()
}

fn identity_projection(dim: usize) -> ProjectionMatrix {
    let mut rows = Vec::with_capacity(dim);
    for row_index in 0..dim {
        let mut row = vec![0.0; dim];
        row[row_index] = 1.0;
        rows.push(row);
    }
    ProjectionMatrix::from_rows(rows)
}

fn padded_embedding(prefix: &[f32]) -> Vec<f32> {
    let mut embedding = vec![0.0; 512];
    embedding[..prefix.len()].copy_from_slice(prefix);
    embedding
}

struct StaticFixtureDoc<'a> {
    doc_id: u64,
    corpus_type: u8,
    text: &'a str,
    embedding: Vec<f32>,
}

fn write_static_fixture(root: &Path, docs: &[StaticFixtureDoc<'_>]) {
    let static_dir = root.join("static");
    fs::create_dir_all(&static_dir).unwrap();

    let dim = 512;
    let mut centroid_values = Vec::with_capacity(dim as usize * 4);
    for _ in 0..dim {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(dim, 4, &centroid_values);
    let projection = identity_projection(dim as usize);
    let header = TurboHeader::new(dim, docs.len() as u64);

    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();

    for doc in docs {
        let encoded = encode_vector(&doc.embedding, &centroids, &projection).unwrap();
        let record = TurboRecord512 {
            doc_id: doc.doc_id,
            idx: encoded.idx.clone().try_into().unwrap(),
            qjl: encoded.qjl.clone().try_into().unwrap(),
            gamma: encoded.gamma,
            _reserved: [0; 4],
        };
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &record as *const TurboRecord512 as *const u8,
                std::mem::size_of::<TurboRecord512>(),
            )
        };
        bin_data.extend_from_slice(record_bytes);

        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(doc.text.as_bytes());
        let meta = MetaRecord {
            doc_id: doc.doc_id,
            corpus_type: doc.corpus_type,
            _pad: [0; 3],
            text_offset,
            text_len: doc.text.len() as u32,
        };
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&meta as *const MetaRecord as *const u8, META_RECORD_SIZE)
        };
        meta_data.extend_from_slice(meta_bytes);
    }

    fs::write(static_dir.join("turbo_static.bin"), &bin_data).unwrap();
    fs::write(static_dir.join("turbo_static_meta.bin"), &meta_data).unwrap();
    fs::write(static_dir.join("turbo_static_text.bin"), &text_blob).unwrap();
    fs::write(static_dir.join("centroids.bin"), centroids.to_bytes()).unwrap();
    fs::write(static_dir.join("projection.bin"), projection.to_bytes()).unwrap();
}

fn valid_search_request() -> SearchRequest {
    SearchRequest {
        query: "rust search".into(),
        top_k: 3,
        filters: None,
        include_metadata: false,
        corpus_weights: None,
    }
}

#[cfg(feature = "ltembed")]
fn maybe_ltembed_bundle_dir() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|ancestor| ancestor.join("LTEmbed/ort_bundle"))
        .find(|candidate| {
            candidate.join("build-info.json").exists()
                && candidate.join("tokenizer.json").exists()
                && candidate.join("model.ort").exists()
        })
}

#[cfg(feature = "ltembed")]
fn repeated_embedding(dim: usize, value: f32) -> Vec<serde_json::Value> {
    (0..dim).map(|_| json!(value)).collect()
}

fn success_handler(_request: SearchRequest) -> Result<SearchResponse, SearchError> {
    Ok(SearchResponse {
        static_chunks: vec![],
        static_count: 0,
        dynamic_chunks: vec![],
        dynamic_count: 0,
        latency_ms: 12,
        index_version: 7,
    })
}

fn validation_error_handler(_request: SearchRequest) -> Result<SearchResponse, SearchError> {
    Err(SearchError::Validation(ValidationError::Required {
        field: "query",
    }))
}

fn execution_error_handler(_request: SearchRequest) -> Result<SearchResponse, SearchError> {
    Err(SearchError::Execution {
        message: "manifest load failed".into(),
    })
}

#[test]
fn query_lambda_returns_plain_search_response_on_success() {
    let response = handle_search_request(success_handler, valid_search_request()).unwrap();

    let body = serde_json::to_value(&response).unwrap();
    assert_eq!(body["index_version"], 7);
    assert_eq!(body["static_count"], 0);
    assert_eq!(body["dynamic_count"], 0);
    assert!(body.get("error_type").is_none());
}

#[test]
fn query_lambda_maps_validation_errors_to_error_envelope() {
    let error =
        handle_search_request(validation_error_handler, valid_search_request()).unwrap_err();

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "validation_error");
    assert_eq!(body["message"], "query is required");
}

#[test]
fn query_lambda_maps_execution_errors_to_error_envelope() {
    let error = handle_search_request(execution_error_handler, valid_search_request()).unwrap_err();

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
    assert_eq!(
        body["message"],
        "search execution failed: manifest load failed"
    );
}

#[test]
fn query_lambda_bootstrap_returns_service_error_when_embedding_provider_is_missing() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    std::env::remove_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER");

    let error = match bootstrap_query_handler_from_env() {
        Ok(_) => panic!("expected bootstrap to fail without embedding provider"),
        Err(error) => error,
    };

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
    assert_eq!(
        body["message"],
        "query lambda bootstrap failed: missing LTSEARCH_QUERY_EMBEDDING_PROVIDER"
    );
}

#[cfg(feature = "ltembed")]
#[test]
fn query_lambda_bootstrap_reports_missing_ltembed_bundle_dir() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let root = temp_fixture_dir("query-lambda-bootstrap-ltembed-missing-bundle");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "ltembed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::remove_var("LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR");
    std::env::remove_var("LTSEARCH_QUERY_LTEMBED_MODEL_PATH");

    let error = match bootstrap_query_handler_from_env() {
        Ok(_) => panic!("expected bootstrap to fail without LTEmbed bundle dir"),
        Err(error) => error,
    };

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
    assert_eq!(
        body["message"],
        "query lambda bootstrap failed: missing LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR"
    );
}

#[cfg(feature = "ltembed")]
#[test]
fn query_lambda_bootstrap_reports_missing_ltembed_bundle_files() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let root = temp_fixture_dir("query-lambda-bootstrap-ltembed-missing-files");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "ltembed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var(
        "LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR",
        root.join("no-such-bundle"),
    );
    std::env::set_var(
        "LTSEARCH_QUERY_LTEMBED_MODEL_PATH",
        root.join("no-such-bundle/model.ort"),
    );

    let error = match bootstrap_query_handler_from_env() {
        Ok(_) => panic!("expected bootstrap to fail when bundle files are missing"),
        Err(error) => error,
    };

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.starts_with(
            "query lambda bootstrap failed: embedding generation failed: LTEmbed bootstrap failed:"
        ),
        "unexpected message: {message}"
    );
}

#[test]
fn query_lambda_bootstrap_builds_fixed_embedding_handler_and_delegates_to_real_router() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let root = temp_fixture_dir("query-lambda-bootstrap-real-router");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id":"doc-1","text":"rust hybrid search","embedding":[0.9,0.0,0.0]}),
            json!({"doc_id":"doc-2","text":"rust keyword","embedding":[0.8,0.0,0.0]}),
        ],
    );

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "fixed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var("LTSEARCH_QUERY_FIXED_EMBEDDING", "0.1,0.2,0.3");

    let handler = bootstrap_query_handler_from_env()
        .expect("expected bootstrap to construct a real query handler");
    let response = handle_search_request(
        handler,
        SearchRequest {
            query: "rust".into(),
            top_k: 2,
            filters: None,
            include_metadata: false,
            corpus_weights: None,
        },
    )
    .expect("expected bootstrapped handler to search local fixtures");

    assert_eq!(response.index_version, 7);
    assert_eq!(response.static_count, 0);
    assert_eq!(response.dynamic_count, 2);
    assert_eq!(response.dynamic_chunks.len(), 2);
    assert_eq!(response.dynamic_chunks[0].doc_id, "doc-1");
    assert!(response
        .dynamic_chunks
        .iter()
        .any(|result| result.doc_id == "doc-2"));
}

#[test]
fn query_lambda_bootstrap_loads_turbo_static_searcher_when_static_artifacts_exist() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let root = temp_fixture_dir("query-lambda-bootstrap-static-router");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(
        &root,
        &version_manifest_key(7),
        &sample_manifest_json_with_dim(7, 512),
    );
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    write_lance_fixture_with_dim(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id":"doc-1","text":"rust hybrid search","embedding": padded_embedding(&[1.2, -1.4, 0.3, 0.9])}),
            json!({"doc_id":"doc-2","text":"rust keyword","embedding": padded_embedding(&[1.0, -1.0, 0.2, 0.7])}),
        ],
        512,
    );
    write_static_fixture(
        &root,
        &[StaticFixtureDoc {
            doc_id: 10,
            corpus_type: 0,
            text: "static legal ten",
            embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        }],
    );

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "fixed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var(
        "LTSEARCH_QUERY_FIXED_EMBEDDING",
        padded_embedding(&[1.2, -1.4, 0.3, 0.9])
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(","),
    );

    let handler = bootstrap_query_handler_from_env()
        .expect("expected bootstrap to construct a router with static retrieval");
    let response = handle_search_request(
        handler,
        SearchRequest {
            query: "rust".into(),
            top_k: 2,
            filters: None,
            include_metadata: false,
            corpus_weights: None,
        },
    )
    .expect("expected bootstrapped handler to search static and dynamic fixtures");

    assert_eq!(response.index_version, 7);
    assert_eq!(response.static_count, 1);
    assert_eq!(response.static_chunks.len(), 1);
    assert_eq!(response.static_chunks[0].doc_id, "10");
}

#[test]
fn query_lambda_bootstrap_rejects_fixed_embedding_dim_mismatch_before_serving_requests() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let root = temp_fixture_dir("query-lambda-bootstrap-dim-mismatch");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "fixed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var("LTSEARCH_QUERY_FIXED_EMBEDDING", "0.1,0.2");

    let error = match bootstrap_query_handler_from_env() {
        Ok(_) => panic!("expected bootstrap to fail for fixed embedding dimension mismatch"),
        Err(error) => error,
    };

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
    assert_eq!(
        body["message"],
        "query lambda bootstrap failed: LTSEARCH_QUERY_FIXED_EMBEDDING dimension 2 does not match manifest embedding_dim 3"
    );
}

#[cfg(feature = "ltembed")]
#[test]
fn query_lambda_bootstrap_builds_ltembed_handler_and_delegates_to_real_router() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let Some(bundle_dir) = maybe_ltembed_bundle_dir() else {
        eprintln!("Skipping: LTEmbed ort_bundle not found in sibling checkout");
        return;
    };

    let root = temp_fixture_dir("query-lambda-bootstrap-ltembed-real-router");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(
        &root,
        &version_manifest_key(7),
        &sample_manifest_json_with_dim(7, 384),
    );
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    write_lance_fixture_with_dim(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id":"doc-1","text":"rust hybrid search","embedding": repeated_embedding(512, 0.01)}),
            json!({"doc_id":"doc-2","text":"rust keyword","embedding": repeated_embedding(512, 0.009)}),
        ],
        512,
    );

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "ltembed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var("LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR", &bundle_dir);
    std::env::set_var(
        "LTSEARCH_QUERY_LTEMBED_MODEL_PATH",
        bundle_dir.join("model.ort"),
    );

    let handler = bootstrap_query_handler_from_env().expect("expected LTEmbed bootstrap to work");
    let response = handle_search_request(
        handler,
        SearchRequest {
            query: "rust".into(),
            top_k: 2,
            filters: None,
            include_metadata: false,
            corpus_weights: None,
        },
    )
    .expect("expected LTEmbed bootstrapped handler to search local fixtures");

    assert_eq!(response.index_version, 7);
    assert_eq!(response.static_count, 0);
    assert_eq!(response.dynamic_count, 2);
    assert_eq!(response.dynamic_chunks.len(), 2);
}

#[cfg(feature = "ltembed")]
#[test]
fn query_lambda_bootstrap_rejects_ltembed_dim_mismatch_before_serving_requests() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let Some(bundle_dir) = maybe_ltembed_bundle_dir() else {
        eprintln!("Skipping: LTEmbed ort_bundle not found in sibling checkout");
        return;
    };

    let root = temp_fixture_dir("query-lambda-bootstrap-ltembed-dim-mismatch");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(
        &root,
        &version_manifest_key(7),
        &sample_manifest_json_with_dim(7, 3),
    );

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "ltembed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var("LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR", &bundle_dir);
    std::env::set_var(
        "LTSEARCH_QUERY_LTEMBED_MODEL_PATH",
        bundle_dir.join("model.ort"),
    );

    let error = match bootstrap_query_handler_from_env() {
        Ok(_) => panic!("expected bootstrap to fail for LTEmbed embedding dimension mismatch"),
        Err(error) => error,
    };
    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
    assert_eq!(
        body["message"],
        "query lambda bootstrap failed: LTSEARCH_QUERY_LTEMBED embedding dimension 512 does not match manifest embedding_dim 3"
    );
}

#[test]
fn query_lambda_bootstrap_reports_unsupported_provider_before_provider_specific_env_errors() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "mystery");
    std::env::remove_var("LTSEARCH_QUERY_ARTIFACT_ROOT");
    std::env::remove_var("LTSEARCH_QUERY_FIXED_EMBEDDING");

    let error = match bootstrap_query_handler_from_env() {
        Ok(_) => panic!("expected bootstrap to reject unsupported provider"),
        Err(error) => error,
    };

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
    assert_eq!(
        body["message"],
        "query lambda bootstrap failed: unsupported LTSEARCH_QUERY_EMBEDDING_PROVIDER: mystery"
    );
}

#[test]
fn query_lambda_bootstrap_for_version_rejects_when_active_version_changes() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let root = temp_fixture_dir("query-lambda-bootstrap-version-mismatch");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(8));
    write_fixture(&root, &version_manifest_key(8), &sample_manifest_json(8));

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "fixed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var("LTSEARCH_QUERY_FIXED_EMBEDDING", "0.1,0.2,0.3");

    let error: ltsearch::query_lambda::QueryLambdaError =
        match bootstrap_query_handler_for_version_from_env(7) {
            Ok(_) => panic!("expected pinned bootstrap to reject changed active version"),
            Err(error) => error,
        };

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
    assert_eq!(
        body["message"],
        "query lambda bootstrap failed: active manifest version changed during bootstrap: expected 7, got 8"
    );
}

#[test]
fn query_lambda_bootstrap_for_version_pins_served_manifest_after_head_changes() {
    let _guard = QUERY_LAMBDA_ENV_LOCK.lock().unwrap();
    let root = temp_fixture_dir("query-lambda-bootstrap-pinned-manifest");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_fixture(&root, &version_manifest_key(8), &sample_manifest_json(8));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust stable result"), ("doc-2", "rust backup")],
    );
    write_index(
        &root,
        "index/v8/shard_0",
        &[("doc-3", "rust upgraded result")],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id":"doc-1","text":"rust stable result","embedding":[0.9,0.0,0.0]}),
            json!({"doc_id":"doc-2","text":"rust backup","embedding":[0.8,0.0,0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v8/shard_0",
        &[json!({"doc_id":"doc-3","text":"rust upgraded result","embedding":[0.95,0.0,0.0]})],
    );

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "fixed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var("LTSEARCH_QUERY_FIXED_EMBEDDING", "0.1,0.2,0.3");

    let handler = bootstrap_query_handler_for_version_from_env(7)
        .expect("expected version-pinned bootstrap to succeed");

    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(8));

    let response = handle_search_request(
        handler,
        SearchRequest {
            query: "rust".into(),
            top_k: 3,
            filters: None,
            include_metadata: false,
            corpus_weights: None,
        },
    )
    .expect("expected pinned handler to continue serving the bootstrapped manifest version");

    assert_eq!(response.index_version, 7);
    assert_eq!(response.static_count, 0);
    assert_eq!(response.dynamic_count, 2);
    assert_eq!(response.dynamic_chunks.len(), 2);
    assert!(response
        .dynamic_chunks
        .iter()
        .any(|result| result.doc_id == "doc-1"));
    assert!(response
        .dynamic_chunks
        .iter()
        .any(|result| result.doc_id == "doc-2"));
    assert!(!response
        .dynamic_chunks
        .iter()
        .any(|result| result.doc_id == "doc-3"));
}
