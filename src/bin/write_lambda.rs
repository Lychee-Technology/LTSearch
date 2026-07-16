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
