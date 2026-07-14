//! write HTTP 服务的 router 级测试：用 `tower::ServiceExt::oneshot` 直接打
//! router，不起真实监听。ingest/delete 以 stub 闭包注入（参照
//! tests/write_lambda_test.rs 的 stub 风格），覆盖成功、畸形载荷、错误映射与
//! 健康语义。

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::FutureExt;
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use ltsearch::error::{IngestError, ValidationError};
use ltsearch::http::write::{write_router, WriteServerState};
use ltsearch::models::{DeleteResponse, IngestResponse};

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

/// stub ingest 恒返回固定 IngestResponse；stub delete 捕获收到的 doc_ids 并回
/// 一个空响应。二者足以覆盖成功路径与「delete 收到正确参数」的断言。
fn stub_state(captured: Arc<Mutex<Vec<String>>>) -> WriteServerState {
    WriteServerState {
        ingest: Arc::new(|documents| {
            let accepted_count = documents.len();
            async move {
                Ok(IngestResponse {
                    accepted_count,
                    wal_event_ids: vec!["evt-1".into(), "evt-2".into()],
                    batch_id: "b-1".into(),
                })
            }
            .boxed()
        }),
        delete: Arc::new(move |doc_ids| {
            let captured = captured.clone();
            async move {
                let accepted_count = doc_ids.len();
                *captured.lock().unwrap() = doc_ids;
                Ok(DeleteResponse {
                    accepted_count,
                    wal_event_ids: vec![],
                    batch_id: "b-del".into(),
                })
            }
            .boxed()
        }),
    }
}

fn sample_document() -> serde_json::Value {
    json!({
        "doc_id": "doc-1",
        "text": "hello world",
        "embedding": null,
        "metadata": {},
        "timestamp": 1_700_000_000_000i64,
    })
}

// 1) POST /write（ingest 载荷）→ 200，body.accepted_count == 2
#[tokio::test]
async fn write_ingest_returns_200_with_accepted_count() {
    let app = write_router(stub_state(Arc::new(Mutex::new(vec![]))));
    let payload = json!({
        "operation": "ingest",
        "documents": [sample_document(), sample_document()],
    });
    let response = app
        .oneshot(
            Request::post("/write")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["accepted_count"], 2);
    assert_eq!(json["batch_id"], "b-1");
}

// 2) POST /write 畸形 body → 400 validation_error 信封
#[tokio::test]
async fn write_with_malformed_body_returns_400_envelope() {
    let app = write_router(stub_state(Arc::new(Mutex::new(vec![]))));
    let response = app
        .oneshot(
            Request::post("/write")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"operation":"ingest","documents":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = body_json(response).await;
    assert_eq!(json["error_type"], "validation_error");
}

// 3) POST /delete {"doc_ids":["a","b"]} → 200，断言 stub delete 收到 ["a","b"]
#[tokio::test]
async fn delete_wraps_doc_ids_and_invokes_delete_handler() {
    let captured = Arc::new(Mutex::new(vec![]));
    let app = write_router(stub_state(captured.clone()));
    let response = app
        .oneshot(
            Request::post("/delete")
                .header("content-type", "application/json")
                .body(Body::from(json!({"doc_ids": ["a", "b"]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["accepted_count"], 2);
    assert_eq!(
        *captured.lock().unwrap(),
        vec!["a".to_string(), "b".to_string()]
    );
}

// 4a) stub ingest 返回 IngestError::Validation → 400 validation_error
#[tokio::test]
async fn write_ingest_validation_error_returns_400() {
    let state = WriteServerState {
        ingest: Arc::new(|_documents| {
            async {
                Err(IngestError::Validation(ValidationError::Required {
                    field: "documents",
                }))
            }
            .boxed()
        }),
        delete: Arc::new(|_doc_ids| async { unreachable!("delete should not be called") }.boxed()),
    };
    let app = write_router(state);
    let payload = json!({"operation": "ingest", "documents": [sample_document()]});
    let response = app
        .oneshot(
            Request::post("/write")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = body_json(response).await;
    assert_eq!(json["error_type"], "validation_error");
    assert_eq!(json["message"], "documents is required");
}

// 4b) stub ingest 返回 IngestError::Operation → 500 operation_error
#[tokio::test]
async fn write_ingest_operation_error_returns_500() {
    let state = WriteServerState {
        ingest: Arc::new(|_documents| {
            async {
                Err(IngestError::Operation {
                    message: "S3 write failed".into(),
                })
            }
            .boxed()
        }),
        delete: Arc::new(|_doc_ids| async { unreachable!("delete should not be called") }.boxed()),
    };
    let app = write_router(state);
    let payload = json!({"operation": "ingest", "documents": [sample_document()]});
    let response = app
        .oneshot(
            Request::post("/write")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(response).await;
    assert_eq!(json["error_type"], "operation_error");
    assert_eq!(json["message"], "ingest operation failed: S3 write failed");
}

// 5) GET /health → 200，component == "ltsearch-write"
#[tokio::test]
async fn health_returns_200_with_write_component() {
    let app = write_router(stub_state(Arc::new(Mutex::new(vec![]))));
    let response = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["component"], "ltsearch-write");
}
