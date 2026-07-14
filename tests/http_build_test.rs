//! index_builder HTTP 服务的 router 级测试：用 `tower::ServiceExt::oneshot`
//! 直接打 router，不起真实监听。build/publish 以 stub 闭包注入（参照
//! tests/index_builder_lambda_test.rs 的 stub 风格），覆盖成功信封、build 失败
//! 时不触发 publish、畸形载荷 400 与 /health 探针失败 503。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::FutureExt;
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use ltsearch::error::IndexError;
use ltsearch::http::build::{build_router, BuildServerState};
use ltsearch::indexing::{BuildIndexResult, PublishResult};
use ltsearch::models::{IndexManifest, ShardManifest};

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

fn sample_manifest() -> IndexManifest {
    IndexManifest {
        version_id: 1,
        created_at: 1_700_000_000_000,
        embedding_dim: 3,
        document_count: 1,
        num_shards: 1,
        shards: vec![ShardManifest {
            shard_id: 0,
            document_count: 1,
            lance_path: "s3://bucket/lance/v1/shard_0".into(),
            tantivy_path: "s3://bucket/index/v1/shard_0".into(),
        }],
    }
}

fn sample_build_body() -> serde_json::Value {
    json!({
        "batch_id": "batch-abc",
        "wal_key": "wal/2026/03/19/batch-abc.jsonl",
        "version_id": 1,
        "embedding_dim": 3,
    })
}

// 1) POST /build 成功 → 200 且 BuildResponse 信封字段正确。
#[tokio::test]
async fn build_returns_200_envelope_on_success() {
    let state = BuildServerState {
        build: Arc::new(|_request| {
            async {
                Ok(BuildIndexResult {
                    manifest: sample_manifest(),
                    documents: vec![],
                })
            }
            .boxed()
        }),
        publish: Arc::new(|_manifest, _expected| {
            async {
                Ok(PublishResult {
                    activated_version_id: 1,
                    previous_version_id: None,
                })
            }
            .boxed()
        }),
        embedding_probe: Arc::new(|| Ok(3)),
    };
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::post("/build")
                .header("content-type", "application/json")
                .body(Body::from(sample_build_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["activated_version_id"], 1);
    assert!(json["previous_version_id"].is_null());
    assert_eq!(json["document_count"], 0);
}

// 2) build 失败 → 500 且 publish 未被调用（AtomicBool）。
#[tokio::test]
async fn build_failure_returns_500_and_does_not_publish() {
    let publish_called = Arc::new(AtomicBool::new(false));
    let publish_flag = publish_called.clone();
    let state = BuildServerState {
        build: Arc::new(|_request| {
            async {
                Err(IndexError::Operation {
                    message: "disk full".into(),
                })
            }
            .boxed()
        }),
        publish: Arc::new(move |_manifest, _expected| {
            publish_flag.store(true, Ordering::SeqCst);
            async {
                Ok(PublishResult {
                    activated_version_id: 1,
                    previous_version_id: None,
                })
            }
            .boxed()
        }),
        embedding_probe: Arc::new(|| Ok(3)),
    };
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::post("/build")
                .header("content-type", "application/json")
                .body(Body::from(sample_build_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(response).await;
    assert_eq!(json["error_type"], "build_error");
    assert!(
        !publish_called.load(Ordering::SeqCst),
        "publish 不应在 build 失败时被调用"
    );
}

// 3) 畸形 body → 400 validation_error 信封。
#[tokio::test]
async fn build_with_malformed_body_returns_400_envelope() {
    let state = BuildServerState {
        build: Arc::new(|_request| {
            async { unreachable!("build should not run on malformed body") }.boxed()
        }),
        publish: Arc::new(|_manifest, _expected| {
            async { unreachable!("publish should not run on malformed body") }.boxed()
        }),
        embedding_probe: Arc::new(|| Ok(3)),
    };
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::post("/build")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"batch_id":"b-1","version_id":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = body_json(response).await;
    assert_eq!(json["error_type"], "validation_error");
}

// 4) GET /health 探针失败 → 503，detail 带 bundle_dir 前缀与整改提示。
#[tokio::test]
async fn health_returns_503_with_detail_when_embedding_probe_fails() {
    std::env::set_var("LTSEARCH_BUILD_LTEMBED_BUNDLE_DIR", "/models");
    let state = BuildServerState {
        build: Arc::new(|_request| async { unreachable!("build not called in health") }.boxed()),
        publish: Arc::new(|_m, _e| async { unreachable!("publish not called in health") }.boxed()),
        embedding_probe: Arc::new(|| Err("LTEmbed bootstrap failed: unsupported pooling".into())),
    };
    let app = build_router(state);
    let response = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(response).await;
    assert_eq!(json["status"], "unavailable");
    let detail = json["detail"].as_str().unwrap();
    assert!(detail.contains("bundle_dir=/models"), "detail: {detail}");
    assert!(detail.contains("unsupported pooling"), "detail: {detail}");
    std::env::remove_var("LTSEARCH_BUILD_LTEMBED_BUNDLE_DIR");
}

// 5) GET /health 探针通过 → 200，component 为 ltsearch-index-builder。
#[tokio::test]
async fn health_returns_200_when_probe_succeeds() {
    let state = BuildServerState {
        build: Arc::new(|_request| async { unreachable!("build not called in health") }.boxed()),
        publish: Arc::new(|_m, _e| async { unreachable!("publish not called in health") }.boxed()),
        embedding_probe: Arc::new(|| Ok(512)),
    };
    let app = build_router(state);
    let response = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["component"], "ltsearch-index-builder");
}
