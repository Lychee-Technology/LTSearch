# Issue #109 Lambda ZIP 交付 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付 arm64 Lambda 自定义运行时 ZIP 产物：query/write 接 API Gateway HTTP API proxy 事件（payload v2），builder 接 SQS 批事件并以 partial-batch failure 报告逐条失败；新增生产 `template.yaml`（Zip / `provided.al2023` / arm64 / HTTP API / SQS EventSource 带 redrive）。

**Architecture:** 按 epic #132 拆 2 个 PR。PR-1 事件适配：新增纯 serde 的 `src/lambda_events.rs` 信封编解码模块（无 feature 门控、任何 profile 可单测），三个 lambda bin 换用信封；builder 复用 `build_worker::process_queue_message`（head 分配版本 + CAS 重试），接线闭包从 `index_builder_server` bin 抽到共享模块 `src/aws_wiring.rs`；两条 SAM e2e 与结构守卫同步切换信封。PR-2 打包 + SAM：在 AL2023 builder 镜像内编译（glibc 2.34 兼容 `provided.al2023`，不引入 cargo-lambda），提取二进制改名 `bootstrap` 置 zip 根；新增生产 `template.yaml` 与 ZIP 路径 e2e + CI job。

**Tech Stack:** Rust（serde/lambda_runtime 0.13/base64）、AWS SAM、moto、bash e2e、python unittest 结构守卫。

## Global Constraints

- `local` profile 必须保持 AWS-free（feature-matrix job 检查 `aws-config`/`aws-sdk-s3`/`aws-sdk-sqs`/`lambda_runtime` 不泄漏进 local 依赖图）——新模块 `lambda_events` 必须是纯 serde，不引 AWS 依赖。
- 所有 Lambda 产物 Linux arm64；CI 跑在 `ubuntu-24.04-arm`，但 ZIP 二进制必须在 AL2023 容器内编译（ubuntu 24.04 glibc 2.39 > provided.al2023 glibc 2.34，宿主机原生编译的动态链接二进制在 al2023 上会因 GLIBC 版本符号缺失而无法启动）。
- 不引入 cargo-lambda；打包 = `cargo build --release`（builder 镜像内已有）+ 改名 `bootstrap` + zip 根。
- builder 绝不手动 delete_message/ack；消息删除完全交给 Lambda SQS event source mapping（成功的平台删，失败的按 redrive 重投/进 DLQ）。
- SQS 队列 `VisibilityTimeout` ≥ 6 × builder 函数 Timeout（AWS ESM 最佳实践，"timeout-safe"）。
- PR 关联 issue；PR-1 正文写 `Part of #109`，PR-2 写 `Closes #109`。不自动合并。
- 每个 PR：清理已合并 PR 的本地分支 → ff main → 独立 worktree。
- 错误信封映射与 HTTP 服务模式保持一致：`validation_error`→400、其余→500（信封层新增 `not_found`→404 用于 write 未知路径）。

## 开工前置（每个 PR 各做一次）

- [ ] `git -C /Users/ruoshi/code/Lychee/LTBase/LTSearch branch --merged main`，删除已合并分支
- [ ] `git checkout main && git pull --ff-only`
- [ ] `git worktree add ../LTSearch-issue-109-pr1 -b feat/109-lambda-event-envelopes`（PR-2 用 `../LTSearch-issue-109-pr2 -b feat/109-lambda-zip-packaging`）

---

# PR-1：运行时事件适配（APIGW HTTP API v2 + SQS 批事件）

### Task 1: `src/lambda_events.rs` 信封编解码模块

**Files:**
- Create: `src/lambda_events.rs`
- Modify: `src/lib.rs`（注册模块）
- Modify: `Cargo.toml`（新增 `base64 = "0.22"` 非可选依赖——纯解码用，无 AWS 依赖）
- Modify: `src/http/mod.rs:20-26`（`error_status` 委托到共享映射，保持单一来源）

**Interfaces:**
- Produces（后续 Task 2/3/4 消费）:
  - `ApiGatewayV2Request { raw_path: String, body: Option<String>, is_base64_encoded: bool }` + `fn body_bytes(&self) -> Result<Vec<u8>, String>`
  - `ApiGatewayV2Response { status_code: u16, headers: BTreeMap<String,String>, body: String, is_base64_encoded: bool }` + `fn json(status_code: u16, payload: &impl Serialize) -> Self` + `fn error(error_type: &str, message: impl Into<String>) -> Self`
  - `fn status_code_for_error_type(error_type: &str) -> u16`
  - `SqsEvent { records: Vec<SqsRecord> }`、`SqsRecord { message_id: String, body: String }`
  - `SqsBatchResponse { batch_item_failures: Vec<SqsBatchItemFailure> }`、`SqsBatchItemFailure { item_identifier: String }`
  - `async fn process_sqs_records<F: AsyncFnMut(&SqsRecord) -> Result<(), String>>(event: SqsEvent, process: F) -> SqsBatchResponse`

- [ ] **Step 1: 写失败测试（模块内 `#[cfg(test)]`，先建带测试的空实现骨架会编译失败，直接写完整模块 + 测试再跑）**

创建 `src/lambda_events.rs`：

```rust
//! Lambda 事件信封编解码：API Gateway HTTP API（payload v2）与 SQS 批事件的
//! 最小 serde 类型 + 传输中立的处理助手。纯 serde、无 AWS 依赖、不设 feature
//! 门控，任何 profile 下都可单测；lambda bin 负责把信封接到各自核心。
//! 只声明我们消费的字段子集，serde 默认忽略其余字段。

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// `validation_error`→400、`not_found`→404、其余→500。HTTP 服务模式的
/// `crate::http::error_status` 委托到这里，保证两种传输形态映射一致。
pub fn status_code_for_error_type(error_type: &str) -> u16 {
    match error_type {
        "validation_error" => 400,
        "not_found" => 404,
        _ => 500,
    }
}

/// API Gateway HTTP API proxy 事件（payload format 2.0）的最小子集。
#[derive(Debug, Clone, Deserialize)]
pub struct ApiGatewayV2Request {
    #[serde(rename = "rawPath", default)]
    pub raw_path: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(rename = "isBase64Encoded", default)]
    pub is_base64_encoded: bool,
}

impl ApiGatewayV2Request {
    /// 取出请求体字节：`isBase64Encoded` 时先解 base64；无 body 视为空。
    pub fn body_bytes(&self) -> Result<Vec<u8>, String> {
        let Some(body) = &self.body else {
            return Ok(Vec::new());
        };
        if self.is_base64_encoded {
            base64::engine::general_purpose::STANDARD
                .decode(body)
                .map_err(|error| format!("invalid base64 request body: {error}"))
        } else {
            Ok(body.clone().into_bytes())
        }
    }
}

/// payload format 2.0 的 Lambda→API Gateway 响应结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiGatewayV2Response {
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    #[serde(rename = "isBase64Encoded")]
    pub is_base64_encoded: bool,
}

impl ApiGatewayV2Response {
    pub fn json(status_code: u16, payload: &impl Serialize) -> Self {
        let body = serde_json::to_string(payload).unwrap_or_else(|error| {
            format!(
                r#"{{"error_type":"execution_error","message":"failed to serialize response: {error}"}}"#
            )
        });
        Self {
            status_code,
            headers: BTreeMap::from([(
                "content-type".to_string(),
                "application/json".to_string(),
            )]),
            body,
            is_base64_encoded: false,
        }
    }

    pub fn error(error_type: &str, message: impl Into<String>) -> Self {
        #[derive(Serialize)]
        struct ErrorBody<'a> {
            error_type: &'a str,
            message: String,
        }
        Self::json(
            status_code_for_error_type(error_type),
            &ErrorBody {
                error_type,
                message: message.into(),
            },
        )
    }
}

/// SQS 触发事件的最小子集（`Records[].messageId/body`）。
#[derive(Debug, Clone, Deserialize)]
pub struct SqsEvent {
    #[serde(rename = "Records", default)]
    pub records: Vec<SqsRecord>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SqsRecord {
    #[serde(rename = "messageId")]
    pub message_id: String,
    #[serde(default)]
    pub body: String,
}

/// Lambda partial-batch failure 响应（`ReportBatchItemFailures`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SqsBatchResponse {
    #[serde(rename = "batchItemFailures")]
    pub batch_item_failures: Vec<SqsBatchItemFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SqsBatchItemFailure {
    #[serde(rename = "itemIdentifier")]
    pub item_identifier: String,
}

/// 逐条处理 SQS 批记录，失败的进 `batchItemFailures`：成功的消息由平台删除，
/// 失败的按队列 redrive 策略重投/进 DLQ。绝不手动 delete_message。失败详情落
/// stderr（与 build worker 的日志惯例一致）。
pub async fn process_sqs_records<F>(event: SqsEvent, mut process: F) -> SqsBatchResponse
where
    F: AsyncFnMut(&SqsRecord) -> Result<(), String>,
{
    let mut batch_item_failures = Vec::new();
    for record in &event.records {
        if let Err(error) = process(record).await {
            eprintln!(
                "lambda_events: sqs record {} failed: {error}",
                record.message_id
            );
            batch_item_failures.push(SqsBatchItemFailure {
                item_identifier: record.message_id.clone(),
            });
        }
    }
    SqsBatchResponse {
        batch_item_failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取自 AWS 官方文档的 payload 2.0 示例事件裁剪版：验证字段名逐字正确。
    #[test]
    fn apigw_v2_event_parses_raw_path_and_plain_body() {
        let event: ApiGatewayV2Request = serde_json::from_str(
            r#"{
                "version": "2.0",
                "routeKey": "POST /write",
                "rawPath": "/write",
                "rawQueryString": "",
                "headers": {"content-type": "application/json"},
                "requestContext": {"http": {"method": "POST", "path": "/write"}},
                "body": "{\"top_k\":3}",
                "isBase64Encoded": false
            }"#,
        )
        .expect("payload v2 event should parse");

        assert_eq!(event.raw_path, "/write");
        assert_eq!(event.body_bytes().unwrap(), br#"{"top_k":3}"#.to_vec());
    }

    #[test]
    fn apigw_v2_event_decodes_base64_body() {
        let event: ApiGatewayV2Request = serde_json::from_str(
            r#"{"rawPath": "/query", "body": "eyJxdWVyeSI6InJ1c3QifQ==", "isBase64Encoded": true}"#,
        )
        .expect("event should parse");

        assert_eq!(event.body_bytes().unwrap(), br#"{"query":"rust"}"#.to_vec());
    }

    #[test]
    fn apigw_v2_event_rejects_invalid_base64_body() {
        let event: ApiGatewayV2Request = serde_json::from_str(
            r#"{"rawPath": "/query", "body": "!!not-base64!!", "isBase64Encoded": true}"#,
        )
        .expect("event should parse");

        let error = event.body_bytes().expect_err("invalid base64 must fail");
        assert!(error.contains("invalid base64 request body"));
    }

    #[test]
    fn apigw_v2_event_without_body_yields_empty_bytes() {
        let event: ApiGatewayV2Request =
            serde_json::from_str(r#"{"rawPath": "/query"}"#).expect("event should parse");
        assert_eq!(event.body_bytes().unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn json_response_serializes_camel_case_envelope() {
        #[derive(Serialize)]
        struct Payload {
            ok: bool,
        }
        let response = ApiGatewayV2Response::json(200, &Payload { ok: true });
        let value = serde_json::to_value(&response).unwrap();

        assert_eq!(value["statusCode"], 200);
        assert_eq!(value["isBase64Encoded"], false);
        assert_eq!(value["headers"]["content-type"], "application/json");
        assert_eq!(value["body"], r#"{"ok":true}"#);
    }

    #[test]
    fn error_response_maps_error_types_to_http_status() {
        assert_eq!(ApiGatewayV2Response::error("validation_error", "x").status_code, 400);
        assert_eq!(ApiGatewayV2Response::error("not_found", "x").status_code, 404);
        assert_eq!(ApiGatewayV2Response::error("execution_error", "x").status_code, 500);
        assert_eq!(ApiGatewayV2Response::error("publish_error", "x").status_code, 500);
    }

    /// 取自 AWS 官方文档的 SQS 事件裁剪版：验证 `Records`/`messageId` 字段名。
    #[test]
    fn sqs_event_parses_records() {
        let event: SqsEvent = serde_json::from_str(
            r#"{
                "Records": [
                    {
                        "messageId": "059f36b4-87a3-44ab-83d2-661975830a7d",
                        "receiptHandle": "AQEBwJnKyrHigUMZj6rYigCgxlaS3SLy0a...",
                        "body": "{\"batch_id\":\"b-1\",\"wal_key\":\"wal/x.jsonl\"}",
                        "attributes": {"ApproximateReceiveCount": "1"},
                        "eventSource": "aws:sqs"
                    }
                ]
            }"#,
        )
        .expect("sqs event should parse");

        assert_eq!(event.records.len(), 1);
        assert_eq!(
            event.records[0].message_id,
            "059f36b4-87a3-44ab-83d2-661975830a7d"
        );
        assert!(event.records[0].body.contains("wal/x.jsonl"));
    }

    #[tokio::test]
    async fn process_sqs_records_reports_only_failed_message_ids() {
        let event: SqsEvent = serde_json::from_str(
            r#"{"Records": [
                {"messageId": "ok-1", "body": "good"},
                {"messageId": "bad-2", "body": "bad"},
                {"messageId": "ok-3", "body": "good"}
            ]}"#,
        )
        .unwrap();

        let response = process_sqs_records(event, async |record: &SqsRecord| {
            if record.body == "bad" {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        })
        .await;

        assert_eq!(
            response,
            SqsBatchResponse {
                batch_item_failures: vec![SqsBatchItemFailure {
                    item_identifier: "bad-2".to_string()
                }]
            }
        );
    }

    #[tokio::test]
    async fn process_sqs_records_empty_batch_yields_empty_failures() {
        let event: SqsEvent = serde_json::from_str(r#"{"Records": []}"#).unwrap();
        let response = process_sqs_records(event, async |_record: &SqsRecord| Ok(())).await;
        assert!(response.batch_item_failures.is_empty());

        // 序列化形状恰为 {"batchItemFailures":[]}——Lambda ESM 以此判定全批成功。
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"batchItemFailures":[]}"#
        );
    }
}
```

`src/lib.rs` 在 `pub mod indexing;` 后按字母序插入：

```rust
pub mod lambda_events;
```

`Cargo.toml` `[dependencies]` 中 `aws-sdk-sqs` 行后插入：

```toml
base64 = "0.22"
```

- [ ] **Step 2: 跑测试验证失败/通过**

Run: `cargo test --lib lambda_events -- --nocapture`
Expected: 首次因缺依赖/模块编译错误 → 加上 Step 1 全部内容后 9 个测试全 PASS

- [ ] **Step 3: `src/http/mod.rs` 的 `error_status` 委托共享映射**

替换 `src/http/mod.rs:20-26` 的函数体：

```rust
pub fn error_status(error_type: &str) -> StatusCode {
    StatusCode::from_u16(crate::lambda_events::status_code_for_error_type(error_type))
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}
```

- [ ] **Step 4: 验证 local profile 无 AWS 泄漏 + 全量测试**

Run: `cargo build --no-default-features --features local && cargo test --no-default-features --features local --lib`
Expected: 编译通过，`lambda_events` 测试在 local profile 下运行且 PASS

- [ ] **Step 5: Commit**

```bash
git add src/lambda_events.rs src/lib.rs src/http/mod.rs Cargo.toml Cargo.lock
git commit -m "feat(lambda): add transport-neutral APIGW v2 + SQS event envelope codec"
```

### Task 2: query_lambda bin 接 APIGW v2 信封

**Files:**
- Modify: `src/bin/query_lambda.rs`（整文件替换）

**Interfaces:**
- Consumes: Task 1 的 `ApiGatewayV2Request/Response`；既有 `ltsearch::query_lambda::handle_search_request`、`QueryService::{new, sync_artifacts_if_configured, resolve_handler}`
- Produces: 函数响应恒为 APIGW v2 信封 JSON（`statusCode`/`body`），成功 200 + `SearchResponse`，失败按 `status_code_for_error_type`

- [ ] **Step 1: 整文件替换 `src/bin/query_lambda.rs`（含失败测试）**

```rust
use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::lambda_events::{ApiGatewayV2Request, ApiGatewayV2Response};
use ltsearch::models::SearchRequest;
use ltsearch::query_lambda::handle_search_request;
use ltsearch::query_service::QueryService;
use serde_json::Value;
use std::sync::OnceLock;

static QUERY_SERVICE: OnceLock<QueryService> = OnceLock::new();

/// 事件 → SearchRequest：信封解析失败与 body 反序列化失败都折叠成 400 响应。
fn decode_search_request(payload: Value) -> Result<SearchRequest, ApiGatewayV2Response> {
    let event: ApiGatewayV2Request = serde_json::from_value(payload).map_err(|source| {
        ApiGatewayV2Response::error(
            "validation_error",
            format!("failed to deserialize API Gateway event: {source}"),
        )
    })?;
    let bytes = event
        .body_bytes()
        .map_err(|error| ApiGatewayV2Response::error("validation_error", error))?;
    serde_json::from_slice(&bytes).map_err(|source| {
        ApiGatewayV2Response::error(
            "validation_error",
            format!("failed to deserialize search request: {source}"),
        )
    })
}

async fn function_handler(event: LambdaEvent<Value>) -> Result<ApiGatewayV2Response, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_search_request(payload) {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };

    let service = QUERY_SERVICE.get_or_init(QueryService::new);

    if let Err(error) = service.sync_artifacts_if_configured().await {
        return Ok(ApiGatewayV2Response::error(
            "execution_error",
            format!("query lambda bootstrap failed: {error}"),
        ));
    }

    let response = match service.resolve_handler() {
        Ok(handler) => match handle_search_request(handler.as_ref(), request) {
            Ok(response) => ApiGatewayV2Response::json(200, &response),
            Err(error) => ApiGatewayV2Response::error(&error.error_type, error.message),
        },
        Err(error) => ApiGatewayV2Response::error(&error.error_type, error.message),
    };

    Ok(response)
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?
        .block_on(async { lambda_runtime::run(service_fn(function_handler)).await })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_apigw_event_decodes_search_request() {
        let payload = json!({
            "version": "2.0",
            "rawPath": "/query",
            "body": "{\"query\":\"rust retrieval\",\"top_k\":3,\"filters\":null,\"include_metadata\":true}",
            "isBase64Encoded": false
        });

        let request = decode_search_request(payload).expect("valid event should decode");
        assert_eq!(request.query, "rust retrieval");
        assert_eq!(request.top_k, 3);
    }

    #[test]
    fn malformed_body_returns_400_envelope() {
        let payload = json!({"rawPath": "/query", "body": "{\"top_k\": \"wrong\"}"});

        let response = decode_search_request(payload).expect_err("must produce error envelope");
        assert_eq!(response.status_code, 400);
        assert!(response.body.contains("validation_error"));
        assert!(response.body.contains("failed to deserialize search request"));
    }

    #[test]
    fn non_apigw_event_returns_400_envelope() {
        // 直调裸 JSON（旧契约）不再被接受：信封字段类型不匹配 → 400。
        let response = decode_search_request(json!({"body": 42}))
            .expect_err("bare payload must be rejected");
        assert_eq!(response.status_code, 400);
        assert!(response.body.contains("failed to deserialize API Gateway event"));
    }
}
```

注意：若 `SearchRequest` 字段访问（`request.query`/`request.top_k`）与 `src/models` 定义不符，以 `src/models` 实际字段为准调整断言（`tests/fixtures/e2e/query_request.json` 显示形状为 `{query, top_k, filters, include_metadata}`）。

- [ ] **Step 2: 跑 bin 单测**

Run: `cargo test --no-default-features --features lambda --bin query_lambda`
Expected: 3 个测试 PASS

- [ ] **Step 3: Commit**

```bash
git add src/bin/query_lambda.rs
git commit -m "feat(lambda): query_lambda accepts API Gateway HTTP API v2 proxy events"
```

### Task 3: write_lambda bin 接 APIGW v2 信封（/write + /delete 路由）

**Files:**
- Modify: `src/bin/write_lambda.rs`（整文件替换）

**Interfaces:**
- Consumes: Task 1 信封类型；既有 `handle_write_request`、`WriteRequest::{Ingest, Delete}`、`WriteApi`/`AwsS3WalStorage`/`AwsSqsBuildQueue` 接线（`main` 不变）
- Produces: `/write` 收 tagged `WriteRequest`，`/delete` 收 `{"doc_ids":[...]}`（与 `src/http/write.rs` 的 HTTP 服务契约对齐）；未知路径 404 `not_found`

- [ ] **Step 1: 整文件替换 `src/bin/write_lambda.rs`**

```rust
use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;
use ltsearch::bootstrap::{s3_client_from_env, sqs_client_from_env, WriteConfig};
use ltsearch::lambda_events::{ApiGatewayV2Request, ApiGatewayV2Response};
use ltsearch::write::api::WriteApi;
use ltsearch::write::wal::WriteAheadLog;
use ltsearch::write_lambda::{handle_write_request, WriteRequest};
use serde::Deserialize;
use serde_json::Value;

type ProdWriteApi = WriteApi<AwsS3WalStorage, AwsSqsBuildQueue>;

/// `/delete` 的请求体（与 `src/http/write.rs` 的 `DeleteBody` 同构）。
#[derive(Debug, Deserialize)]
struct DeleteBody {
    doc_ids: Vec<String>,
}

/// 事件 → WriteRequest：按 rawPath 分派 `/write`（tagged WriteRequest）与
/// `/delete`（doc_ids 包装为 Delete），与 HTTP 服务模式的双路由契约对齐。
fn decode_write_request(payload: Value) -> Result<WriteRequest, ApiGatewayV2Response> {
    let event: ApiGatewayV2Request = serde_json::from_value(payload).map_err(|source| {
        ApiGatewayV2Response::error(
            "validation_error",
            format!("failed to deserialize API Gateway event: {source}"),
        )
    })?;
    let bytes = event
        .body_bytes()
        .map_err(|error| ApiGatewayV2Response::error("validation_error", error))?;

    if event.raw_path.ends_with("/delete") {
        let body: DeleteBody = serde_json::from_slice(&bytes).map_err(|source| {
            ApiGatewayV2Response::error(
                "validation_error",
                format!("failed to deserialize delete request: {source}"),
            )
        })?;
        Ok(WriteRequest::Delete {
            doc_ids: body.doc_ids,
        })
    } else if event.raw_path.ends_with("/write") {
        serde_json::from_slice(&bytes).map_err(|source| {
            ApiGatewayV2Response::error(
                "validation_error",
                format!("failed to deserialize write request: {source}"),
            )
        })
    } else {
        Err(ApiGatewayV2Response::error(
            "not_found",
            format!("unsupported path: {}", event.raw_path),
        ))
    }
}

async fn function_handler(
    write_api: ProdWriteApi,
    event: LambdaEvent<Value>,
) -> Result<ApiGatewayV2Response, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_write_request(payload) {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };

    let result = handle_write_request(
        async |documents| write_api.ingest(documents).await,
        async |doc_ids| write_api.delete(doc_ids).await,
        request,
    )
    .await;

    let response = match result {
        Ok(response) => ApiGatewayV2Response::json(200, &response),
        Err(error) => ApiGatewayV2Response::error(&error.error_type, error.message),
    };

    Ok(response)
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let write_config = WriteConfig::from_env()?;
        let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

        let wal_storage =
            AwsS3WalStorage::new(write_config.s3_bucket, s3_client_from_env(&sdk_config));
        let build_queue =
            AwsSqsBuildQueue::new(write_config.sqs_queue_url, sqs_client_from_env(&sdk_config));
        let write_api = WriteApi::new(WriteAheadLog::new(wal_storage), build_queue);

        lambda_runtime::run(service_fn(move |event| {
            let write_api = write_api.clone();
            async move { function_handler(write_api, event).await }
        }))
        .await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn write_path_decodes_tagged_ingest_request() {
        let payload = json!({
            "rawPath": "/write",
            "body": "{\"operation\":\"ingest\",\"documents\":[{\"doc_id\":\"d1\",\"text\":\"hello\",\"metadata\":{},\"timestamp\":1700000000100}]}"
        });

        match decode_write_request(payload).expect("valid write event should decode") {
            WriteRequest::Ingest { documents } => assert_eq!(documents.len(), 1),
            other => panic!("expected Ingest, got {other:?}"),
        }
    }

    #[test]
    fn delete_path_wraps_doc_ids() {
        let payload = json!({
            "rawPath": "/delete",
            "body": "{\"doc_ids\":[\"d1\",\"d2\"]}"
        });

        match decode_write_request(payload).expect("valid delete event should decode") {
            WriteRequest::Delete { doc_ids } => assert_eq!(doc_ids, vec!["d1", "d2"]),
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn malformed_body_returns_400_envelope() {
        let payload = json!({
            "rawPath": "/write",
            "body": "{\"operation\":\"ingest\",\"documents\":\"wrong\"}"
        });

        let response = decode_write_request(payload).expect_err("must produce error envelope");
        assert_eq!(response.status_code, 400);
        assert!(response.body.contains("failed to deserialize write request"));
    }

    #[test]
    fn unknown_path_returns_404_envelope() {
        let payload = json!({"rawPath": "/nope", "body": "{}"});

        let response = decode_write_request(payload).expect_err("must produce error envelope");
        assert_eq!(response.status_code, 404);
        assert!(response.body.contains("not_found"));
    }
}
```

注意：`WriteRequest` 若未派生 `Debug`，把 `panic!("expected Ingest, got {other:?}")` 改为不打印值的形式（`panic!("expected Ingest variant")`，先 `let _ = other;`）。

- [ ] **Step 2: 跑 bin 单测**

Run: `cargo test --no-default-features --features lambda --bin write_lambda`
Expected: 4 个测试 PASS

- [ ] **Step 3: Commit**

```bash
git add src/bin/write_lambda.rs
git commit -m "feat(lambda): write_lambda accepts HTTP API v2 events with /write and /delete routes"
```

### Task 4: 抽取共享构建接线模块 `src/aws_wiring.rs`

**Files:**
- Create: `src/aws_wiring.rs`
- Modify: `src/lib.rs`（注册模块）
- Modify: `src/bin/index_builder_server.rs`（删除本地闭包定义，改为 import）

**Interfaces:**
- Consumes: 既有 `BuildConfig`、`http::build::{BuildFn, PublishFn}`、`build_worker::ListWalKeysFn`、`bootstrap::*`、`indexing::*`、`write::{WriteAheadLog, WAL_PREFIX}`
- Produces（Task 5 与 server bin 共用，签名逐字）:
  - `pub fn build_closure(config: BuildConfig, s3_client: aws_sdk_s3::Client) -> ltsearch::http::build::BuildFn`
  - `pub fn publish_closure(config: BuildConfig, s3_client: aws_sdk_s3::Client) -> ltsearch::http::build::PublishFn`
  - `pub fn list_wal_keys_closure(bucket: String, s3_client: aws_sdk_s3::Client) -> ListWalKeysFn`
  - `pub fn build_embedding_probe() -> impl Fn() -> Result<usize, String> + Send + Sync`

- [ ] **Step 1: 创建 `src/aws_wiring.rs`**

内容 = 把 `src/bin/index_builder_server.rs:63-188` 的 `build_closure` / `publish_closure` / `list_wal_keys_closure` / `build_embedding_probe` / `current_time_millis` 五个函数**原样平移**（fn 前加 `pub`，`current_time_millis` 保持私有），路径引用从 `ltsearch::xxx` 改为 `crate::xxx`。模块头注释：

```rust
//! index_builder 的 AWS 接线闭包：build（读全部 WAL 段 → embedding → 建索引）、
//! publish（CAS 发布）、WAL 段列举、embedding 健康 probe。原先内联在
//! index_builder_server bin 中，抽到 lib 供 server 与 lambda 两个 bin 复用
//! （#109：SQS 事件触发的 builder lambda 复用 process_queue_message 全链路）。
```

`src/lib.rs` 在 `pub mod app;` 后插入（cfg 门控与 adapters 的 aws 实现一致——`http::build` 依赖 server feature，`aws` feature 已隐含 server）：

```rust
#[cfg(feature = "aws")]
pub mod aws_wiring;
```

- [ ] **Step 2: `src/bin/index_builder_server.rs` 改用共享模块**

删除该文件中五个函数定义及其 import 依赖（保留 `main` 需要的），新增：

```rust
use ltsearch::aws_wiring::{
    build_closure, build_embedding_probe, list_wal_keys_closure, publish_closure,
};
```

- [ ] **Step 3: 验证 aws profile 编译 + 测试（纯平移重构，靠既有测试守护）**

Run: `cargo build --no-default-features --features aws && cargo test --no-default-features --features aws --lib --test runtime_aws_test`
Expected: 编译通过、测试 PASS

- [ ] **Step 4: Commit**

```bash
git add src/aws_wiring.rs src/lib.rs src/bin/index_builder_server.rs
git commit -m "refactor(builder): extract shared AWS build wiring closures into lib"
```

### Task 5: index_builder_lambda bin 接 SQS 批事件（复用 process_queue_message）

**Files:**
- Modify: `src/bin/index_builder_lambda.rs`（整文件替换）

**Interfaces:**
- Consumes: Task 1 的 `SqsEvent/process_sqs_records`；Task 4 的接线闭包；既有 `build_worker::process_queue_message`（消息体 = 写路径入队的 `QueueBatch` JSON，只消费 `batch_id`/`wal_key`；版本号从 `_head` 分配 + CAS 冲突重试一次；`embedding_dim` 读 env `LTSEARCH_BUILD_EMBEDDING_DIM`）
- Produces: 函数响应恒为 `{"batchItemFailures":[...]}`；**语义变更**：不再接受直调 `BuildRequest`（`version_id`/`embedding_dim` 不再由调用方传入），部署面必须配 `LTSEARCH_BUILD_EMBEDDING_DIM`

- [ ] **Step 1: 整文件替换 `src/bin/index_builder_lambda.rs`**

```rust
//! SQS 触发的 index builder lambda：每条记录即写路径入队的 `QueueBatch`，复用
//! build worker 的 `process_queue_message`（list 全部 WAL 段 → head 分配版本 →
//! run_build，CAS 冲突重试一次）。失败记录以 partial-batch failure 报告，由
//! Lambda event source mapping 决定重投/进 DLQ——本进程绝不手动删消息。

use std::sync::Arc;

use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::aws_wiring::{
    build_closure, build_embedding_probe, list_wal_keys_closure, publish_closure,
};
use ltsearch::bootstrap::{s3_client_from_env, BuildConfig};
use ltsearch::build_worker::{process_queue_message, ListWalKeysFn};
use ltsearch::http::build::BuildServerState;
use ltsearch::lambda_events::{process_sqs_records, SqsBatchResponse, SqsEvent, SqsRecord};
use serde_json::Value;

async fn function_handler(
    state: &BuildServerState,
    storage: &AwsPublishStorage,
    list_wal_keys: &ListWalKeysFn,
    event: LambdaEvent<Value>,
) -> Result<SqsBatchResponse, Error> {
    let (payload, _) = event.into_parts();
    // 信封本身解析失败 = 非 SQS 触发的异常调用：整批报错，交给重投策略。
    let sqs_event: SqsEvent = serde_json::from_value(payload)
        .map_err(|source| Error::from(format!("failed to deserialize SQS event: {source}")))?;

    let response = process_sqs_records(sqs_event, async |record: &SqsRecord| {
        process_queue_message(state, storage, list_wal_keys, &record.body)
            .await
            .map(|version_id| {
                eprintln!("index builder lambda: published index version {version_id}");
            })
    })
    .await;

    Ok(response)
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let config = BuildConfig::from_env()?;
        let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let s3_client = s3_client_from_env(&sdk_config);

        let state = BuildServerState {
            build: build_closure(config.clone(), s3_client.clone()),
            publish: publish_closure(config.clone(), s3_client.clone()),
            embedding_probe: Arc::new(build_embedding_probe()),
        };
        let storage = AwsPublishStorage::new(config.s3_bucket.clone(), s3_client.clone());
        let list_wal_keys = list_wal_keys_closure(config.s3_bucket.clone(), s3_client.clone());

        lambda_runtime::run(service_fn(move |event| {
            let state = state.clone();
            let storage = storage.clone();
            let list_wal_keys = list_wal_keys.clone();
            async move { function_handler(&state, &storage, &list_wal_keys, event).await }
        }))
        .await
    })
}
```

注意：
- `BuildConfig` 若未派生 `Clone`（当前 server bin 用了 `config.clone()`，应已派生），保持现状。
- `AwsPublishStorage` 需要 `Clone`；若未派生，在 `src/adapters/s3_publish.rs` 为其 `#[derive(Clone)]`（字段是 String + aws client，均可 Clone）。
- 旧文件中 `decode_request_payload`/`BuildRequest` 直调路径整体删除；`src/build_lambda.rs` 的 `BuildRequest` 类型仍被 `http/build.rs` 的 `POST /build` 使用，**保留不动**。

- [ ] **Step 2: 编译三个 lambda bin**

Run: `cargo build --no-default-features --features lambda --bin query_lambda --bin write_lambda --bin index_builder_lambda`
Expected: 编译通过

- [ ] **Step 3: 补集成测试（SQS 记录失败上报语义，无 AWS 依赖侧已在 Task 1 覆盖；此处验证 malformed 消息体走 batchItemFailures）**

在 `tests/index_builder_lambda_test.rs` 末尾追加（先读该文件确认现有 harness 与 feature 门控，测试放在与现有用例相同的 cfg 下；若该文件因 lambda bin 直调契约删除而引用了已删符号，同步清理）：

```rust
#[tokio::test]
async fn malformed_queue_body_is_reported_as_batch_item_failure() {
    use ltsearch::lambda_events::{process_sqs_records, SqsEvent, SqsRecord};

    let event: SqsEvent = serde_json::from_str(
        r#"{"Records": [{"messageId": "m-1", "body": "not json"}]}"#,
    )
    .unwrap();

    // 与 bin 的 function_handler 相同的组合方式：process_queue_message 解析失败
    // → Err → batchItemFailures 含该 messageId。这里用解析失败路径即可覆盖组合
    // 语义，不需要真实 S3。
    let response = process_sqs_records(event, async |record: &SqsRecord| {
        serde_json::from_str::<ltsearch::build_worker::QueueBuildMessage>(&record.body)
            .map(|_| ())
            .map_err(|error| format!("failed to parse queue message: {error}"))
    })
    .await;

    assert_eq!(response.batch_item_failures.len(), 1);
    assert_eq!(response.batch_item_failures[0].item_identifier, "m-1");
}
```

Run: `cargo test --no-default-features --features local --test index_builder_lambda_test`
Expected: PASS（`lambda_events`/`build_worker` 均无 AWS 门控；若该测试文件整体带 aws 门控则改跑 `--features aws`）

- [ ] **Step 4: Commit**

```bash
git add src/bin/index_builder_lambda.rs tests/index_builder_lambda_test.rs
git commit -m "feat(lambda): index_builder_lambda consumes SQS batches with partial-batch failures"
```

### Task 6: CI 增加 lambda bin 单测步骤

**Files:**
- Modify: `.github/workflows/ci.yml:52-53`（feature-matrix job）
- Modify: `tests/test_ci_workflow.py`（结构守卫同步）

- [ ] **Step 1: ci.yml 的 `lambda profile builds` 步骤后追加**

```yaml
      - name: lambda bin unit tests
        run: cargo test --no-default-features --features lambda --bins
```

- [ ] **Step 2: 更新 `tests/test_ci_workflow.py`**

先读该文件找到 feature-matrix 相关断言处，追加：

```python
        self.assertIn(
            "cargo test --no-default-features --features lambda --bins", content
        )
```

Run: `python3 -B tests/test_ci_workflow.py`
Expected: OK（全部通过）

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml tests/test_ci_workflow.py
git commit -m "ci: run lambda bin unit tests in feature matrix"
```

### Task 7: SAM e2e 切换事件信封（模板 + 两条脚本 + fixtures 守卫）

**Files:**
- Modify: `template.sam-e2e.yaml`（`Type: Api` → `Type: HttpApi`，payload v2）
- Modify: `scripts/e2e/lib.sh`（新增信封构造 + 响应断言 helper）
- Modify: `scripts/e2e/run-sam-local-invoke-e2e.sh`
- Modify: `scripts/e2e/run-http-flow.sh`
- Modify: `tests/test_sam_invoke_e2e.py`、`tests/test_sam_start_api_e2e.py`

**Interfaces:**
- Consumes: Task 2/3/5 的新事件契约
- Produces: e2e 断言新契约——write/query 响应为 `{statusCode, body}` 信封；builder 响应为 `{"batchItemFailures": []}`；builder 版本号改由 head 分配（fixed 跑出 1，ltembed 复用同 bucket 自动跑出 2）

- [ ] **Step 1: `template.sam-e2e.yaml` 事件类型切 HttpApi**

`WriteFunction` 与 `QueryFunction` 的 Events 块替换为（HTTP API 默认 payload 2.0，`sam local start-api` 会发 v2 信封）：

```yaml
      Events:
        WriteApi:
          Type: HttpApi
          Properties:
            Path: /write
            Method: post
```

```yaml
      Events:
        QueryApi:
          Type: HttpApi
          Properties:
            Path: /query
            Method: post
```

`BuildFunction` 不加事件（e2e 用 `sam local invoke` 手工喂 SQS 信封），但其 Environment 保持含 `LTSEARCH_BUILD_EMBEDDING_DIM: 3`（builder 现在必需此 env）。

- [ ] **Step 2: `scripts/e2e/lib.sh` 追加三个 helper**

```bash
# 把裸请求体包成 API Gateway HTTP API payload v2 信封事件文件。
# 用法: make_apigw_event <body-json-file> <raw-path> <out-file>
make_apigw_event() {
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys
body_path, raw_path, out_path = sys.argv[1:4]
event = {
    'version': '2.0',
    'routeKey': f'POST {raw_path}',
    'rawPath': raw_path,
    'requestContext': {'http': {'method': 'POST', 'path': raw_path}},
    'isBase64Encoded': False,
    'body': open(body_path).read(),
}
json.dump(event, open(out_path, 'w'))
PY
}

# 把 `aws sqs receive-message` 的响应包成 Lambda SQS 触发事件文件。
# 用法: make_sqs_event <receive-message-response-file> <out-file>
make_sqs_event() {
  python3 - "$1" "$2" <<'PY'
import json, sys
response = json.load(open(sys.argv[1]))
messages = response.get('Messages', [])
if not messages:
    raise SystemExit('expected one SQS batch message')
event = {'Records': [{
    'messageId': messages[0].get('MessageId', 'e2e-message-1'),
    'body': messages[0]['Body'],
    'eventSource': 'aws:sqs',
}]}
json.dump(event, open(sys.argv[2], 'w'))
PY
}

# 断言 APIGW v2 信封响应: statusCode==200 且 body 内字段等于期望值。
# 用法: assert_lambda_json_field <response-file> <field> <expected>
assert_lambda_json_field() {
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys
path, field, expected = sys.argv[1:4]
response = json.load(open(path))
assert response.get('statusCode') == 200, f'non-200 lambda response: {response}'
body = json.loads(response['body'])
actual = str(body.get(field))
assert actual == expected, f'{field}: expected {expected}, got {actual} in {body}'
PY
}
```

- [ ] **Step 3: 改 `scripts/e2e/run-sam-local-invoke-e2e.sh`**

fixed 主流程改动（ltembed 分支对称照改）：

1. write 调用前生成信封，`--event` 改用信封文件：

```bash
WRITE_EVENT_JSON="$E2E_OUTPUT_DIR/write-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/write_request.json" /write "$WRITE_EVENT_JSON"
```

2. 现有 `build-event.json` 的 python 生成块整体替换为：

```bash
make_sqs_event "$BATCH_RESPONSE_JSON" "$E2E_OUTPUT_DIR/build-event.json"
```

3. query 同 write：`make_apigw_event "$E2E_FIXTURES_DIR/query_request.json" /query "$QUERY_EVENT_JSON"`。
4. 断言改为：

```bash
assert_lambda_json_field "$WRITE_RESPONSE_JSON" accepted_count 6

python3 - <<'PY' "$BUILD_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response == {'batchItemFailures': []}, response
PY

python3 - <<'PY' "$QUERY_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response['statusCode'] == 200, response
body = json.loads(response['body'])
assert body['index_version'] == 1, body
assert body['dynamic_count'] >= 1, body
doc_ids = [item['doc_id'] for item in body['dynamic_chunks']]
assert 'doc-rust-hybrid' in doc_ids, body
PY
```

5. env-vars 两个生成块的 `BuildFunction` 条目补 `'LTSEARCH_BUILD_EMBEDDING_DIM': '3'`（fixed）/`'512'`（ltembed）。
6. ltembed 分支删除手工 `version_id: 2 / embedding_dim: 512` 的 build-event python 块，改 `make_sqs_event`；版本断言 `index_version == 2` 保持（head 自动从 1 推进到 2）。

- [ ] **Step 4: 改 `scripts/e2e/run-http-flow.sh`**

先读全文。write/query 走 `sam local start-api` 的 curl 不动（HttpApi 会把 v2 信封拆包，curl 拿到的仍是内层 body，`assert_json_field ... accepted_count 6` 等断言原样成立）。改动仅 builder 段：`receive_one_sqs_batch` 后的 build-event 生成块换成 `make_sqs_event`，`sam local invoke BuildFunction` 的响应断言从 `assert_json_field ... activated_version_id 1` 换成上面 Step 3 第 4 点的 `batchItemFailures == []` python 块，并在其后紧跟的 query 断言里校验 `index_version`（已有）。

- [ ] **Step 5: 更新两份结构守卫**

`tests/test_sam_invoke_e2e.py`：
- 模板断言：`self.assertIn("Type: HttpApi", content)`；删除对 `Type: Api` 的断言（如有）；保留 `PackageType: Image` 等原有断言。
- 脚本断言：新增 `self.assertIn("make_apigw_event", content)`、`self.assertIn("make_sqs_event", content)`、`self.assertIn("batchItemFailures", content)`、`self.assertIn("assert_lambda_json_field", content)`；删除已不存在的字符串断言（跑一遍测试按失败清单清理，如 `'version_id': 1` 生成块相关）。

`tests/test_sam_start_api_e2e.py`：
- `test_http_flow_script_runs_write_build_query` 中 `self.assertIn("activated_version_id", content)` 替换为 `self.assertIn("batchItemFailures", content)` 与 `self.assertIn("make_sqs_event", content)`。

Run: `python3 -B tests/test_sam_invoke_e2e.py && python3 -B tests/test_sam_start_api_e2e.py && python3 -B tests/test_e2e_fixtures.py`
Expected: 全部 OK（裸 fixtures 未动，test_e2e_fixtures.py 不受影响）

- [ ] **Step 6: 本地全量回归（含 moto docker，若本机可用）**

Run: `bash scripts/verify-fast.sh`
Expected: PASS。SAM e2e 留给 CI（arm64 + docker 环境）。

- [ ] **Step 7: Commit + 发 PR-1**

```bash
git add template.sam-e2e.yaml scripts/e2e/lib.sh scripts/e2e/run-sam-local-invoke-e2e.sh scripts/e2e/run-http-flow.sh tests/test_sam_invoke_e2e.py tests/test_sam_start_api_e2e.py
git commit -m "test(e2e): drive SAM flows through HTTP API v2 and SQS event envelopes"
gh pr create --repo Lychee-Technology/LTSearch --title "feat(lambda): HTTP API v2 + SQS event envelopes for lambda runtimes (#109 PR-1)" --body "Part of #109（PR 1/2：运行时事件适配）..."
```

PR 正文列出契约变更：lambda 直调裸 JSON 不再支持；builder 版本号改由 head 分配、`LTSEARCH_BUILD_EMBEDDING_DIM` 成为必需 env。等待 review 合并后再启动 PR-2。

---

# PR-2：ZIP 打包 + 生产 SAM 模板 + ZIP e2e

### Task 8: `scripts/package-lambda-zips.sh` 打包脚本

**Files:**
- Create: `scripts/package-lambda-zips.sh`（`chmod +x`）

**Interfaces:**
- Consumes: `sam/builder.Dockerfile`（已在 AL2023 内 `cargo build --release --no-default-features --features lambda[,ltembed]`，产物在镜像根 `/query_lambda` `/write_lambda` `/index_builder_lambda`；`LTEMBED_MODE=stub|real` 控制 feature 组合）
- Produces: `dist/<fn>/bootstrap`（可执行，供 `sam local invoke`/`sam deploy` 的 CodeUri 目录）与 `dist/<fn>.zip`（根含 `bootstrap`，交付产物）；`fn ∈ {query_lambda, write_lambda, index_builder_lambda}`

- [ ] **Step 1: 创建脚本**

```bash
#!/usr/bin/env bash
# 打包 3 个 Lambda ZIP（#109）：在 AL2023 builder 镜像内编译（glibc 2.34 兼容
# provided.al2023 运行时；ubuntu 宿主机原生编译会链接更新的 glibc 符号），提取
# 二进制改名 bootstrap 置于 zip 根。不引入 cargo-lambda。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DIST_DIR="${LTSEARCH_DIST_DIR:-$REPO_ROOT/dist}"
readonly BUILDER_IMAGE="${LTSEARCH_BUILDER_IMAGE:-ltsearch-lambda-zip-builder}"
# stub = features lambda（fixed embedding，e2e/CI 用）；real = features lambda,ltembed。
readonly LTEMBED_MODE="${LTSEARCH_LTEMBED_MODE:-stub}"

docker build \
  --platform linux/arm64 \
  --build-arg LTEMBED_MODE="$LTEMBED_MODE" \
  --tag "$BUILDER_IMAGE" \
  --file "$REPO_ROOT/sam/builder.Dockerfile" \
  "$REPO_ROOT"

container_id="$(docker create --platform linux/arm64 "$BUILDER_IMAGE")"
trap 'docker rm -f "$container_id" >/dev/null' EXIT

mkdir -p "$DIST_DIR"
for fn in query_lambda write_lambda index_builder_lambda; do
  fn_dir="$DIST_DIR/$fn"
  rm -rf "$fn_dir" "$DIST_DIR/$fn.zip"
  mkdir -p "$fn_dir"
  docker cp "$container_id:/$fn" "$fn_dir/bootstrap"
  chmod +x "$fn_dir/bootstrap"
  (cd "$fn_dir" && zip -q -X "$DIST_DIR/$fn.zip" bootstrap)
done

echo "packaged lambda zips into $DIST_DIR" >&2
```

Run: `chmod +x scripts/package-lambda-zips.sh && bash -n scripts/package-lambda-zips.sh`
Expected: 语法检查通过（真实打包留给 e2e/CI；本机如有 docker 可直接跑一次验证 `dist/` 产物）

- [ ] **Step 2: Commit**

```bash
git add scripts/package-lambda-zips.sh
git commit -m "build: add lambda zip packaging script (AL2023 builder image, bootstrap at zip root)"
```

### Task 9: 生产 `template.yaml`

**Files:**
- Create: `template.yaml`

**Interfaces:**
- Consumes: Task 8 的 `dist/<fn>/` CodeUri 目录；PR-1 的事件契约（HTTP API v2 / SQS + `ReportBatchItemFailures`）
- Produces: 可 `sam deploy` 的生产模板；`sam local invoke` 直接可用（配 `--env-vars` 覆盖 endpoint）

- [ ] **Step 1: 创建 `template.yaml`**

```yaml
AWSTemplateFormatVersion: '2010-09-09'
Transform: AWS::Serverless-2016-10-31
Description: >-
  LTSearch production deployment: Lambda ZIP (provided.al2023 / arm64),
  HTTP API front for write/query, SQS-triggered index builder with
  partial-batch failure redrive. (#109)

Parameters:
  EmbeddingProvider:
    Type: String
    Default: ltembed
    Description: >-
      LTSEARCH_*_EMBEDDING_PROVIDER for build/query. ltembed 模型资产
      （LTSEARCH_*_LTEMBED_BUNDLE_DIR 等 env 与 Lambda Layer）由 #111 交付；
      在那之前生产部署需自带模型资产或改用 fixed 做冒烟。
  EmbeddingDim:
    Type: String
    Default: '512'
    Description: LTSEARCH_BUILD_EMBEDDING_DIM（jina/512 为 #94 裁决的默认档）。

Globals:
  Function:
    Runtime: provided.al2023
    Handler: bootstrap
    Architectures:
      - arm64
    Timeout: 30
    MemorySize: 1024

Resources:
  ArtifactBucket:
    Type: AWS::S3::Bucket

  BuildDeadLetterQueue:
    Type: AWS::SQS::Queue
    Properties:
      MessageRetentionPeriod: 1209600

  BuildQueue:
    Type: AWS::SQS::Queue
    Properties:
      # timeout-safe：≥ 6 × BuildFunction Timeout(900s)，避免在途消息被重复投递。
      VisibilityTimeout: 5400
      RedrivePolicy:
        deadLetterTargetArn: !GetAtt BuildDeadLetterQueue.Arn
        maxReceiveCount: 3

  WriteFunction:
    Type: AWS::Serverless::Function
    Properties:
      CodeUri: dist/write_lambda/
      Events:
        WriteApi:
          Type: HttpApi
          Properties:
            Path: /write
            Method: post
        DeleteApi:
          Type: HttpApi
          Properties:
            Path: /delete
            Method: post
      Environment:
        Variables:
          LTSEARCH_WRITE_S3_BUCKET: !Ref ArtifactBucket
          LTSEARCH_WRITE_SQS_QUEUE_URL: !Ref BuildQueue
      Policies:
        - S3CrudPolicy:
            BucketName: !Ref ArtifactBucket
        - SQSSendMessagePolicy:
            QueueName: !GetAtt BuildQueue.QueueName

  QueryFunction:
    Type: AWS::Serverless::Function
    Properties:
      CodeUri: dist/query_lambda/
      MemorySize: 3008
      Events:
        QueryApi:
          Type: HttpApi
          Properties:
            Path: /query
            Method: post
      Environment:
        Variables:
          LTSEARCH_QUERY_S3_BUCKET: !Ref ArtifactBucket
          LTSEARCH_QUERY_ARTIFACT_ROOT: /tmp/ltsearch-artifacts
          LTSEARCH_QUERY_EMBEDDING_PROVIDER: !Ref EmbeddingProvider
      Policies:
        - S3ReadPolicy:
            BucketName: !Ref ArtifactBucket

  BuildFunction:
    Type: AWS::Serverless::Function
    Properties:
      CodeUri: dist/index_builder_lambda/
      Timeout: 900
      MemorySize: 3008
      Events:
        BuildQueueEvent:
          Type: SQS
          Properties:
            Queue: !GetAtt BuildQueue.Arn
            BatchSize: 1
            FunctionResponseTypes:
              - ReportBatchItemFailures
      Environment:
        Variables:
          LTSEARCH_BUILD_S3_BUCKET: !Ref ArtifactBucket
          LTSEARCH_BUILD_ARTIFACT_ROOT: /tmp/ltsearch-artifacts
          LTSEARCH_BUILD_EMBEDDING_PROVIDER: !Ref EmbeddingProvider
          LTSEARCH_BUILD_EMBEDDING_DIM: !Ref EmbeddingDim
      Policies:
        - S3CrudPolicy:
            BucketName: !Ref ArtifactBucket
        - SQSPollerPolicy:
            QueueName: !GetAtt BuildQueue.QueueName

Outputs:
  ApiUrl:
    Description: HTTP API base URL (POST /write, /delete, /query)
    Value: !Sub 'https://${ServerlessHttpApi}.execute-api.${AWS::Region}.amazonaws.com'
  ArtifactBucketName:
    Value: !Ref ArtifactBucket
  BuildQueueUrl:
    Value: !Ref BuildQueue
```

Run: `sam validate --template-file template.yaml --lint`（本机装了 sam 时）
Expected: 校验通过；未装 sam 则由 Task 11 的 CI e2e 兜底

- [ ] **Step 2: Commit**

```bash
git add template.yaml
git commit -m "feat(sam): production template with zip packages, HTTP API and SQS redrive (#109)"
```

### Task 10: 结构守卫 `tests/test_lambda_zip_packaging.py`

**Files:**
- Create: `tests/test_lambda_zip_packaging.py`

- [ ] **Step 1: 创建守卫测试（模式仿 `tests/test_sam_invoke_e2e.py`）**

```python
import stat
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
PACKAGE_SCRIPT_PATH = REPO_ROOT / "scripts" / "package-lambda-zips.sh"
TEMPLATE_PATH = REPO_ROOT / "template.yaml"
ZIP_E2E_SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "run-sam-zip-invoke-e2e.sh"
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci.yml"


class LambdaZipPackagingTest(unittest.TestCase):
    def test_package_script_builds_in_al2023_and_stages_bootstrap(self) -> None:
        self.assertTrue(PACKAGE_SCRIPT_PATH.exists())
        content = PACKAGE_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("sam/builder.Dockerfile", content)
        self.assertIn("--platform linux/arm64", content)
        self.assertIn("bootstrap", content)
        self.assertIn("chmod +x", content)
        self.assertIn("zip -q -X", content)
        for fn in ("query_lambda", "write_lambda", "index_builder_lambda"):
            self.assertIn(fn, content)
        # 打包绝不在宿主机原生 cargo build（glibc 兼容性），也不引 cargo-lambda。
        self.assertNotIn("cargo-lambda", content)
        mode = PACKAGE_SCRIPT_PATH.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR, "package script must be executable")

    def test_production_template_uses_zip_httpapi_and_sqs_redrive(self) -> None:
        self.assertTrue(TEMPLATE_PATH.exists())
        content = TEMPLATE_PATH.read_text(encoding="utf-8")
        self.assertIn("Transform: AWS::Serverless-2016-10-31", content)
        self.assertIn("Runtime: provided.al2023", content)
        self.assertIn("Handler: bootstrap", content)
        self.assertIn("arm64", content)
        self.assertNotIn("PackageType: Image", content)
        for code_uri in (
            "dist/write_lambda/",
            "dist/query_lambda/",
            "dist/index_builder_lambda/",
        ):
            self.assertIn(f"CodeUri: {code_uri}", content)
        self.assertIn("Type: HttpApi", content)
        self.assertIn("Type: SQS", content)
        self.assertIn("ReportBatchItemFailures", content)
        self.assertIn("RedrivePolicy", content)
        self.assertIn("deadLetterTargetArn", content)
        self.assertIn("VisibilityTimeout: 5400", content)
        self.assertIn("Timeout: 900", content)
        self.assertIn("LTSEARCH_BUILD_EMBEDDING_DIM", content)

    def test_zip_e2e_script_covers_package_invoke_and_layout(self) -> None:
        self.assertTrue(ZIP_E2E_SCRIPT_PATH.exists())
        content = ZIP_E2E_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("package-lambda-zips.sh", content)
        self.assertIn("assert_zip_layout", content)
        self.assertIn("make_apigw_event", content)
        self.assertIn("make_sqs_event", content)
        self.assertIn("batchItemFailures", content)
        self.assertIn('--template-file "$REPO_ROOT/template.yaml"', content)

    def test_ci_has_zip_e2e_job(self) -> None:
        content = WORKFLOW_PATH.read_text(encoding="utf-8")
        self.assertIn("sam-zip-e2e:", content)
        self.assertIn("run-sam-zip-invoke-e2e.sh", content)


if __name__ == "__main__":
    unittest.main()
```

Run: `python3 -B tests/test_lambda_zip_packaging.py`
Expected: 前两个用例 PASS，后两个 FAIL（e2e 脚本与 CI job 尚不存在）——Task 11 转绿

- [ ] **Step 2: Commit（红灯提交与 Task 11 合并亦可；仓库惯例允许则分开）**

```bash
git add tests/test_lambda_zip_packaging.py
git commit -m "test: structural guards for lambda zip packaging and production template"
```

### Task 11: ZIP 路径 e2e 脚本 + CI job

**Files:**
- Create: `scripts/e2e/run-sam-zip-invoke-e2e.sh`（`chmod +x`）
- Modify: `scripts/e2e/lib.sh`（新增 `assert_zip_layout` helper）
- Modify: `.github/workflows/ci.yml`（新增 `sam-zip-e2e` job）
- Modify: `tests/test_ci_workflow.py`（若其枚举 job 列表则同步）

**Interfaces:**
- Consumes: Task 8 脚本、Task 9 模板、PR-1 的 lib.sh 信封 helper 与 moto compose
- Produces: AC-1（zip 布局断言）与 AC-5（ZIP SAM 路径 e2e）的自动化验证

- [ ] **Step 1: `scripts/e2e/lib.sh` 追加 zip 布局断言**

```bash
# 断言 zip 根含可执行 bootstrap（provided.al2023 自定义运行时布局）。
# 用法: assert_zip_layout <zip-file>
assert_zip_layout() {
  python3 - "$1" <<'PY'
import stat, sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as archive:
    names = archive.namelist()
    assert names == ['bootstrap'], f'zip root must contain only bootstrap, got {names}'
    info = archive.getinfo('bootstrap')
    mode = info.external_attr >> 16
    assert mode & stat.S_IXUSR, f'bootstrap must be executable, mode={oct(mode)}'
PY
}
```

- [ ] **Step 2: 创建 `scripts/e2e/run-sam-zip-invoke-e2e.sh`**

结构照抄 `run-sam-local-invoke-e2e.sh` 的 fixed 主流程，差异点：

```bash
#!/usr/bin/env bash
# ZIP 路径 SAM e2e（#109 AC-1/AC-5）：package-lambda-zips.sh（stub 模式）产出
# dist/，断言 zip 布局，再用生产 template.yaml 直接 sam local invoke（CodeUri
# 指向 dist/<fn>/ 目录，无需 sam build），走 write→SQS→build→query 全链路。
set -euo pipefail

source "$(dirname "$0")/lib.sh"

readonly REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
readonly E2E_FIXTURES_DIR="$REPO_ROOT/tests/fixtures/e2e"
readonly E2E_OUTPUT_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$REPO_ROOT/.e2e-tmp}"
readonly E2E_RUN_ID="${LTSEARCH_E2E_RUN_ID:-$(date +%s)-$$}"
readonly E2E_BUCKET="${LTSEARCH_E2E_BUCKET:-ltsearch-zip-e2e-$E2E_RUN_ID}"
readonly E2E_QUEUE_NAME="${LTSEARCH_E2E_QUEUE_NAME:-ltsearch-zip-e2e-$E2E_RUN_ID}"

mkdir -p "$E2E_OUTPUT_DIR"

wait_for_moto
create_e2e_bucket "$E2E_BUCKET"
QUEUE_URL="$(create_e2e_queue "$E2E_QUEUE_NAME")"

LTSEARCH_LTEMBED_MODE=stub bash "$REPO_ROOT/scripts/package-lambda-zips.sh"

for fn in query_lambda write_lambda index_builder_lambda; do
  assert_zip_layout "$REPO_ROOT/dist/$fn.zip"
done

ENV_VARS_JSON="$E2E_OUTPUT_DIR/zip-env-vars.json"
python3 - <<'PY' "$ENV_VARS_JSON" "$E2E_BUCKET" "$QUEUE_URL"
import json, sys
env_path, bucket, queue_url = sys.argv[1:4]
moto_endpoint = 'http://moto:5000'
container_queue_url = queue_url.replace('http://localhost:5000', moto_endpoint)
common_aws = {
    'AWS_ACCESS_KEY_ID': 'test',
    'AWS_SECRET_ACCESS_KEY': 'test',
    'AWS_DEFAULT_REGION': 'us-east-1',
    'AWS_REGION': 'us-east-1',
    'AWS_ENDPOINT_URL_S3': moto_endpoint,
}
env = {
    'WriteFunction': {
        **common_aws,
        'AWS_ENDPOINT_URL_SQS': moto_endpoint,
        'LTSEARCH_WRITE_S3_BUCKET': bucket,
        'LTSEARCH_WRITE_SQS_QUEUE_URL': container_queue_url,
    },
    'BuildFunction': {
        **common_aws,
        'LTSEARCH_BUILD_S3_BUCKET': bucket,
        'LTSEARCH_BUILD_ARTIFACT_ROOT': '/tmp/ltsearch-zip-e2e-artifacts',
        'LTSEARCH_BUILD_EMBEDDING_PROVIDER': 'fixed',
        'LTSEARCH_BUILD_FIXED_EMBEDDING': '0.9,0.1,0.0',
        'LTSEARCH_BUILD_EMBEDDING_DIM': '3',
    },
    'QueryFunction': {
        **common_aws,
        'LTSEARCH_QUERY_S3_BUCKET': bucket,
        'LTSEARCH_QUERY_ARTIFACT_ROOT': '/tmp/ltsearch-zip-e2e-artifacts',
        'LTSEARCH_QUERY_EMBEDDING_PROVIDER': 'fixed',
        'LTSEARCH_QUERY_FIXED_EMBEDDING': '0.9,0.1,0.0',
    },
}
json.dump(env, open(env_path, 'w'))
PY

WRITE_EVENT_JSON="$E2E_OUTPUT_DIR/zip-write-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/write_request.json" /write "$WRITE_EVENT_JSON"
WRITE_RESPONSE_JSON="$E2E_OUTPUT_DIR/zip-write-response.json"
sam local invoke WriteFunction \
  --template-file "$REPO_ROOT/template.yaml" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$WRITE_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$WRITE_RESPONSE_JSON"
assert_lambda_json_field "$WRITE_RESPONSE_JSON" accepted_count 6

BATCH_RESPONSE_JSON="$E2E_OUTPUT_DIR/zip-batch-response.json"
receive_one_sqs_batch "$QUEUE_URL" > "$BATCH_RESPONSE_JSON"
BUILD_EVENT_JSON="$E2E_OUTPUT_DIR/zip-build-event.json"
make_sqs_event "$BATCH_RESPONSE_JSON" "$BUILD_EVENT_JSON"

BUILD_RESPONSE_JSON="$E2E_OUTPUT_DIR/zip-build-response.json"
sam local invoke BuildFunction \
  --template-file "$REPO_ROOT/template.yaml" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$BUILD_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$BUILD_RESPONSE_JSON"
python3 - <<'PY' "$BUILD_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response == {'batchItemFailures': []}, response
PY

QUERY_EVENT_JSON="$E2E_OUTPUT_DIR/zip-query-event.json"
make_apigw_event "$E2E_FIXTURES_DIR/query_request.json" /query "$QUERY_EVENT_JSON"
QUERY_RESPONSE_JSON="$E2E_OUTPUT_DIR/zip-query-response.json"
sam local invoke QueryFunction \
  --template-file "$REPO_ROOT/template.yaml" \
  --env-vars "$ENV_VARS_JSON" \
  --event "$QUERY_EVENT_JSON" \
  --docker-network ltsearch-e2e \
  > "$QUERY_RESPONSE_JSON"
python3 - <<'PY' "$QUERY_RESPONSE_JSON"
import json, sys
response = json.load(open(sys.argv[1]))
assert response['statusCode'] == 200, response
body = json.loads(response['body'])
assert body['index_version'] == 1, body
doc_ids = [item['doc_id'] for item in body['dynamic_chunks']]
assert 'doc-rust-hybrid' in doc_ids, body
PY

echo "ZIP SAM e2e passed" >&2
```

注意：`sam local invoke` 对 `!Ref` 环境变量解析为占位串，全部关键 env 已由 `--env-vars` 覆盖。若 `receive_one_sqs_batch` / `create_e2e_*` helper 签名与假设不符，以 `scripts/e2e/lib.sh` 现状为准。

- [ ] **Step 3: ci.yml 新增 job（放在 `sam-e2e` 之后，同级依赖 integration）**

```yaml
  sam-zip-e2e:
    needs: integration
    runs-on: ubuntu-24.04-arm
    timeout-minutes: 120
    steps:
      - uses: actions/checkout@v6
      - uses: actions/setup-python@v6
        with:
          python-version: '3.x'
      - run: python3 -B tests/test_lambda_zip_packaging.py
      - run: python3 -m pip install --upgrade pip awscli aws-sam-cli
      - run: docker compose -f docker-compose.moto.yml up -d
      - run: bash scripts/e2e/run-sam-zip-invoke-e2e.sh
      - if: always()
        run: docker compose -f docker-compose.moto.yml down -v
```

- [ ] **Step 4: 全部守卫转绿**

Run: `chmod +x scripts/e2e/run-sam-zip-invoke-e2e.sh && python3 -B tests/test_lambda_zip_packaging.py && python3 -B tests/test_ci_workflow.py && python3 -B tests/test_sam_invoke_e2e.py`
Expected: 全 OK（`test_ci_workflow.py` 若断言 job 全集需补 `sam-zip-e2e`）

- [ ] **Step 5: 本机冒烟（可选，需 docker + sam）**

Run: `docker compose -f docker-compose.moto.yml up -d && bash scripts/e2e/run-sam-zip-invoke-e2e.sh; docker compose -f docker-compose.moto.yml down -v`
Expected: `ZIP SAM e2e passed`

- [ ] **Step 6: Commit + 发 PR-2**

```bash
git add scripts/e2e/run-sam-zip-invoke-e2e.sh scripts/e2e/lib.sh .github/workflows/ci.yml tests/test_ci_workflow.py
git commit -m "feat(sam): zip-path e2e over production template + sam-zip-e2e CI job"
gh pr create --repo Lychee-Technology/LTSearch --title "feat(sam): lambda zip packaging + production template (#109 PR-2)" --body "Closes #109（PR 2/2：打包 + SAM）..."
```

---

## 验收对照（issue #109 AC）

| AC | 覆盖 |
|---|---|
| Linux arm64 ZIP、根含可执行 `bootstrap`、兼容 `provided.al2023` | Task 8（AL2023 镜像内编译）+ Task 11 `assert_zip_layout` + zip e2e 实跑 |
| query/write 接 HTTP API proxy 事件、返回合法 HTTP 响应 | Task 1-3 单测 + Task 7/11 e2e（curl 经 start-api 与 invoke 信封双路） |
| builder 处理 SQS 批、partial-batch failure、绝不手动 ack | Task 1 `process_sqs_records` 单测 + Task 5（bin 不再构造 SQS client，无 delete_message 路径）+ e2e `batchItemFailures` 断言 |
| SAM 模板：ZIP 包、HTTP API 事件、SQS 事件源带 redrive 且 timeout-safe | Task 9（VisibilityTimeout 5400 = 6×900、DLQ maxReceiveCount 3）+ Task 10 守卫 |
| 自动化测试覆盖 ZIP 布局、API proxy 事件、真实 SQS 信封、ZIP SAM 路径 | Task 1/2/3/5 单测（信封 fixture 取自 AWS 文档）、Task 7（moto 真实消息体入信封）、Task 10/11 |

## 明确不做（留给后续 issue）

- LTEmbed Lambda Layer、模型资产进 ZIP/Layer 的尺寸预算 → #111（模板 `EmbeddingProvider` 参数已留缝）。
- `docs/deployment.md` / `docs/arch.md` §22 叙事校准、GitHub Release 产物自动化、`publish-images.yml` 收敛 → #113。
- static release 指针/双版本查询 → #112。

## 风险与回退

- **glibc 兼容**：已规避（AL2023 容器内编译）。若 CI 中 `sam local invoke` 启动 `provided.al2023` 容器报 GLIBC 符号错误，检查 builder 镜像基底是否仍为 `amazonlinux:2023`。
- **`sam local start-api` 对 HttpApi 的 payload v2 支持**：sam cli 长期支持；若 CI 现 v1 信封（`httpMethod` 字段），在 `ApiGatewayV2Request` 上兼容解析（补 `#[serde(alias = "path")]`）并回报。
- **builder 直调契约移除**：`run-http-flow.sh`/`run-sam-local-invoke-e2e.sh` 已同步；若还有隐藏调用方（grep `sam local invoke BuildFunction` 与 `BuildRequest` 全仓确认），在 PR-1 内一并迁移。
