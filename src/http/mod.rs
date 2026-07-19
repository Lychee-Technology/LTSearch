//! HTTP 服务模式的公共骨架：错误信封映射、健康响应、监听与优雅退出。
//! 见 docs/deployment.md「Mechanism: AWS Lambda Web Adapter」。

pub mod build;
pub mod query;
pub mod write;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Router;
use serde::Serialize;
use tokio::net::TcpListener;

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error_type: String,
    pub message: String,
}

pub fn error_status(error_type: &str) -> StatusCode {
    StatusCode::from_u16(crate::lambda_events::status_code_for_error_type(error_type))
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

pub fn error_response(error_type: impl Into<String>, message: impl Into<String>) -> Response {
    let body = ErrorBody {
        error_type: error_type.into(),
        message: message.into(),
    };
    (error_status(&body.error_type), Json(body)).into_response()
}

#[derive(Debug, Serialize)]
pub struct HealthBody {
    pub status: String,
    pub component: String,
    pub index_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_release_id: Option<String>,
    pub detail: Option<String>,
}

pub fn health_response(body: HealthBody) -> Response {
    let code = if body.status == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body)).into_response()
}

pub fn port_from_env() -> u16 {
    std::env::var("LTSEARCH_HTTP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080)
}

pub async fn serve(router: Router, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
