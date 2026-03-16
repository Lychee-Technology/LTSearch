use std::fs;
use std::path::Path;

use async_trait::async_trait;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;

use crate::error::PublishError;
use crate::indexing::PublishStorage;

#[derive(Clone)]
pub struct AwsPublishStorage {
    bucket: String,
    client: S3Client,
}

impl AwsPublishStorage {
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
impl PublishStorage for AwsPublishStorage {
    async fn upload_directory(&self, key: &str, source: &Path) -> Result<(), PublishError> {
        if !source.is_dir() {
            return Err(PublishError::Operation {
                message: format!("missing source directory {}", source.display()),
            });
        }

        for entry in fs::read_dir(source).map_err(|error| PublishError::Operation {
            message: format!("failed to read directory {}: {error}", source.display()),
        })? {
            let entry = entry.map_err(|error| PublishError::Operation {
                message: format!("failed to iterate directory {}: {error}", source.display()),
            })?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let child_key = format!("{key}/{name}");

            if path.is_dir() {
                self.upload_directory(&child_key, &path).await?;
            } else {
                self.upload_file(&child_key, &path).await?;
            }
        }

        Ok(())
    }

    async fn upload_file(&self, key: &str, source: &Path) -> Result<(), PublishError> {
        let bytes = fs::read(source).map_err(|error| PublishError::Operation {
            message: format!("failed to read {}: {error}", source.display()),
        })?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|error| PublishError::Operation {
                message: format!("failed to upload {key}: {error}"),
            })?;

        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Option<Vec<u8>>, PublishError> {
        let object = match self.client.get_object().bucket(&self.bucket).key(key).send().await {
            Ok(object) => object,
            Err(error) if is_missing_object_error(&error) => return Ok(None),
            Err(error) => {
                return Err(PublishError::Operation {
                    message: format!("failed to load object {key}: {error}"),
                });
            }
        };

        let bytes = object
            .body
            .collect()
            .await
            .map_err(|error| PublishError::Operation {
                message: format!("failed to read object body {key}: {error}"),
            })?
            .into_bytes()
            .to_vec();

        Ok(Some(bytes))
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        let current = self.read(key).await?;
        if current.as_deref() != expected {
            return Ok(false);
        }

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(new_value.to_vec()))
            .send()
            .await
            .map_err(|error| PublishError::Operation {
                message: format!("failed to update {key}: {error}"),
            })?;

        Ok(true)
    }
}

fn is_missing_object_error(error: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>) -> bool {
    matches!(
        error.as_service_error().and_then(ProvideErrorMetadata::code),
        Some("NoSuchKey") | Some("NotFound")
    )
}
