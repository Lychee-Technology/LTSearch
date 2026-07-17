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
            headers: BTreeMap::from([("content-type".to_string(), "application/json".to_string())]),
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
        assert_eq!(
            ApiGatewayV2Response::error("validation_error", "x").status_code,
            400
        );
        assert_eq!(
            ApiGatewayV2Response::error("not_found", "x").status_code,
            404
        );
        assert_eq!(
            ApiGatewayV2Response::error("execution_error", "x").status_code,
            500
        );
        assert_eq!(
            ApiGatewayV2Response::error("publish_error", "x").status_code,
            500
        );
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
