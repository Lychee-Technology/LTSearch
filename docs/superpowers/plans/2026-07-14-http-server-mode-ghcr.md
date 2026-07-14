# HTTP Server Mode + GHCR 镜像发布 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) 语法跟踪进度。

**Goal:** 按 `docs/deployment.md` 的目标设计，为 query / write / index_builder 三个组件各提供一个 axum HTTP 服务入口（`0.0.0.0:8080`，含 `/health`），并新增 GHA workflow 把三个 arm64 镜像发布到公开 GHCR，使下游（im4pe）能用 Docker Compose 直接编排 LTSearch。

**Architecture:** 三个请求核心（`handle_search_request` / `handle_write_request` / `handle_build_request`）已是传输无关的，HTTP 层只做薄封装。query 侧把目前埋在 `src/bin/query_lambda.rs` 里的 S3 制品同步 + 版本化 handler 缓存上提为可复用的 `QueryService`。index_builder 增加可选的 SQS 轮询循环，用「读 `_head` → head+1 → CAS publish」闭环解决 `docs/design.md` Known Gaps #1 的版本分配缺口。镜像不烘焙 LTEmbed 模型资产——运行时通过 `LTSEARCH_*_LTEMBED_BUNDLE_DIR` 指向挂载卷。

**Tech Stack:** Rust 1.94 / axum 0.8 / tokio / aws-sdk-{s3,sqs}（Moto 兼容）/ Docker（arm64）/ GitHub Actions（self-hosted ARM64 runner）→ GHCR。

## Global Constraints

- 仓库将转为 public；镜像推到 **公开 GHCR**：`ghcr.io/lychee-technology/ltsearch-{query,write,index-builder}-server`。
- 目标平台仅 **linux/arm64**（`ort` load-dynamic + 固定的 arm64 `libonnxruntime.so`，见 deployment.md）。
- 服务镜像 **不烘焙模型资产**；`ltembed` cargo feature 编译进二进制（`LTEMBED_MODE=real`），bundle 路径完全由运行时 env 决定。
- 每个 HTTP 服务监听 `0.0.0.0:${LTSEARCH_HTTP_PORT:-8080}`；错误响应保持既有信封 `{error_type, message}`：`validation_error` → 400，其余 → 500；`/health` 返回 200/503。
- 所有既有验证门必须过：`bash scripts/verify-fast.sh`（含 `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`）；Moto 相关跑 `scripts/verify-moto.sh`。
- 不改动三个 Lambda bin 的对外行为（`sam local` 流程照旧可用）。
- Commit message 风格延续仓库现状（`feat:` / `fix:` / `docs:` 前缀，一行摘要）。

---

### Task 1: axum 依赖 + `src/http` 公共模块（错误映射 / 健康信封 / serve 骨架）

**Files:**
- Modify: `Cargo.toml`（新增 axum、tokio features、dev-deps tower + http-body-util）
- Create: `src/http/mod.rs`
- Modify: `src/lib.rs`（导出 `pub mod http;`）
- Test: `tests/http_common_test.rs`

**Interfaces:**
- Produces:
  - `http::ErrorBody { error_type: String, message: String }`（serde 序列化，与 Lambda 信封字段一致）
  - `http::error_status(error_type: &str) -> axum::http::StatusCode`（`validation_error`→400，其余→500）
  - `http::HealthBody { status: String, component: String, index_version: Option<u64>, detail: Option<String> }`
  - `http::health_response(body: HealthBody) -> axum::response::Response`（`status=="ok"` → 200，否则 503）
  - `http::serve(router: axum::Router, port: u16) -> impl Future<Output = Result<(), std::io::Error>>`（绑定 `0.0.0.0:port`，SIGTERM/SIGINT 优雅退出）
  - `http::port_from_env() -> u16`（读 `LTSEARCH_HTTP_PORT`，默认 8080）

- [ ] **Step 1: 修改 Cargo.toml**

`[dependencies]` 增加：

```toml
axum = "0.8"
```

`tokio` 行改为：

```toml
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "net", "signal"] }
```

`[dev-dependencies]` 增加：

```toml
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
```

运行 `cargo fetch` 更新 `Cargo.lock`。

- [ ] **Step 2: 写失败测试 `tests/http_common_test.rs`**

```rust
use axum::http::StatusCode;
use ltsearch::http::{error_status, health_response, HealthBody};

#[test]
fn validation_error_maps_to_400_and_others_to_500() {
    assert_eq!(error_status("validation_error"), StatusCode::BAD_REQUEST);
    assert_eq!(error_status("execution_error"), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_status("operation_error"), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn health_response_uses_503_when_not_ok() {
    let ok = health_response(HealthBody {
        status: "ok".into(),
        component: "query".into(),
        index_version: Some(3),
        detail: None,
    });
    assert_eq!(ok.status(), StatusCode::OK);

    let bad = health_response(HealthBody {
        status: "unavailable".into(),
        component: "query".into(),
        index_version: None,
        detail: Some("LTEmbed bundle missing".into()),
    });
    assert_eq!(bad.status(), StatusCode::SERVICE_UNAVAILABLE);
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cargo test --test http_common_test`
Expected: 编译失败，`ltsearch::http` 不存在。

- [ ] **Step 4: 实现 `src/http/mod.rs` 并在 `src/lib.rs` 导出**

```rust
//! HTTP 服务模式的公共骨架：错误信封映射、健康响应、监听与优雅退出。
//! 见 docs/deployment.md「Mechanism: AWS Lambda Web Adapter」。

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
    if error_type == "validation_error" {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
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
```

`src/lib.rs` 增加一行 `pub mod http;`（按现有 mod 声明的字母序插入）。

- [ ] **Step 5: 运行测试通过**

Run: `cargo test --test http_common_test`
Expected: PASS（2 个测试）。

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/http/mod.rs src/lib.rs tests/http_common_test.rs
git commit -m "feat(http): axum 依赖与 HTTP 公共骨架 — 错误信封映射、健康响应、优雅退出"
```

---

### Task 2: `QueryService` — 上提 S3 制品同步与版本化 handler 缓存

**Files:**
- Create: `src/query_service.rs`
- Modify: `src/lib.rs`（`pub mod query_service;`）
- Modify: `src/bin/query_lambda.rs`（改为委托 `QueryService`，删除本地重复实现）
- Test: `tests/query_service_test.rs`

**Interfaces:**
- Consumes: `query_lambda.rs` 中既有的 `resolve_versioned_handler` / `resolve_versioned_handler_with_retry` / `sync_query_artifacts_from_s3_if_configured` / `sync_prefix` / `synced_artifact_prefixes` 逻辑（目前是 `src/bin/query_lambda.rs:40-206` 的私有函数，整体搬入本模块）；`ltsearch::query_lambda::{bootstrap_query_handler_for_version_from_env, load_active_query_version_from_env, is_retriable_bootstrap_version_change, SharedQueryRequestHandler, QueryLambdaError}`。
- Produces:
  - `pub struct QueryService { cache: std::sync::Mutex<Option<CachedQueryHandler>> }`
  - `impl QueryService { pub fn new() -> Self; pub async fn sync_artifacts_if_configured(&self) -> Result<(), String>; pub fn resolve_handler(&self) -> Result<SharedQueryRequestHandler, QueryLambdaError>; pub fn cached_version(&self) -> Option<u64>; }`
  - 泛型内部函数 `resolve_versioned_handler(_with_retry)` 保持原签名但移到本模块并 `pub(crate)`，单测直接搬迁。

- [ ] **Step 1: 新建 `tests/query_service_test.rs`（失败测试）**

把 `src/bin/query_lambda.rs:221-339` 的两个缓存行为测试（`versioned_cache_reuses_current_version_and_rebootstraps_after_version_change`、`versioned_cache_retries_once_when_bootstrap_loses_version_race`）搬为对 `ltsearch::query_service` 的集成测试，断言不变，另加：

```rust
#[test]
fn fresh_service_reports_no_cached_version() {
    let service = ltsearch::query_service::QueryService::new();
    assert_eq!(service.cached_version(), None);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --test query_service_test`
Expected: 编译失败，`query_service` 模块不存在。

- [ ] **Step 3: 实现 `src/query_service.rs`**

将 `src/bin/query_lambda.rs` 的 `CachedQueryHandler`、`resolve_versioned_handler`、`resolve_versioned_handler_with_retry`、`sync_query_artifacts_from_s3_if_configured`、`synced_artifact_prefixes`、`sync_prefix` 原样搬入（逻辑零改动），包装为：

```rust
pub struct QueryService {
    cache: Mutex<Option<CachedQueryHandler>>,
}

impl QueryService {
    pub fn new() -> Self {
        Self { cache: Mutex::new(None) }
    }

    pub async fn sync_artifacts_if_configured(&self) -> Result<(), String> {
        sync_query_artifacts_from_s3_if_configured().await
    }

    pub fn resolve_handler(&self) -> Result<SharedQueryRequestHandler, QueryLambdaError> {
        resolve_versioned_handler_with_retry(
            &self.cache,
            load_active_query_version_from_env,
            |expected_version| {
                bootstrap_query_handler_for_version_from_env(expected_version)
                    .map(SharedQueryRequestHandler::new)
            },
        )
    }

    pub fn cached_version(&self) -> Option<u64> {
        self.cache
            .lock()
            .expect("query handler cache lock poisoned")
            .as_ref()
            .map(|cached| cached.version_id)
    }
}
```

- [ ] **Step 4: `src/bin/query_lambda.rs` 委托 `QueryService`**

bin 保留 `decode_request_payload` 与 Lambda 信封；`static QUERY_HANDLER: OnceLock<Mutex<...>>` 改为 `static QUERY_SERVICE: OnceLock<QueryService>`；`function_handler` 调 `service.sync_artifacts_if_configured().await` + `service.resolve_handler()`。删除搬走的私有函数与已搬迁的单测（bin 内保留 `malformed_event_payload_returns_typed_error_envelope`）。

- [ ] **Step 5: 全量验证（含既有 query 测试回归）**

Run: `cargo test --test query_service_test --test query_lambda_test && cargo build --release --bin query_lambda`
Expected: 全 PASS，bin 编译通过。

- [ ] **Step 6: Commit**

```bash
git add src/query_service.rs src/lib.rs src/bin/query_lambda.rs tests/query_service_test.rs
git commit -m "refactor(query): S3 制品同步与版本化 handler 缓存上提为 QueryService，Lambda bin 改为委托"
```

---

### Task 3: query HTTP 服务 — `POST /query` + `GET /health`

**Files:**
- Create: `src/http/query.rs`（router 构建，可测试）
- Modify: `src/http/mod.rs`（`pub mod query;`）
- Create: `src/bin/query_server.rs`
- Test: `tests/http_query_test.rs`

**Interfaces:**
- Consumes: Task 1 的 `http::{error_response, health_response, HealthBody, serve, port_from_env}`；Task 2 的 `QueryService`；`ltsearch::models::{SearchRequest, SearchResponse}`；`ltsearch::query_lambda::handle_search_request`。
- Produces:
  - `pub struct QueryServerState { pub service: Arc<QueryService>, pub embedding_probe: Arc<dyn Fn() -> Result<usize, String> + Send + Sync> }`
  - `pub fn query_router(state: QueryServerState) -> axum::Router`
  - 健康语义：embedding probe 失败 → 503（detail 含 bundle 目录路径与「重新拉取 bundle 并重启」提示）；probe 成功且无 `_head`（空索引）→ 200 `index_version: null`；有 `_head` 但 handler bootstrap 失败 → 503。

- [ ] **Step 1: 写失败测试 `tests/http_query_test.rs`**

用 `tower::ServiceExt::oneshot` 直接打 router，不起真实监听。embedding probe 用闭包注入，`/query` handler 场景注入假 `QueryService` 不可行（其方法读 env + 文件系统），因此 `/query` 的成功/失败路径复用 `tests/query_lambda_test.rs` 的磁盘 fixture 构建函数（该文件里已有 `_head` + manifest + tantivy + lance 的 fixture helpers，按需要提为 `tests/common/mod.rs` 共享）。最小失败测试集：

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

use ltsearch::http::query::{query_router, QueryServerState};
use ltsearch::query_service::QueryService;

fn state_with_probe(probe: impl Fn() -> Result<usize, String> + Send + Sync + 'static) -> QueryServerState {
    QueryServerState {
        service: Arc::new(QueryService::new()),
        embedding_probe: Arc::new(probe),
    }
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "unavailable");
    assert!(json["detail"].as_str().unwrap().contains("model.ort missing"));
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error_type"], "validation_error");
}
```

另加一个用磁盘 fixture 的端到端 router 测试（fixed provider，套用 `query_lambda_test.rs` 的 `bootstrap_query_handler_from_env` 成功场景的 env/fixture 搭建方式）：`POST /query` 命中返回 200 且 `index_version` 正确、`GET /health` 返回 200 且 `index_version` 一致。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --test http_query_test`
Expected: 编译失败，`http::query` 不存在。

- [ ] **Step 3: 实现 `src/http/query.rs` 与 bin**

`src/http/query.rs`：

```rust
use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::http::{error_response, health_response, HealthBody};
use crate::models::SearchRequest;
use crate::query_lambda::handle_search_request;
use crate::query_service::QueryService;

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

async fn handle_query(
    State(state): State<QueryServerState>,
    body: axum::body::Bytes,
) -> Response {
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
    // 查询核心是同步 CPU-bound 调用，放到阻塞线程池执行。
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
        return health_response(HealthBody {
            status: "unavailable".into(),
            component: "ltsearch-query".into(),
            index_version: None,
            detail: Some(format!(
                "{detail}；请重新拉取 LTEmbed bundle 到挂载目录后重启容器"
            )),
        });
    }

    if let Err(error) = state.service.sync_artifacts_if_configured().await {
        return health_response(HealthBody {
            status: "unavailable".into(),
            component: "ltsearch-query".into(),
            index_version: None,
            detail: Some(format!("artifact sync failed: {error}")),
        });
    }

    // 空索引（尚无 _head）视为健康：模型可用即可服务，索引由写入侧驱动产生。
    match crate::query_lambda::load_active_query_version_from_env() {
        Ok(version) => {
            let service = state.service.clone();
            let resolved = tokio::task::spawn_blocking(move || service.resolve_handler()).await;
            match resolved {
                Ok(Ok(_)) => health_response(HealthBody {
                    status: "ok".into(),
                    component: "ltsearch-query".into(),
                    index_version: Some(version),
                    detail: None,
                }),
                Ok(Err(error)) => health_response(HealthBody {
                    status: "unavailable".into(),
                    component: "ltsearch-query".into(),
                    index_version: Some(version),
                    detail: Some(error.message),
                }),
                Err(join_error) => health_response(HealthBody {
                    status: "unavailable".into(),
                    component: "ltsearch-query".into(),
                    index_version: Some(version),
                    detail: Some(format!("health probe panicked: {join_error}")),
                }),
            }
        }
        Err(_) => health_response(HealthBody {
            status: "ok".into(),
            component: "ltsearch-query".into(),
            index_version: None,
            detail: Some("索引尚未发布（无 _head），等待首次导入".into()),
        }),
    }
}
```

注意：`load_active_query_version_from_env` 无法区分「_head 不存在」与「读取失败」时，检查 `ltsearch::query_lambda` 现有错误信息；若可区分（如 message 含 not found），读取失败应返回 503 而非 200——以实际错误类型为准实现并在测试中固化。

`src/bin/query_server.rs`：

```rust
use std::sync::Arc;

use ltsearch::http::query::{query_router, QueryServerState};
use ltsearch::http::{port_from_env, serve};
use ltsearch::query_service::QueryService;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let state = QueryServerState {
            service: Arc::new(QueryService::new()),
            embedding_probe: Arc::new(build_embedding_probe()),
        };
        let port = port_from_env();
        eprintln!("ltsearch-query-server listening on 0.0.0.0:{port}");
        serve(query_router(state), port).await?;
        Ok(())
    })
}

/// 启动时构建一次 probe 闭包；probe 本身按调用惰性初始化 embedding 引擎，
/// 避免模型损坏导致进程直接退出——健康检查需要能以 503 报告细节。
fn build_embedding_probe() -> impl Fn() -> Result<usize, String> + Send + Sync {
    use std::sync::OnceLock;
    static PROBE_RESULT: OnceLock<Result<usize, String>> = OnceLock::new();
    move || {
        PROBE_RESULT
            .get_or_init(|| {
                // 复用 query bootstrap 的 provider 选择与引擎构建路径：
                // provider=fixed → 返回固定向量维度；
                // provider=ltembed → OnnxEngine::from_bundle_dir + generate("healthcheck") 探针。
                // 具体调用 ltsearch::query_lambda 中现成的 env 引导函数（见
                // bootstrap_query_handler_from_env 内的 embedding 构建段），如该段
                // 未单独导出，则在 query_lambda.rs 补一个
                // pub fn probe_query_embedding_from_env() -> Result<usize, String>。
                ltsearch::query_lambda::probe_query_embedding_from_env()
            })
            .clone()
    }
}
```

需要在 `src/query_lambda.rs` 新增 `pub fn probe_query_embedding_from_env() -> Result<usize, String>`：提取现有 bootstrap（`src/query_lambda.rs:52-158`）中「读 provider env → 构建 generator → `generate` 探针 → 返回维度」的片段并复用，失败信息保留底层 `LTEmbed bootstrap failed: ...` 文本。

- [ ] **Step 4: 运行测试通过**

Run: `cargo test --test http_query_test && cargo build --release --bin query_server`
Expected: 全 PASS，bin 编译通过。

- [ ] **Step 5: Commit**

```bash
git add src/http/query.rs src/http/mod.rs src/query_lambda.rs src/bin/query_server.rs tests/http_query_test.rs
git commit -m "feat(http): query HTTP 服务 — POST /query 与含模型完整性探针的 GET /health"
```

---

### Task 4: write HTTP 服务 — `POST /write`、`POST /delete` + `GET /health`

**Files:**
- Create: `src/http/write.rs`
- Modify: `src/http/mod.rs`（`pub mod write;`）
- Create: `src/bin/write_server.rs`
- Test: `tests/http_write_test.rs`

**Interfaces:**
- Consumes: `ltsearch::write_lambda::{handle_write_request, WriteRequest}`；`ltsearch::write::api::WriteApi`、`ltsearch::adapters::{s3_wal::AwsS3WalStorage, sqs_build_queue::AwsSqsBuildQueue}`、`ltsearch::bootstrap::{WriteConfig, s3_client_from_env, sqs_client_from_env}`（接线照抄 `src/bin/write_lambda.rs:54-71`）。
- Produces:
  - `pub fn write_router<W>(api: W) -> axum::Router`，其中 `W: WriteApiHandlers`（见下）——router 对存储做泛型，测试注入 stub。
  - `pub trait WriteApiHandlers: Clone + Send + Sync + 'static { fn ingest(...); fn delete(...); }` 或直接以两个 `Arc<dyn Fn>` 闭包为 state（与 `handle_write_request` 的闭包风格一致，二选一，实现取更简者）。
  - `/write` 接受完整 tagged `WriteRequest`（`{"operation":"ingest",...}` 或 `{"operation":"delete",...}`）；`/delete` 接受 `{"doc_ids": [...]}` 并包装为 `WriteRequest::Delete`；`/health` 恒 200（write 无模型依赖；S3/SQS 故障由写请求路径报错）。

- [ ] **Step 1: 写失败测试 `tests/http_write_test.rs`**

参照 `tests/write_lambda_test.rs` 的 stub 闭包风格：

```rust
// stub ingest 返回 IngestResponse { accepted_count: 2, wal_event_ids: [...], batch_id: "b-1" }
// 1) POST /write（ingest 载荷）→ 200，body.accepted_count == 2
// 2) POST /write 畸形 body → 400 validation_error 信封
// 3) POST /delete {"doc_ids":["a","b"]} → 200，断言 stub delete 收到 ["a","b"]（AtomicBool/Mutex 捕获）
// 4) stub 返回 IngestError::Validation → 400；IngestError::Operation → 500
// 5) GET /health → 200，component == "ltsearch-write"
```

（测试代码按上述断言完整写出，风格与 `tests/write_lambda_test.rs` 一致。）

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --test http_write_test`
Expected: 编译失败。

- [ ] **Step 3: 实现 `src/http/write.rs` + `src/bin/write_server.rs`**

router 以两个闭包为 state（与核心签名对齐）：

```rust
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
```

`handle_write` 反序列化 `WriteRequest` 后调 `handle_write_request(ingest 闭包, delete 闭包, request)`，信封映射同 Task 1；`handle_delete` 反序列化 `#[derive(Deserialize)] struct DeleteBody { doc_ids: Vec<String> }` 包装为 `WriteRequest::Delete`。`futures::future::BoxFuture` 已有 `futures = "0.3"` 依赖可用。

bin `write_server.rs` 接线照抄 `src/bin/write_lambda.rs:56-63`（`WriteConfig::from_env` + `aws_config::load_defaults` + `AwsS3WalStorage` + `AwsSqsBuildQueue` + `WriteApi::new`），把 `write_api.ingest/delete` 包成两个闭包塞进 state，`serve(write_router(state), port_from_env()).await`。

- [ ] **Step 4: 运行测试通过**

Run: `cargo test --test http_write_test && cargo build --release --bin write_server`
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add src/http/write.rs src/http/mod.rs src/bin/write_server.rs tests/http_write_test.rs
git commit -m "feat(http): write HTTP 服务 — POST /write、POST /delete 与 GET /health"
```

---

### Task 5: index_builder HTTP 服务 — `POST /build` + `GET /health` + SQS 轮询闭环（head+1 版本分配）

**Files:**
- Create: `src/http/build.rs`
- Create: `src/build_worker.rs`（SQS 轮询循环 + 版本分配）
- Modify: `src/http/mod.rs`、`src/lib.rs`
- Create: `src/bin/index_builder_server.rs`
- Test: `tests/http_build_test.rs`、`tests/build_worker_test.rs`

**Interfaces:**
- Consumes: `handle_build_request`（`src/build_lambda.rs:58`）；build/publish 闭包接线照抄 `src/bin/index_builder_lambda.rs:50-101`；`IndexPublisher::publish` 的 CAS 语义（`src/indexing/publisher.rs:108-137`：`expected_current_version` 不匹配即失败、版本必须单调递增）；`AwsPublishStorage::read` + `INDEX_HEAD_KEY`（`src/storage/s3_paths.rs:1`）+ `ManifestHead::from_json`（`src/storage/head.rs:44`）。
- Produces:
  - `pub fn build_router(state: BuildServerState) -> axum::Router`：`POST /build`（显式 `BuildRequest`，行为同 Lambda）、`GET /health`（embedding probe：Document kind，语义同 Task 3 query 侧）。
  - `src/build_worker.rs`：
    - `pub struct QueueBuildMessage { pub batch_id: String, pub wal_key: String }`（serde Deserialize，对应 `AwsSqsBuildQueue` 发出的 body）
    - `pub async fn next_version_id(storage: &AwsPublishStorage) -> Result<(u64, Option<u64>), String>`——读 `_head`：存在 → `(head+1, Some(head))`；不存在 → `(1, None)`。
    - `pub async fn run_sqs_worker_loop(...)`：`receive_message`（long poll 10s）→ 解析 body → `next_version_id` → 组装 `BuildRequest { batch_id, wal_key, version_id, embedding_dim: env LTSEARCH_BUILD_EMBEDDING_DIM }` → 复用与 `POST /build` 相同的 `run_build(state, request)` → 成功/失败都 `delete_message` 并打日志（本地单用户场景不做毒消息隔离，失败详情必须完整落日志）。CAS 冲突（publish 返回 expected mismatch）重试一次（重读 head）。
  - 启用条件：env `LTSEARCH_BUILD_SQS_QUEUE_URL` 非空时 bin 内 `tokio::spawn` 该循环；未设置则仅提供 HTTP。

- [ ] **Step 1: 写失败测试**

`tests/build_worker_test.rs`：
- `queue_build_message_parses_body_from_sqs_batch`：用 `serde_json::from_str::<QueueBuildMessage>(r#"{"batch_id":"b-1","wal_key":"wal/x.jsonl","extra":"ignored"}"#)` 断言解析成功且忽略多余字段。
- `next_version_id` 的两种分支：由于 `AwsPublishStorage` 依赖真实 S3 客户端，把 `next_version_id` 写成泛型（参数为 `impl Fn() -> Result<Option<u64>, String>` 的 head 读取闭包，或复用 `PublishStorage` trait——`src/adapters/s3_publish.rs:36` 已有该 trait，用 in-memory 实现测试）：head 为 None → `(1, None)`；head 为 Some(7) → `(8, Some(7))`。

`tests/http_build_test.rs`：参照 `tests/index_builder_lambda_test.rs` 的 stub 闭包风格——`POST /build` 成功 → 200 envelope；build 失败 → 500 且 publish 未被调用（AtomicBool）；畸形 body → 400；`GET /health` 在 probe 失败时 503。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --test build_worker_test --test http_build_test`
Expected: 编译失败。

- [ ] **Step 3: 实现**

`src/build_worker.rs` 核心（`next_version_id` 以 `PublishStorage` trait 为参）：

```rust
pub async fn next_version_id<S: PublishStorage>(
    storage: &S,
) -> Result<(u64, Option<u64>), String> {
    let head_object = storage
        .read(INDEX_HEAD_KEY)
        .await
        .map_err(|error| format!("failed to read index head: {error}"))?;
    match head_object {
        None => Ok((1, None)),
        Some(object) => {
            let head = ManifestHead::from_json(&object.bytes)
                .map_err(|error| format!("failed to parse index head: {error}"))?;
            Ok((head.version_id + 1, Some(head.version_id)))
        }
    }
}
```

`run_sqs_worker_loop` 骨架：

```rust
pub async fn run_sqs_worker_loop(
    sqs: aws_sdk_sqs::Client,
    queue_url: String,
    state: BuildServerState,
) {
    loop {
        let received = sqs
            .receive_message()
            .queue_url(&queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(10)
            .send()
            .await;
        let messages = match received {
            Ok(output) => output.messages.unwrap_or_default(),
            Err(error) => {
                eprintln!("build worker: receive_message failed: {error}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        for message in messages {
            let outcome = process_queue_message(&state, message.body().unwrap_or_default()).await;
            if let Err(error) = &outcome {
                eprintln!("build worker: build failed (message dropped after logging): {error}");
            }
            if let Some(handle) = message.receipt_handle() {
                if let Err(error) = sqs
                    .delete_message()
                    .queue_url(&queue_url)
                    .receipt_handle(handle)
                    .send()
                    .await
                {
                    eprintln!("build worker: delete_message failed: {error}");
                }
            }
        }
    }
}
```

`process_queue_message`：解析 `QueueBuildMessage` → `next_version_id`（CAS 冲突重读一次）→ 读 env `LTSEARCH_BUILD_EMBEDDING_DIM`（缺失即报错返回）→ 复用 `POST /build` 的共享执行函数 `run_build(state, build_request)`（该函数封装 `handle_build_request` + `src/bin/index_builder_lambda.rs:50-101` 的两个闭包，publish 闭包中 `expected_current_version` 填 `next_version_id` 返回的旧 head 而非 `None`）。

`src/http/build.rs` 的 `BuildServerState` 与 `src/bin/index_builder_lambda.rs` 的 `BuildState` 同构（`BuildConfig` + `aws_sdk_s3::Client`），另携带 embedding probe 闭包（Document kind，复用 `bootstrap::{build_embedding_provider_from_env, build_embedding_generator_from_env}`，包装成 `probe_build_embedding_from_env() -> Result<usize, String>` 放 `src/bootstrap.rs`）。

bin `index_builder_server.rs`：构建 state → 若 `LTSEARCH_BUILD_SQS_QUEUE_URL` 设置则 `tokio::spawn(run_sqs_worker_loop(...))` → `serve(build_router(state), port_from_env())`。

- [ ] **Step 4: 运行测试通过**

Run: `cargo test --test build_worker_test --test http_build_test && cargo build --release --bin index_builder_server`
Expected: 全 PASS。

- [ ] **Step 5: 全量回归 + Commit**

Run: `bash scripts/verify-fast.sh`
Expected: 全绿（fmt/clippy/全部 bin 构建/既有测试）。

```bash
git add src/http/build.rs src/build_worker.rs src/http/mod.rs src/lib.rs src/bootstrap.rs src/bin/index_builder_server.rs tests/http_build_test.rs tests/build_worker_test.rs
git commit -m "feat(http): index_builder HTTP 服务与 SQS 轮询闭环 — head+1 版本分配 + CAS 发布（补 design.md Known Gaps #1）"
```

---

### Task 6: 服务镜像 Dockerfile ×3（不烘焙模型资产）

**Files:**
- Modify: `sam/builder.Dockerfile`（追加构建并导出三个 `*_server` 二进制）
- Create: `sam/query_server.Dockerfile`、`sam/write_server.Dockerfile`、`sam/index_builder_server.Dockerfile`

**Interfaces:**
- Consumes: 既有 builder 阶段镜像 `ltsearch-e2e-builder`（`sam/builder.Dockerfile`，arm64，`LTEMBED_MODE=real` 时以 `--features ltembed` 编译并需要 `.sam-local-deps/LTEmbed` checkout——staging 用 `scripts/e2e/lib.sh:125` 的 `prepare_local_ltembed_checkout`）。
- Produces: 三个运行镜像，`EXPOSE 8080`，`CMD ["/app/server"]`，内嵌 Lambda Web Adapter extension（deployment.md 的双运行时目标），**不含** `/ltembed-assets`。

- [ ] **Step 1: 修改 `sam/builder.Dockerfile`**

在现有 `cargo build` 完成后的二进制导出段（复制 `/query_lambda` 等三行附近）追加：

```dockerfile
RUN cp target/release/query_server /query_server \
 && cp target/release/write_server /write_server \
 && cp target/release/index_builder_server /index_builder_server
```

（`cargo build --release` 自动构建 `src/bin/` 全部 bin，无需改构建命令；确认 cache mount 场景下 `target/` 路径与现有 cp 写法一致，照抄现有行的路径风格。）

- [ ] **Step 2: 新建三个服务 Dockerfile**

`sam/query_server.Dockerfile`（write/index_builder 同构，仅二进制名不同）：

```dockerfile
FROM ltsearch-e2e-builder AS builder

FROM --platform=linux/arm64 public.ecr.aws/amazonlinux/amazonlinux:2023
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1 /lambda-adapter /opt/extensions/lambda-adapter
COPY --from=builder /query_server /app/server
ENV AWS_LWA_PORT=8080 \
    AWS_LWA_READINESS_CHECK_PATH=/health \
    LTSEARCH_HTTP_PORT=8080
EXPOSE 8080
CMD ["/app/server"]
```

- [ ] **Step 3: 本地构建验证（stub 模式即可验证 Dockerfile 正确性）**

Run:
```bash
docker build -f sam/builder.Dockerfile -t ltsearch-e2e-builder --build-arg LTEMBED_MODE=stub .
docker build -f sam/query_server.Dockerfile -t ltsearch-query-server:dev .
docker build -f sam/write_server.Dockerfile -t ltsearch-write-server:dev .
docker build -f sam/index_builder_server.Dockerfile -t ltsearch-index-builder-server:dev .
docker run --rm -d --name qs -p 18080:8080 -e LTSEARCH_QUERY_EMBEDDING_PROVIDER=fixed -e LTSEARCH_QUERY_FIXED_EMBEDDING=0.1,0.2,0.3 -e LTSEARCH_QUERY_ARTIFACT_ROOT=/tmp/artifacts ltsearch-query-server:dev
sleep 2 && curl -sf http://localhost:18080/health; docker rm -f qs
```
Expected: 四个镜像构建成功；`/health` 返回 200（fixed provider、空索引 → ok）。

- [ ] **Step 4: Commit**

```bash
git add sam/builder.Dockerfile sam/query_server.Dockerfile sam/write_server.Dockerfile sam/index_builder_server.Dockerfile
git commit -m "feat(docker): 三个 HTTP 服务镜像 — 复用 builder 阶段、不烘焙模型资产、内嵌 Web Adapter"
```

---

### Task 7: Compose HTTP 拓扑 + 端到端冒烟脚本

**Files:**
- Create: `docker-compose.http.yml`
- Create: `scripts/e2e/run-http-server-flow.sh`
- Modify: `.github/workflows/ci.yml`（新增 `http-e2e` job，`needs: integration`）

**Interfaces:**
- Consumes: Task 6 的三个镜像；`docker-compose.moto.yml` 的 moto 服务模式；`scripts/e2e/run-http-flow.sh` 的断言风格与 fixtures（`tests/fixtures/e2e/write_request.json`、`query_request.json`）。
- Produces: `docker compose -f docker-compose.http.yml up` 后 write→(SQS 自动 build)→query 全链路可用——这正是 im4pe S8 要编排的拓扑，先在本仓库验证。

- [ ] **Step 1: 写 `docker-compose.http.yml`**

```yaml
# HTTP 服务模式的本地端到端拓扑（fixed provider 版；ltembed 由调用脚本以
# LTSEARCH_E2E_LTEMBED=true 覆盖 env 与挂载）。moto 模拟 S3/SQS。
services:
  moto:
    image: motoserver/moto:latest
    environment:
      MOTO_PORT: "5000"
    networks: [ltsearch-http]

  aws-init:
    image: amazon/aws-cli:latest
    depends_on: [moto]
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        until aws --endpoint-url http://moto:5000 s3 mb s3://ltsearch-e2e 2>/dev/null; do sleep 1; done
        aws --endpoint-url http://moto:5000 sqs create-queue --queue-name ltsearch-build
    environment: &awsenv
      AWS_ACCESS_KEY_ID: test
      AWS_SECRET_ACCESS_KEY: test
      AWS_DEFAULT_REGION: us-east-1
    networks: [ltsearch-http]

  write:
    image: ltsearch-write-server:dev
    depends_on:
      aws-init: { condition: service_completed_successfully }
    environment:
      <<: *awsenv
      AWS_ENDPOINT_URL_S3: http://moto:5000
      AWS_ENDPOINT_URL_SQS: http://moto:5000
      LTSEARCH_WRITE_S3_BUCKET: ltsearch-e2e
      LTSEARCH_WRITE_SQS_QUEUE_URL: http://moto:5000/123456789012/ltsearch-build
    healthcheck: &hc
      test: ["CMD", "curl", "-sf", "http://localhost:8080/health"]
      interval: 5s
      timeout: 3s
      retries: 12
    networks: [ltsearch-http]

  index-builder:
    image: ltsearch-index-builder-server:dev
    depends_on:
      aws-init: { condition: service_completed_successfully }
    environment:
      <<: *awsenv
      AWS_ENDPOINT_URL_S3: http://moto:5000
      AWS_ENDPOINT_URL_SQS: http://moto:5000
      LTSEARCH_BUILD_S3_BUCKET: ltsearch-e2e
      LTSEARCH_BUILD_SQS_QUEUE_URL: http://moto:5000/123456789012/ltsearch-build
      LTSEARCH_BUILD_ARTIFACT_ROOT: /tmp/ltsearch
      LTSEARCH_BUILD_EMBEDDING_PROVIDER: fixed
      LTSEARCH_BUILD_FIXED_EMBEDDING: "0.1,0.2,0.3"
      LTSEARCH_BUILD_EMBEDDING_DIM: "3"
    healthcheck: *hc
    networks: [ltsearch-http]

  query:
    image: ltsearch-query-server:dev
    depends_on:
      aws-init: { condition: service_completed_successfully }
    environment:
      <<: *awsenv
      AWS_ENDPOINT_URL_S3: http://moto:5000
      LTSEARCH_QUERY_S3_BUCKET: ltsearch-e2e
      LTSEARCH_QUERY_ARTIFACT_ROOT: /tmp/artifacts
      LTSEARCH_QUERY_EMBEDDING_PROVIDER: fixed
      LTSEARCH_QUERY_FIXED_EMBEDDING: "0.1,0.2,0.3"
    ports:
      - "127.0.0.1:18080:8080" # 仅供本仓库冒烟脚本使用；im4pe 侧不映射任何端口
    healthcheck: *hc
    networks: [ltsearch-http]

networks:
  ltsearch-http:
    name: ltsearch-http
```

注意：curl 若不在 amazonlinux 基础镜像内，healthcheck 改为二进制自检子命令或 `wget`；实测后取可用者（amazonlinux:2023 默认含 curl-minimal）。write/query 端口映射按需——write 也需要被冒烟脚本访问，同样映射 `127.0.0.1:18081:8080`。

- [ ] **Step 2: 写 `scripts/e2e/run-http-server-flow.sh`**

参照 `run-http-flow.sh` 的断言，但不再手动搬运 SQS 消息（builder 自动轮询）：

```bash
#!/usr/bin/env bash
set -euo pipefail
# 前置：三个 :dev 镜像已构建；docker compose -f docker-compose.http.yml up -d --wait 已执行
BASE_WRITE="${LTSEARCH_E2E_WRITE_BASE:-http://localhost:18081}"
BASE_QUERY="${LTSEARCH_E2E_QUERY_BASE:-http://localhost:18080}"
FIXTURES="$(cd "$(dirname "$0")/../.." && pwd)/tests/fixtures/e2e"

curl -sf -X POST "$BASE_WRITE/write" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/write_request.json" | tee /tmp/write-resp.json
python3 -c 'import json;assert json.load(open("/tmp/write-resp.json"))["accepted_count"]==6'

# 等 builder 轮询消费并发布 v1（上限 120s）
for i in $(seq 1 60); do
  VERSION=$(curl -sf "$BASE_QUERY/health" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("index_version") or 0)')
  [ "$VERSION" -ge 1 ] && break
  sleep 2
done
[ "$VERSION" -ge 1 ] || { echo "index never became active"; exit 1; }

curl -sf -X POST "$BASE_QUERY/query" -H 'Content-Type: application/json' \
  -d @"$FIXTURES/query_request.json" | tee /tmp/query-resp.json
python3 - <<'PY'
import json
r = json.load(open("/tmp/query-resp.json"))
assert r["index_version"] >= 1, r
assert r["dynamic_count"] >= 1, r
assert "doc-rust-hybrid" in [c["doc_id"] for c in r["dynamic_chunks"]], r
print("HTTP server flow OK:", r["index_version"], r["dynamic_count"])
PY
```

- [ ] **Step 3: 本地跑通**

Run:
```bash
docker compose -f docker-compose.http.yml up -d --wait
bash scripts/e2e/run-http-server-flow.sh
docker compose -f docker-compose.http.yml down -v
```
Expected: 脚本输出 `HTTP server flow OK: 1 <n>`。

- [ ] **Step 4: CI job**

`.github/workflows/ci.yml` 追加（风格与现有 job 一致，self-hosted ARM64）：

```yaml
  http-e2e:
    needs: integration
    runs-on: [self-hosted, Linux, ARM64]
    steps:
      - uses: actions/checkout@v4
      - name: Build server images (stub embeddings)
        run: |
          docker build -f sam/builder.Dockerfile -t ltsearch-e2e-builder --build-arg LTEMBED_MODE=stub .
          docker build -f sam/query_server.Dockerfile -t ltsearch-query-server:dev .
          docker build -f sam/write_server.Dockerfile -t ltsearch-write-server:dev .
          docker build -f sam/index_builder_server.Dockerfile -t ltsearch-index-builder-server:dev .
      - name: Run HTTP flow
        run: |
          docker compose -f docker-compose.http.yml up -d --wait
          bash scripts/e2e/run-http-server-flow.sh
      - name: Teardown
        if: always()
        run: docker compose -f docker-compose.http.yml down -v
```

- [ ] **Step 5: Commit**

```bash
git add docker-compose.http.yml scripts/e2e/run-http-server-flow.sh .github/workflows/ci.yml
git commit -m "feat(e2e): Compose HTTP 拓扑与全自动 write→build→query 冒烟（builder SQS 轮询驱动）"
```

---

### Task 8: GHA workflow — 发布镜像到公开 GHCR

**Files:**
- Create: `.github/workflows/publish-images.yml`

**Interfaces:**
- Consumes: Task 6 的 Dockerfiles；`prepare_local_ltembed_checkout` 的语义（workflow 中直接 `git clone` LTEmbed 到 `.sam-local-deps/LTEmbed`，LTEmbed 是公开仓库）。
- Produces: `ghcr.io/lychee-technology/ltsearch-{query,write,index-builder}-server`，tag：`latest` + `sha-<short>`；`LTEMBED_MODE=real`（二进制含 ltembed feature，镜像不含模型资产）。

- [ ] **Step 1: 写 workflow**

```yaml
name: Publish Images

on:
  push:
    branches: [main]
    tags: ["v*"]
  workflow_dispatch:

permissions:
  contents: read
  packages: write

jobs:
  publish:
    runs-on: [self-hosted, Linux, ARM64]
    steps:
      - uses: actions/checkout@v4
      - name: Stage LTEmbed checkout for real-mode build
        run: git clone --depth 1 https://github.com/Lychee-Technology/LTEmbed .sam-local-deps/LTEmbed
      - name: Build builder image (real embeddings, arm64)
        run: docker build -f sam/builder.Dockerfile -t ltsearch-e2e-builder --build-arg LTEMBED_MODE=real .
      - name: Build server images
        run: |
          docker build -f sam/query_server.Dockerfile -t ghcr.io/lychee-technology/ltsearch-query-server:latest .
          docker build -f sam/write_server.Dockerfile -t ghcr.io/lychee-technology/ltsearch-write-server:latest .
          docker build -f sam/index_builder_server.Dockerfile -t ghcr.io/lychee-technology/ltsearch-index-builder-server:latest .
      - name: Login GHCR
        run: echo "${{ secrets.GITHUB_TOKEN }}" | docker login ghcr.io -u "${{ github.actor }}" --password-stdin
      - name: Tag with sha and push
        run: |
          SHORT_SHA="${GITHUB_SHA::7}"
          for name in query write index-builder; do
            image="ghcr.io/lychee-technology/ltsearch-${name}-server"
            docker tag "$image:latest" "$image:sha-$SHORT_SHA"
            docker push "$image:latest"
            docker push "$image:sha-$SHORT_SHA"
          done
```

- [ ] **Step 2: 语法验证**

Run: `python3 -c "import yaml,sys;yaml.safe_load(open('.github/workflows/publish-images.yml'))" && echo OK`
Expected: OK。（推送 main 后在 Actions 页确认真实运行；首次发布后需在 GitHub Packages 设置里把三个 package 可见性设为 public——这是一次性手工步骤，写入 PR 描述提醒。）

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/publish-images.yml
git commit -m "ci: 发布 query/write/index-builder HTTP 服务镜像到 GHCR（arm64，real embeddings，不含模型资产）"
```

---

### Task 9: 文档更新

**Files:**
- Modify: `README.md`（新增「HTTP Server Mode」小节：三个 bin、端点表、env 表、compose 冒烟命令、GHCR 镜像坐标与模型挂载说明）
- Modify: `docs/deployment.md`（顶部状态从「target / planned」改为「HTTP server mode 已实现（本地/Compose）；Fargate/Lambda 双运行时部署仍为规划」；「Open items」勾掉 HTTP entrypoints 与 SQS→build 两项）

**Interfaces:**
- Consumes: Task 3–8 的最终端点、env、镜像名。

- [ ] **Step 1: 更新两个文档**

README 新小节包含：

```markdown
## HTTP Server Mode

| Image (ghcr.io/lychee-technology/…) | Endpoints | Key env |
| --- | --- | --- |
| ltsearch-query-server | POST /query, GET /health | LTSEARCH_QUERY_{EMBEDDING_PROVIDER,S3_BUCKET,ARTIFACT_ROOT,LTEMBED_BUNDLE_DIR,LTEMBED_MODEL_PATH} |
| ltsearch-write-server | POST /write, POST /delete, GET /health | LTSEARCH_WRITE_{S3_BUCKET,SQS_QUEUE_URL} |
| ltsearch-index-builder-server | POST /build, GET /health（设 LTSEARCH_BUILD_SQS_QUEUE_URL 后自动轮询建索引） | LTSEARCH_BUILD_{S3_BUCKET,SQS_QUEUE_URL,ARTIFACT_ROOT,EMBEDDING_PROVIDER,EMBEDDING_DIM,LTEMBED_BUNDLE_DIR,LTEMBED_MODEL_PATH} |

镜像不内置 embedding 模型：`ltembed` 模式需把 LTEmbed bundle（model.ort / tokenizer.json /
build-info.json / libonnxruntime.so，来自 minimal-ort-builder release）挂载进容器并用
`*_LTEMBED_BUNDLE_DIR` / `*_LTEMBED_MODEL_PATH` 指向挂载路径；模型缺失或损坏时
`GET /health` 返回 503 并附修复提示。本地全链路验证：
`docker compose -f docker-compose.http.yml up -d --wait && bash scripts/e2e/run-http-server-flow.sh`
```

- [ ] **Step 2: Commit**

```bash
git add README.md docs/deployment.md
git commit -m "docs: HTTP server mode 使用说明与 deployment.md 状态更新"
```

---

## Self-Review 结论

- 覆盖 deployment.md 的三个 open item 中的前两个（HTTP entrypoints、SQS→build 触发与版本分配）；第三项（ECS/Lambda 基础设施模板）明确不在本计划范围。
- 下游 im4pe S8 需要的合同全部产出：三个 GHCR 镜像坐标、`/health` 的模型完整性语义（503 + detail）、模型运行时挂载 env、write→自动 build→query 的自驱动链路、compose 拓扑参考（Task 7 与 im4pe 侧编排同构）。
- 类型一致性检查：`error_type` 集合沿用现网三种（validation_error/execution_error/operation_error）；`HealthBody.index_version` 与 `SearchResponse.index_version` 均为 u64/Option<u64>；`QueueBuildMessage` 字段与 `AwsSqsBuildQueue.enqueue` 发出的 body 字段（batch_id/wal_key）一致（执行 Task 5 时以 `src/adapters/sqs_build_queue.rs:32-44` 实际序列化字段为准复核）。
- 已知留白（执行时按实际代码微调，不视为缺口）：`load_active_query_version_from_env` 对「无 _head」与「读取失败」的区分方式；amazonlinux 基础镜像的 healthcheck 命令可用性；builder cache mount 下 `target/` 导出路径。
