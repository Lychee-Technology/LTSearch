mod common;

use std::fs;
use std::path::Path;
#[cfg(feature = "ltembed")]
use std::path::PathBuf;
use std::sync::Mutex;

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

// 磁盘 fixture 构建器（temp dir / _head / manifest / tantivy / lance）统一由
// `tests/common` 提供，避免与 http_query_test 各持一份副本。此处仅保留
// query_lambda 专用的静态检索 fixture 与 handler 桩。
static QUERY_LAMBDA_ENV_LOCK: Mutex<()> = Mutex::new(());

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
            _pad: [0; 7],
            title_offset: 0,
            title_len: 0,
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
    fs::write(static_dir.join("turbo_static_title.bin"), []).unwrap();
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
    let root = common::temp_fixture_dir("query-lambda-bootstrap-ltembed-missing-bundle");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json(7),
    );

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
    let root = common::temp_fixture_dir("query-lambda-bootstrap-ltembed-missing-files");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json(7),
    );

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
    let root = common::temp_fixture_dir("query-lambda-bootstrap-real-router");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json(7),
    );
    common::write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    common::write_lance_fixture(
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
    let root = common::temp_fixture_dir("query-lambda-bootstrap-static-router");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json_with_dim(7, 512),
    );
    common::write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    common::write_lance_fixture_with_dim(
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
    let root = common::temp_fixture_dir("query-lambda-bootstrap-dim-mismatch");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json(7),
    );

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

    let root = common::temp_fixture_dir("query-lambda-bootstrap-ltembed-real-router");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json_with_dim(7, 384),
    );
    common::write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    common::write_lance_fixture_with_dim(
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

    let root = common::temp_fixture_dir("query-lambda-bootstrap-ltembed-dim-mismatch");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json_with_dim(7, 3),
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
    let root = common::temp_fixture_dir("query-lambda-bootstrap-version-mismatch");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(8));
    common::write_fixture(
        &root,
        &version_manifest_key(8),
        &common::sample_manifest_json(8),
    );

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
    let root = common::temp_fixture_dir("query-lambda-bootstrap-pinned-manifest");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json(7),
    );
    common::write_fixture(
        &root,
        &version_manifest_key(8),
        &common::sample_manifest_json(8),
    );
    common::write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust stable result"), ("doc-2", "rust backup")],
    );
    common::write_index(
        &root,
        "index/v8/shard_0",
        &[("doc-3", "rust upgraded result")],
    );
    common::write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id":"doc-1","text":"rust stable result","embedding":[0.9,0.0,0.0]}),
            json!({"doc_id":"doc-2","text":"rust backup","embedding":[0.8,0.0,0.0]}),
        ],
    );
    common::write_lance_fixture(
        &root,
        "lance/v8/shard_0",
        &[json!({"doc_id":"doc-3","text":"rust upgraded result","embedding":[0.95,0.0,0.0]})],
    );

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "fixed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var("LTSEARCH_QUERY_FIXED_EMBEDDING", "0.1,0.2,0.3");

    let handler = bootstrap_query_handler_for_version_from_env(7)
        .expect("expected version-pinned bootstrap to succeed");

    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(8));

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
