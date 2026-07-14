//! query HTTP 服务的 router 级测试：用 `tower::ServiceExt::oneshot` 直接打
//! router，不起真实监听。embedding probe 用闭包注入以覆盖健康语义，`/query`
//! 的成功/失败路径复用磁盘 fixture（见 tests/common）。

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use ltsearch::http::query::{query_router, QueryServerState};
use ltsearch::query_service::QueryService;
use ltsearch::storage::{version_manifest_key, INDEX_HEAD_KEY};

fn state_with_probe(
    probe: impl Fn() -> Result<usize, String> + Send + Sync + 'static,
) -> QueryServerState {
    QueryServerState {
        service: Arc::new(QueryService::new()),
        embedding_probe: Arc::new(probe),
    }
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn health_returns_503_with_detail_when_embedding_probe_fails() {
    let app = query_router(state_with_probe(|| {
        Err("LTEmbed bootstrap failed: model.ort missing at /models".into())
    }));
    let response = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(response).await;
    assert_eq!(json["status"], "unavailable");
    assert!(json["detail"]
        .as_str()
        .unwrap()
        .contains("model.ort missing"));
}

#[tokio::test]
async fn query_with_malformed_body_returns_400_envelope() {
    let app = query_router(state_with_probe(|| Ok(3)));
    let response = app
        .oneshot(
            Request::post("/query")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"top_k":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = body_json(response).await;
    assert_eq!(json["error_type"], "validation_error");
}

// env 需在整段异步请求期间保持不变，故 guard 跨 await 持有以串行化用例。
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn query_and_health_serve_index_version_from_disk_fixture() {
    let _guard = common::ENV_LOCK.lock().unwrap();
    let root = common::temp_fixture_dir("http-query-router-e2e");
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
    std::env::remove_var("LTSEARCH_QUERY_S3_BUCKET");
    std::env::remove_var("LTSEARCH_QUERY_STATIC_DIR");

    // probe 复用 fixed provider 的固定向量维度（3），与 manifest 一致。
    let app = query_router(state_with_probe(|| Ok(3)));

    let query_response = app
        .clone()
        .oneshot(
            Request::post("/query")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "query": "rust",
                        "top_k": 2,
                        "filters": null,
                        "include_metadata": false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(query_response.status(), StatusCode::OK);
    let query_json = body_json(query_response).await;
    assert_eq!(query_json["index_version"], 7);
    assert_eq!(query_json["dynamic_count"], 2);

    let health_response = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health_response.status(), StatusCode::OK);
    let health_json = body_json(health_response).await;
    assert_eq!(health_json["status"], "ok");
    assert_eq!(health_json["index_version"], 7);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn health_reports_empty_index_as_ok_with_null_version() {
    let _guard = common::ENV_LOCK.lock().unwrap();
    let root = common::temp_fixture_dir("http-query-router-empty");
    // 不写 _head：空索引（新装/尚未导入）应视为健康。
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::remove_var("LTSEARCH_QUERY_S3_BUCKET");

    let app = query_router(state_with_probe(|| Ok(3)));
    let response = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "ok");
    assert!(json["index_version"].is_null());
}
