//! index_builder HTTP 服务的 router：`POST /build` 接受显式 `BuildRequest`
//! 并复用与 SQS worker 相同的 `run_build`（HTTP 侧 `expected_current_version`
//! 恒为 `None`，语义与 Lambda 今日一致）；`GET /health` 用 embedding 探针
//! （Document kind）报告构建侧模型完整性，语义同 Task 3 query 侧。见
//! docs/deployment.md。

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::future::BoxFuture;

use crate::build_lambda::{handle_build_request, BuildLambdaError, BuildRequest, BuildResponse};
use crate::error::{IndexError, PublishError};
use crate::http::{error_response, health_response, HealthBody};
use crate::indexing::{BuildIndexResult, PublishResult};
use crate::models::IndexManifest;

const COMPONENT: &str = "ltsearch-index-builder";

/// build 闭包读取 WAL、构建 embedding 引擎并生成分片索引，签名与核心
/// `handle_build_request` 的 `build_handler` 对齐。
pub type BuildFn = Arc<
    dyn Fn(BuildRequest) -> BoxFuture<'static, Result<BuildIndexResult, IndexError>> + Send + Sync,
>;
/// publish 闭包额外携带 `expected_current_version`：HTTP 侧传 `None`，worker
/// 侧传观测到的 head，从而复用同一发布路径而 CAS 语义各自正确。
pub type PublishFn = Arc<
    dyn Fn(IndexManifest, Option<u64>) -> BoxFuture<'static, Result<PublishResult, PublishError>>
        + Send
        + Sync,
>;

/// bin 侧把 `src/bin/index_builder_lambda.rs` 的 build/publish 接线包成闭包，
/// 测试侧注入 stub；probe 闭包惰性构建 embedding 引擎以固化健康语义。
#[derive(Clone)]
pub struct BuildServerState {
    pub build: BuildFn,
    pub publish: PublishFn,
    pub embedding_probe: Arc<dyn Fn() -> Result<usize, String> + Send + Sync>,
}

pub fn build_router(state: BuildServerState) -> Router {
    Router::new()
        .route("/build", post(handle_build))
        .route("/health", get(handle_health))
        .with_state(state)
}

/// HTTP 与 SQS worker 共享的执行核心：包装两个闭包喂给 `handle_build_request`，
/// publish 闭包按调用方意图注入 `expected_current_version`。
pub async fn run_build(
    state: &BuildServerState,
    request: BuildRequest,
    expected_current_version: Option<u64>,
) -> Result<BuildResponse, BuildLambdaError> {
    let build = state.build.clone();
    let publish = state.publish.clone();
    handle_build_request(
        move |request| build(request),
        move |manifest| publish(manifest, expected_current_version),
        request,
    )
    .await
}

async fn handle_build(State(state): State<BuildServerState>, body: axum::body::Bytes) -> Response {
    let request: BuildRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(source) => {
            return error_response(
                "validation_error",
                format!("failed to deserialize build request: {source}"),
            )
        }
    };

    // HTTP 侧显式 build（行为同 Lambda）：不做 head 乐观校验，publish 的 ETag
    // CAS 仍保证指针交换原子、且新版本必须大于当前活动版本。
    match run_build(&state, request, None).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => error_response(error.error_type, error.message),
    }
}

async fn handle_health(State(state): State<BuildServerState>) -> Response {
    if let Err(detail) = (state.embedding_probe)() {
        // 底层 LTEmbed 错误只在「文件缺失 / build-info 读取解析失败」时才带
        // bundle 路径；若配置了 bundle dir，显式前缀之以便定位到挂载目录。
        let detail = match std::env::var("LTSEARCH_BUILD_LTEMBED_BUNDLE_DIR") {
            Ok(bundle_dir) if !bundle_dir.is_empty() => {
                format!("bundle_dir={bundle_dir}：{detail}")
            }
            _ => detail,
        };
        return health_response(HealthBody {
            status: "unavailable".into(),
            component: COMPONENT.into(),
            index_version: None,
            detail: Some(format!(
                "{detail}；请重新拉取 LTEmbed bundle 到挂载目录后重启容器"
            )),
        });
    }

    health_response(HealthBody {
        status: "ok".into(),
        component: COMPONENT.into(),
        index_version: None,
        detail: None,
    })
}
