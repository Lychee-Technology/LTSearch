use std::time::{SystemTime, UNIX_EPOCH};

use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::bootstrap::{
    build_embedding_generator_from_env, build_embedding_provider_from_env, s3_client_from_env,
    BuildConfig,
};
use ltsearch::build_lambda::{handle_build_request, BuildLambdaError, BuildRequest, BuildResponse};
use ltsearch::error::IndexError;
use ltsearch::indexing::{BuildIndexRequest, LocalIndexBuilder};
use ltsearch::indexing::{IndexPublisher, PublishRequest};
use ltsearch::write::WriteAheadLog;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum BuildLambdaPayload {
    Success(BuildResponse),
    Error(BuildLambdaError),
}

fn decode_request_payload(payload: Value) -> Result<BuildRequest, BuildLambdaPayload> {
    serde_json::from_value(payload).map_err(|source| {
        BuildLambdaPayload::Error(BuildLambdaError {
            error_type: "validation_error".into(),
            message: format!("failed to deserialize build request: {source}"),
        })
    })
}

#[derive(Clone)]
struct BuildState {
    config: BuildConfig,
    s3_client: aws_sdk_s3::Client,
}

async fn function_handler(
    state: BuildState,
    event: LambdaEvent<Value>,
) -> Result<BuildLambdaPayload, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_request_payload(payload) {
        Ok(request) => request,
        Err(payload) => return Ok(payload),
    };

    let result = handle_build_request(
        async |request: BuildRequest| {
            let wal_storage =
                AwsS3WalStorage::new(state.config.s3_bucket.clone(), state.s3_client.clone());
            let wal = WriteAheadLog::new(wal_storage);
            let records =
                wal.read(&request.wal_key)
                    .await
                    .map_err(|error| IndexError::Operation {
                        message: format!("failed to read WAL records: {error}"),
                    })?;

            let provider =
                build_embedding_provider_from_env().map_err(|error| IndexError::Operation {
                    message: error.to_string(),
                })?;
            let embedding_generator =
                build_embedding_generator_from_env(provider).map_err(|error| {
                    IndexError::Operation {
                        message: error.to_string(),
                    }
                })?;

            // The build is sync + CPU-heavy, so run it off the async runtime.
            let builder = LocalIndexBuilder::new(&state.config.artifact_root, embedding_generator);
            let build_request = BuildIndexRequest {
                version_id: request.version_id,
                created_at: current_time_millis(),
                embedding_dim: request.embedding_dim,
                records,
            };
            tokio::task::spawn_blocking(move || builder.build(&build_request))
                .await
                .map_err(|error| IndexError::Operation {
                    message: format!("build task panicked: {error}"),
                })?
        },
        async |manifest| {
            let publish_storage =
                AwsPublishStorage::new(state.config.s3_bucket.clone(), state.s3_client.clone());
            let publisher = IndexPublisher::new(&state.config.artifact_root, publish_storage);
            publisher
                .publish(&PublishRequest {
                    manifest,
                    expected_current_version: None,
                    updated_at: current_time_millis(),
                })
                .await
        },
        request,
    )
    .await;

    let payload = match result {
        Ok(response) => BuildLambdaPayload::Success(response),
        Err(error) => BuildLambdaPayload::Error(error),
    };

    Ok(payload)
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let config = BuildConfig::from_env()?;
        let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let state = BuildState {
            config,
            s3_client: s3_client_from_env(&sdk_config),
        };

        lambda_runtime::run(service_fn(move |event| {
            let state = state.clone();
            async move { function_handler(state, event).await }
        }))
        .await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_event_payload_returns_typed_error_envelope() {
        let payload = decode_request_payload(serde_json::json!({"version_id": "wrong"}));

        match payload {
            Ok(_) => panic!("expected malformed payload to return an error envelope"),
            Err(payload) => match payload {
                BuildLambdaPayload::Success(_) => {
                    panic!("expected malformed payload to produce an error envelope")
                }
                BuildLambdaPayload::Error(error) => {
                    assert_eq!(error.error_type, "validation_error");
                    assert!(error
                        .message
                        .contains("failed to deserialize build request"));
                }
            },
        }
    }

    #[test]
    fn valid_build_request_deserializes_correctly() {
        let payload = serde_json::json!({
            "batch_id": "batch-abc",
            "wal_key": "wal/2026/03/19/batch-abc.jsonl",
            "version_id": 1,
            "embedding_dim": 3,
        });

        let request = decode_request_payload(payload).expect("expected valid request to decode");
        assert_eq!(request.batch_id, "batch-abc");
        assert_eq!(request.wal_key, "wal/2026/03/19/batch-abc.jsonl");
        assert_eq!(request.version_id, 1);
        assert_eq!(request.embedding_dim, 3);
    }
}
