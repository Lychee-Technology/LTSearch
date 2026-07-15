//! `ArtifactSync` 的 S3 实现：把活跃版本所需的 index/lance/static 前缀下载到本地
//! `artifact_root`。逻辑原样搬自 query_service 的 sync_prefix，`#[cfg(feature = "aws")]`
//! 门控，S3 细节收敛于此，query 侧调用点只依赖中立契约。

use std::fs;
use std::path::Path;

use async_trait::async_trait;

use crate::bootstrap::s3_client_from_env;
use crate::contracts::ArtifactSync;

pub struct S3ArtifactSync {
    bucket: String,
}

impl S3ArtifactSync {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
        }
    }
}

#[async_trait]
impl ArtifactSync for S3ArtifactSync {
    async fn sync(&self, artifact_root: &Path) -> Result<(), String> {
        fs::create_dir_all(artifact_root)
            .map_err(|error| format!("failed to create query artifact root: {error}"))?;

        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = s3_client_from_env(&config);

        for prefix in synced_artifact_prefixes() {
            sync_prefix(&client, &self.bucket, prefix, artifact_root).await?;
        }

        Ok(())
    }
}

fn synced_artifact_prefixes() -> Vec<&'static str> {
    vec!["index/", "lance/", "static/"]
}

async fn sync_prefix(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
    artifact_root: &Path,
) -> Result<(), String> {
    let mut continuation_token = None;

    loop {
        let mut request = client.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(token) = continuation_token.as_deref() {
            request = request.continuation_token(token);
        }

        let response = request
            .send()
            .await
            .map_err(|error| format!("failed to list {prefix} objects from S3: {error}"))?;

        for object in response.contents() {
            let Some(key) = object.key() else {
                continue;
            };
            if key.ends_with('/') {
                continue;
            }

            let body = client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|error| format!("failed to download {key} from S3: {error}"))?
                .body
                .collect()
                .await
                .map_err(|error| format!("failed to read {key} body from S3: {error}"))?
                .into_bytes();

            let destination = artifact_root.join(key);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!("failed to create local artifact directories: {error}")
                })?;
            }
            fs::write(&destination, body)
                .map_err(|error| format!("failed to write local artifact {key}: {error}"))?;
        }

        if !response.is_truncated().unwrap_or(false) {
            break;
        }
        continuation_token = response
            .next_continuation_token()
            .map(|value| value.to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_artifact_prefixes_include_static_artifacts() {
        assert_eq!(
            synced_artifact_prefixes(),
            vec!["index/", "lance/", "static/"]
        );
    }
}
