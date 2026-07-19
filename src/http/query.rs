//! query HTTP 服务的 router：`POST /query` 走版本化 handler 缓存执行检索，
//! `GET /health` 先探测模型完整性再报告索引版本。见 docs/deployment.md。

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::http::{error_response, health_response, HealthBody};
use crate::models::SearchRequest;
use crate::query_lambda::handle_search_request;
use crate::query_service::QueryService;

const COMPONENT: &str = "ltsearch-query";

/// probe 闭包在 bin 侧惰性构建 embedding 引擎，测试侧注入假实现以固化健康语义。
#[derive(Clone)]
pub struct QueryServerState {
    pub service: Arc<QueryService>,
    pub embedding_probe: Arc<dyn Fn() -> Result<usize, String> + Send + Sync>,
}

pub fn query_router(state: QueryServerState) -> Router {
    Router::new()
        .route("/query", post(handle_query))
        .route("/health", get(handle_health))
        .with_state(state)
}

async fn handle_query(State(state): State<QueryServerState>, body: axum::body::Bytes) -> Response {
    let request: SearchRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(source) => {
            return error_response(
                "validation_error",
                format!("failed to deserialize search request: {source}"),
            )
        }
    };

    if let Err(error) = state.service.sync_artifacts_if_configured().await {
        return error_response(
            "execution_error",
            format!("query bootstrap failed: {error}"),
        );
    }

    let service = state.service.clone();
    // 查询核心是同步 CPU-bound 调用，放到阻塞线程池执行，避免占用 async 运行时。
    let result = tokio::task::spawn_blocking(move || {
        let handler = service.resolve_handler()?;
        handle_search_request(handler.as_ref(), request)
    })
    .await;

    match result {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error_response(error.error_type, error.message),
        Err(join_error) => error_response(
            "execution_error",
            format!("query task panicked: {join_error}"),
        ),
    }
}

async fn handle_health(State(state): State<QueryServerState>) -> Response {
    if let Err(detail) = (state.embedding_probe)() {
        // 底层 LTEmbed 错误只在「文件缺失 / build-info 读取解析失败」时才带 bundle
        // 路径；ORT runtime、配置校验、pooling/input_kind 不支持等失败并不含目录。
        // 若配置了 bundle dir，显式前缀之，便于定位到挂载目录。
        let detail = match std::env::var("LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR") {
            Ok(bundle_dir) if !bundle_dir.is_empty() => {
                format!("bundle_dir={bundle_dir}：{detail}")
            }
            _ => detail,
        };
        return unavailable(
            None,
            format!("{detail}；请重新拉取 LTEmbed bundle 到挂载目录后重启容器"),
        );
    }

    if let Err(error) = state.service.sync_artifacts_if_configured().await {
        return unavailable(None, format!("artifact sync failed: {error}"));
    }

    match crate::query_lambda::load_active_query_version_from_env_opt() {
        // 空索引（尚无 _head）视为健康：模型可用即可服务，索引由写入侧驱动产生。
        Ok(None) => health_response(HealthBody {
            status: "ok".into(),
            component: COMPONENT.into(),
            index_version: None,
            static_release_id: None,
            detail: Some("索引尚未发布（无 _head），等待首次导入".into()),
        }),
        Ok(Some(version)) => {
            let service = state.service.clone();
            let resolved = tokio::task::spawn_blocking(move || service.resolve_handler()).await;
            match resolved {
                Ok(Ok(_)) => health_response(HealthBody {
                    status: "ok".into(),
                    component: COMPONENT.into(),
                    index_version: Some(version),
                    // handler 已解析成功：直接读缓存键固定的 release id 上报，与本
                    // 请求装载的静态 release 一致（single-resolve consistent）。并发
                    // 重建间隙里 index_version 与 static_release_id 可能瞬时跨代，健康
                    // 端点仅作信息上报，不据此保证二者同代。指针不可读时上面的
                    // resolve_handler 已按硬错误走 503，不会到这里。
                    static_release_id: state.service.cached_static_release_id(),
                    detail: None,
                }),
                Ok(Err(error)) => unavailable(Some(version), error.message),
                Err(join_error) => unavailable(
                    Some(version),
                    format!("health probe panicked: {join_error}"),
                ),
            }
        }
        // `_head` 存在但读取失败等其他错误 → 503（区别于「无 _head」的健康态）。
        Err(error) => unavailable(None, error.message),
    }
}

fn unavailable(index_version: Option<u64>, detail: impl Into<String>) -> Response {
    health_response(HealthBody {
        status: "unavailable".into(),
        component: COMPONENT.into(),
        index_version,
        static_release_id: None,
        detail: Some(detail.into()),
    })
}
