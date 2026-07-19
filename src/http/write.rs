//! write HTTP 服务的 router：`POST /write` 接受完整 tagged `WriteRequest`，
//! `POST /delete` 接受 `{"doc_ids":[...]}` 并包装为 `WriteRequest::Delete`，
//! `/health` 恒 200（write 无模型依赖，S3/SQS 故障在写请求路径上以错误信封报
//! 告）。见 docs/deployment.md。

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::future::BoxFuture;
use serde::Deserialize;

use crate::error::IngestError;
use crate::http::{error_response, health_response, HealthBody};
use crate::models::{DeleteResponse, Document, IngestResponse};
use crate::write_lambda::{handle_write_request, WriteRequest};

const COMPONENT: &str = "ltsearch-write";

/// ingest/delete 以装箱闭包为 state，与核心 `handle_write_request` 的闭包签名对
/// 齐：bin 侧把 `WriteApi::ingest/delete` 包成闭包，测试侧注入 stub。
pub type IngestFn = Arc<
    dyn Fn(Vec<Document>) -> BoxFuture<'static, Result<IngestResponse, IngestError>> + Send + Sync,
>;
pub type DeleteFn = Arc<
    dyn Fn(Vec<String>) -> BoxFuture<'static, Result<DeleteResponse, IngestError>> + Send + Sync,
>;

#[derive(Clone)]
pub struct WriteServerState {
    pub ingest: IngestFn,
    pub delete: DeleteFn,
}

pub fn write_router(state: WriteServerState) -> Router {
    Router::new()
        .route("/write", post(handle_write))
        .route("/delete", post(handle_delete))
        .route("/health", get(handle_health))
        .with_state(state)
}

async fn handle_write(State(state): State<WriteServerState>, body: axum::body::Bytes) -> Response {
    let request: WriteRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(source) => {
            return error_response(
                "validation_error",
                format!("failed to deserialize write request: {source}"),
            )
        }
    };

    dispatch(&state, request).await
}

#[derive(Debug, Deserialize)]
struct DeleteBody {
    doc_ids: Vec<String>,
}

async fn handle_delete(State(state): State<WriteServerState>, body: axum::body::Bytes) -> Response {
    let request: DeleteBody = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(source) => {
            return error_response(
                "validation_error",
                format!("failed to deserialize delete request: {source}"),
            )
        }
    };

    dispatch(
        &state,
        WriteRequest::Delete {
            doc_ids: request.doc_ids,
        },
    )
    .await
}

/// 用 state 中的闭包适配核心签名，错误信封映射同 write_lambda（Task 1）：
/// `validation_error`→400、其余→500。
async fn dispatch(state: &WriteServerState, request: WriteRequest) -> Response {
    let ingest = state.ingest.clone();
    let delete = state.delete.clone();
    let result = handle_write_request(
        async move |documents| ingest(documents).await,
        async move |doc_ids| delete(doc_ids).await,
        request,
    )
    .await;

    match result {
        Ok(response) => Json(response).into_response(),
        Err(error) => error_response(error.error_type, error.message),
    }
}

async fn handle_health() -> Response {
    health_response(HealthBody {
        status: "ok".into(),
        component: COMPONENT.into(),
        index_version: None,
        static_release_id: None,
        detail: None,
    })
}
