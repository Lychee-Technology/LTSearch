mod common;

#[cfg(feature = "ltembed")]
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use common::{padded_embedding, write_static_release_fixture, StaticFixtureDoc};
use ltsearch::error::{SearchError, ValidationError};
use ltsearch::models::{SearchRequest, SearchResponse};
use ltsearch::query_lambda::{
    bootstrap_query_handler_for_version_from_env, bootstrap_query_handler_from_env,
    handle_search_request,
};
use ltsearch::storage::{version_manifest_key, StaticReleaseHead, INDEX_HEAD_KEY, STATIC_HEAD_KEY};
use serde_json::json;

// 磁盘 fixture 构建器（temp dir / _head / manifest / tantivy / lance / 静态
// release）统一由 `tests/common` 提供，避免与 http_query_test 各持一份副本。
static QUERY_LAMBDA_ENV_LOCK: Mutex<()> = Mutex::new(());

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
        static_release_id: None,
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
    // 内容寻址布局：release 落 `<root>/static/releases/<id>/`，`static/_head`
    // 指针指向同一 id；bootstrap 经指针解析 release 并装载。
    let release_id = "a".repeat(64);
    write_static_release_fixture(
        &root,
        &release_id,
        &[StaticFixtureDoc {
            doc_id: 10,
            corpus_type: 0,
            text: "static legal ten",
            embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        }],
    );
    let static_head = StaticReleaseHead::new(release_id.clone(), 1_700_000_000_000);
    common::write_fixture(&root, STATIC_HEAD_KEY, &static_head.to_json_pretty());

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
    assert_eq!(response.static_release_id, Some(release_id));
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
