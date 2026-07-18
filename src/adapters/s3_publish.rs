use std::fs;
use std::path::Path;

use async_trait::async_trait;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;

use crate::error::PublishError;
use crate::indexing::{PublishStorage, UploadMode, VersionedObject};

/// Discriminating phrase in the `PublishError::Operation` message this adapter
/// raises when a `CreateOnly` upload hits an already-present object. Because
/// `PublishError` collapses that precondition failure into a stringly-typed
/// `Operation { message }`, this substring is the only stable discriminator the
/// error shape offers. It lives here, beside its single construction site, so
/// callers that key idempotent re-runs off the CreateOnly conflict (e.g. the
/// `static_activate` bin) match against the same const the message is built
/// from — a wording edit can never silently desync the two.
pub const CREATE_ONLY_CONFLICT_PHRASE: &str = "refusing to overwrite existing version artifact";

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
    async fn upload_directory(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
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
                self.upload_directory(&child_key, &path, mode).await?;
            } else {
                self.upload_file(&child_key, &path, mode).await?;
            }
        }

        Ok(())
    }

    async fn upload_file(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
        let bytes = fs::read(source).map_err(|error| PublishError::Operation {
            message: format!("failed to read {}: {error}", source.display()),
        })?;

        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes));
        if mode == UploadMode::CreateOnly {
            request = request.if_none_match("*");
        }

        match request.send().await {
            Ok(_) => Ok(()),
            Err(error) if mode == UploadMode::CreateOnly && is_precondition_failure(&error) => {
                Err(PublishError::Operation {
                    message: format!(
                        "{CREATE_ONLY_CONFLICT_PHRASE} {key}: version artifacts are immutable"
                    ),
                })
            }
            Err(error) => Err(PublishError::Operation {
                message: format!("failed to upload {key}: {error}"),
            }),
        }
    }

    async fn read(&self, key: &str) -> Result<Option<VersionedObject>, PublishError> {
        let object = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(object) => object,
            Err(error) if is_missing_object_error(&error) => return Ok(None),
            Err(error) => {
                return Err(PublishError::Operation {
                    message: format!("failed to load object {key}: {error}"),
                });
            }
        };

        let etag =
            object
                .e_tag()
                .map(|etag| etag.to_string())
                .ok_or_else(|| PublishError::Operation {
                    message: format!("object {key} has no ETag"),
                })?;
        let bytes = object
            .body
            .collect()
            .await
            .map_err(|error| PublishError::Operation {
                message: format!("failed to read object body {key}: {error}"),
            })?
            .into_bytes()
            .to_vec();

        Ok(Some(VersionedObject { bytes, etag }))
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_etag: Option<&str>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(new_value.to_vec()));

        // S3 conditional writes make the swap atomic: If-Match guards
        // replacement of an existing head, If-None-Match guards creation.
        request = match expected_etag {
            Some(etag) => request.if_match(etag),
            None => request.if_none_match("*"),
        };

        match request.send().await {
            Ok(_) => Ok(true),
            Err(error) if is_precondition_failure(&error) => Ok(false),
            Err(error) => Err(PublishError::Operation {
                message: format!("failed to update {key}: {error}"),
            }),
        }
    }
}

fn is_precondition_failure(
    error: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::put_object::PutObjectError>,
) -> bool {
    if matches!(
        error
            .as_service_error()
            .and_then(ProvideErrorMetadata::code),
        Some("PreconditionFailed") | Some("ConditionalRequestConflict")
    ) {
        return true;
    }

    matches!(
        error
            .raw_response()
            .map(|response| response.status().as_u16()),
        Some(412) | Some(409)
    )
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
