use std::env;

use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;
use ltsearch::write::api::WriteApi;
use ltsearch::write::wal::WriteAheadLog;
use ltsearch::write_lambda::{WriteLambdaError, WriteRequest, WriteResponse};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WriteLambdaPayload {
    Success(WriteResponse),
    Error(WriteLambdaError),
}

fn decode_request_payload(payload: Value) -> Result<WriteRequest, WriteLambdaPayload> {
    serde_json::from_value(payload).map_err(|source| {
        WriteLambdaPayload::Error(WriteLambdaError {
            error_type: "validation_error".into(),
            message: format!("failed to deserialize write request: {source}"),
        })
    })
}

async fn function_handler(event: LambdaEvent<Value>) -> Result<WriteLambdaPayload, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_request_payload(payload) {
        Ok(request) => request,
        Err(payload) => return Ok(payload),
    };

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

    let s3_bucket = env::var("LTSEARCH_WRITE_S3_BUCKET").unwrap_or_default();
    let sqs_queue_url = env::var("LTSEARCH_WRITE_SQS_QUEUE_URL").unwrap_or_default();

    let s3_client = s3_client_from_env(&config);
    let sqs_client = sqs_client_from_env(&config);

    let wal_storage = AwsS3WalStorage::new(s3_bucket, s3_client);
    let build_queue = AwsSqsBuildQueue::new(sqs_queue_url, sqs_client);
    let wal = WriteAheadLog::new(wal_storage);
    let write_api = WriteApi::new(wal, build_queue);

    let result = match request {
        WriteRequest::Ingest { documents } => {
            write_api.ingest(documents).await.map(|r| WriteResponse {
                accepted_count: r.accepted_count,
                wal_event_ids: r.wal_event_ids,
                batch_id: r.batch_id,
            })
        }
        WriteRequest::Delete { doc_ids } => {
            write_api.delete(doc_ids).await.map(|r| WriteResponse {
                accepted_count: r.accepted_count,
                wal_event_ids: r.wal_event_ids,
                batch_id: r.batch_id,
            })
        }
    };

    let payload = match result {
        Ok(response) => WriteLambdaPayload::Success(response),
        Err(error) => WriteLambdaPayload::Error(WriteLambdaError::from(error)),
    };

    Ok(payload)
}

fn s3_client_from_env(config: &aws_config::SdkConfig) -> aws_sdk_s3::Client {
    match env::var("AWS_ENDPOINT_URL_S3") {
        Ok(endpoint_url) => {
            let s3_config = aws_sdk_s3::config::Builder::from(config)
                .endpoint_url(endpoint_url)
                .force_path_style(true)
                .build();
            aws_sdk_s3::Client::from_conf(s3_config)
        }
        Err(_) => aws_sdk_s3::Client::new(config),
    }
}

fn sqs_client_from_env(config: &aws_config::SdkConfig) -> aws_sdk_sqs::Client {
    match env::var("AWS_ENDPOINT_URL_SQS") {
        Ok(endpoint_url) => {
            let sqs_config = aws_sdk_sqs::config::Builder::from(config)
                .endpoint_url(endpoint_url)
                .build();
            aws_sdk_sqs::Client::from_conf(sqs_config)
        }
        Err(_) => aws_sdk_sqs::Client::new(config),
    }
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?
        .block_on(async { lambda_runtime::run(service_fn(function_handler)).await })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static WRITE_LAMBDA_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn malformed_event_payload_returns_typed_error_envelope() {
        let payload =
            decode_request_payload(serde_json::json!({"operation":"ingest","documents":"wrong"}));

        match payload {
            Ok(_) => panic!("expected malformed payload to return an error envelope"),
            Err(payload) => match payload {
                WriteLambdaPayload::Success(_) => {
                    panic!("expected malformed payload to produce an error envelope")
                }
                WriteLambdaPayload::Error(error) => {
                    assert_eq!(error.error_type, "validation_error");
                    assert!(error
                        .message
                        .contains("failed to deserialize write request"));
                }
            },
        }
    }

    #[test]
    fn endpoint_overrides_are_applied_without_panicking() {
        let _guard = WRITE_LAMBDA_ENV_LOCK.lock().unwrap();
        std::env::set_var("AWS_ENDPOINT_URL_S3", "http://localhost:5000");
        std::env::set_var("AWS_ENDPOINT_URL_SQS", "http://localhost:5000");

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .load()
                .await;
            let _ = s3_client_from_env(&config);
            let _ = sqs_client_from_env(&config);
        });
    }
}
