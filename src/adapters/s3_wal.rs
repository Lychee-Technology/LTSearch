use async_trait::async_trait;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;

use crate::error::IngestError;
use crate::write::WalStorage;

#[derive(Clone)]
pub struct AwsS3WalStorage {
    bucket: String,
    client: S3Client,
}

impl AwsS3WalStorage {
    pub fn new(bucket: impl Into<String>, client: S3Client) -> Self {
        Self {
            bucket: bucket.into(),
            client,
        }
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn client(&self) -> &S3Client {
        &self.client
    }
}

#[async_trait]
impl WalStorage for AwsS3WalStorage {
    async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        let existing = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(object) => object
                .body
                .collect()
                .await
                .map_err(|error| IngestError::Operation {
                    message: format!("failed to read existing WAL object {key}: {error}"),
                })?
                .into_bytes()
                .to_vec(),
            Err(error) if is_missing_object_error(&error) => Vec::new(),
            Err(error) => {
                return Err(IngestError::Operation {
                    message: format!("failed to load existing WAL object {key}: {error}"),
                });
            }
        };

        let mut combined = existing;
        combined.extend_from_slice(bytes);

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(combined))
            .send()
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to write WAL object {key}: {error}"),
            })?;

        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Vec<u8>, IngestError> {
        let object = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to load WAL object {key}: {error}"),
            })?;

        let bytes = object
            .body
            .collect()
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to read WAL object body {key}: {error}"),
            })?
            .into_bytes()
            .to_vec();

        Ok(bytes)
    }
}

fn is_missing_object_error(
    error: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
) -> bool {
    matches!(
        error
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("NoSuchKey") | Some("NotFound")
    )
}
