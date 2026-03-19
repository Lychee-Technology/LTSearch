use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::build_lambda::{BuildLambdaError, BuildRequest, BuildResponse};
use ltsearch::embedding::{
    fixed_generator_from_env, provider_from_env_or_default, EmbeddingGenerator, EmbeddingProvider,
};
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

async fn function_handler(event: LambdaEvent<Value>) -> Result<BuildLambdaPayload, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_request_payload(payload) {
        Ok(request) => request,
        Err(payload) => return Ok(payload),
    };

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

    let s3_bucket = env::var("LTSEARCH_BUILD_S3_BUCKET").unwrap_or_default();
    let artifact_root =
        env::var("LTSEARCH_BUILD_ARTIFACT_ROOT").unwrap_or_else(|_| "/tmp/ltsearch".into());
    let embedding_provider = match provider_from_env_or_default(
        "LTSEARCH_BUILD_EMBEDDING_PROVIDER",
        EmbeddingProvider::Fixed,
    ) {
        Ok(provider) => provider,
        Err(error) => {
            return Ok(BuildLambdaPayload::Error(BuildLambdaError {
                error_type: "build_error".into(),
                message: error.to_string(),
            }))
        }
    };

    let s3_client = aws_sdk_s3::Client::new(&config);

    // Read WAL records from S3
    let wal_storage = AwsS3WalStorage::new(s3_bucket.clone(), s3_client.clone());
    let wal = WriteAheadLog::new(wal_storage);
    let records = wal.read(&request.wal_key).await.map_err(|error| {
        BuildLambdaPayload::Error(BuildLambdaError {
            error_type: "build_error".into(),
            message: format!("failed to read WAL records: {error}"),
        })
    });
    let records = match records {
        Ok(records) => records,
        Err(payload) => return Ok(payload),
    };

    // Build the embedding generator
    let embedding_generator = match build_embedding_generator(embedding_provider) {
        Ok(generator) => generator,
        Err(payload) => return Ok(payload),
    };

    // Build the index (sync + CPU-heavy, use spawn_blocking)
    let builder = LocalIndexBuilder::new(&artifact_root, embedding_generator);
    let build_request = BuildIndexRequest {
        version_id: request.version_id,
        created_at: current_time_millis(),
        embedding_dim: request.embedding_dim,
        records,
    };

    let build_result = tokio::task::spawn_blocking(move || builder.build(&build_request))
        .await
        .map_err(|error| Error::from(format!("build task panicked: {error}")))?;

    let build_result = match build_result {
        Ok(result) => result,
        Err(error) => {
            return Ok(BuildLambdaPayload::Error(BuildLambdaError::from(error)));
        }
    };

    // Publish the index (async)
    let publish_storage = AwsPublishStorage::new(s3_bucket, s3_client);
    let publisher = IndexPublisher::new(&artifact_root, publish_storage);
    let publish_request = PublishRequest {
        manifest: build_result.manifest.clone(),
        expected_current_version: None,
        updated_at: current_time_millis(),
    };

    let publish_result = match publisher.publish(&publish_request).await {
        Ok(result) => result,
        Err(error) => {
            return Ok(BuildLambdaPayload::Error(BuildLambdaError::from(error)));
        }
    };

    Ok(BuildLambdaPayload::Success(BuildResponse {
        activated_version_id: publish_result.activated_version_id,
        previous_version_id: publish_result.previous_version_id,
        document_count: build_result.documents.len(),
    }))
}

fn build_embedding_generator(
    provider: EmbeddingProvider,
) -> Result<Box<dyn EmbeddingGenerator>, BuildLambdaPayload> {
    match provider {
        EmbeddingProvider::Fixed => fixed_generator_from_env(
            "LTSEARCH_BUILD_FIXED_EMBEDDING",
            Some("LTSEARCH_BUILD_EMBEDDING_DIM"),
        )
        .map(|generator| Box::new(generator) as Box<dyn EmbeddingGenerator>)
        .map_err(|error| {
            BuildLambdaPayload::Error(BuildLambdaError {
                error_type: "build_error".into(),
                message: error.to_string(),
            })
        }),
        EmbeddingProvider::LTEmbed => Err(BuildLambdaPayload::Error(BuildLambdaError {
            error_type: "build_error".into(),
            message: "unsupported LTSEARCH_BUILD_EMBEDDING_PROVIDER: ltembed".into(),
        })),
    }
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?
        .block_on(async { lambda_runtime::run(service_fn(function_handler)).await })
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
