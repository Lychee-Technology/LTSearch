use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;
use ltsearch::bootstrap::{s3_client_from_env, sqs_client_from_env, WriteConfig};
use ltsearch::write::api::WriteApi;
use ltsearch::write::wal::WriteAheadLog;
use ltsearch::write_lambda::{handle_write_request, WriteLambdaError, WriteRequest, WriteResponse};
use serde::Serialize;
use serde_json::Value;

type ProdWriteApi = WriteApi<AwsS3WalStorage, AwsSqsBuildQueue>;

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

async fn function_handler(
    write_api: ProdWriteApi,
    event: LambdaEvent<Value>,
) -> Result<WriteLambdaPayload, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_request_payload(payload) {
        Ok(request) => request,
        Err(payload) => return Ok(payload),
    };

    let result = handle_write_request(
        async |documents| write_api.ingest(documents).await,
        async |doc_ids| write_api.delete(doc_ids).await,
        request,
    )
    .await;

    let payload = match result {
        Ok(response) => WriteLambdaPayload::Success(response),
        Err(error) => WriteLambdaPayload::Error(error),
    };

    Ok(payload)
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
}
