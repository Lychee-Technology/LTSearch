use std::fs;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{PublishError, ValidationError};
use crate::models::IndexManifest;
use crate::storage::{version_manifest_key, ManifestHead, INDEX_HEAD_KEY};

#[derive(Debug)]
struct PreparedDirectoryUpload {
    key: String,
    source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRequest {
    pub manifest: IndexManifest,
    pub expected_current_version: Option<u64>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackRequest {
    pub target_version_id: u64,
    pub expected_current_version: Option<u64>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    pub activated_version_id: u64,
    pub previous_version_id: Option<u64>,
}

#[async_trait]
pub trait PublishStorage: Clone + Send + Sync + 'static {
    async fn upload_directory(&self, key: &str, source: &Path) -> Result<(), PublishError>;
    async fn upload_file(&self, key: &str, source: &Path) -> Result<(), PublishError>;
    async fn read(&self, key: &str) -> Result<Option<Vec<u8>>, PublishError>;
    async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new_value: &[u8],
    ) -> Result<bool, PublishError>;
}

#[derive(Debug, Clone)]
pub struct IndexPublisher<S> {
    artifact_root: PathBuf,
    storage: S,
}

impl<S> IndexPublisher<S>
where
    S: PublishStorage,
{
    pub fn new(artifact_root: impl AsRef<Path>, storage: S) -> Self {
        Self {
            artifact_root: artifact_root.as_ref().to_path_buf(),
            storage,
        }
    }

    pub async fn publish(&self, request: &PublishRequest) -> Result<PublishResult, PublishError> {
        request.manifest.validate()?;
        validate_updated_at(request.updated_at)?;

        let current_head_bytes = self.storage.read(INDEX_HEAD_KEY).await?;
        let current_head = current_head_bytes.as_deref().map(parse_head).transpose()?;

        validate_publish_version(request.manifest.version_id)?;

        if current_head.as_ref().map(|head| head.version_id) != request.expected_current_version {
            return Err(PublishError::Operation {
                message: format!(
                    "publish conflict: expected current version {:?}, found {:?}",
                    request.expected_current_version,
                    current_head.as_ref().map(|head| head.version_id)
                ),
            });
        }

        let manifest_key = version_manifest_key(request.manifest.version_id);
        let manifest_source = artifact_source_path(&self.artifact_root, &manifest_key)?;
        validate_manifest_file_matches_request(&manifest_source, &request.manifest)?;

        let mut directory_uploads = Vec::with_capacity(request.manifest.shards.len() * 2);

        for shard in &request.manifest.shards {
            let lance_key = s3_key(&shard.lance_path)?;
            let tantivy_key = s3_key(&shard.tantivy_path)?;
            let lance_source = artifact_source_path(&self.artifact_root, &lance_key)?;
            let tantivy_source = artifact_source_path(&self.artifact_root, &tantivy_key)?;

            validate_directory_tree_within_root(&self.artifact_root, &lance_source)?;
            validate_directory_tree_within_root(&self.artifact_root, &tantivy_source)?;

            directory_uploads.push(PreparedDirectoryUpload {
                key: lance_key,
                source: lance_source,
            });
            directory_uploads.push(PreparedDirectoryUpload {
                key: tantivy_key,
                source: tantivy_source,
            });
        }

        for upload in &directory_uploads {
            self.storage
                .upload_directory(&upload.key, &upload.source)
                .await?;
        }

        if let Some(static_source) = static_artifact_source(&self.artifact_root)? {
            self.storage
                .upload_directory("static", &static_source)
                .await?;
        }

        self.storage
            .upload_file(&manifest_key, &manifest_source)
            .await?;

        let new_head = HeadDocument {
            version_id: request.manifest.version_id,
            manifest_path: manifest_key,
            updated_at: request.updated_at,
        };
        let new_head_bytes =
            serde_json::to_vec_pretty(&new_head).map_err(|source| PublishError::Operation {
                message: format!("failed to serialize manifest head: {source}"),
            })?;

        let swapped = self
            .storage
            .compare_and_swap(
                INDEX_HEAD_KEY,
                current_head_bytes.as_deref(),
                &new_head_bytes,
            )
            .await?;
        if !swapped {
            let observed = self
                .storage
                .read(INDEX_HEAD_KEY)
                .await?
                .as_deref()
                .map(parse_head)
                .transpose()?
                .map(|head| head.version_id);
            return Err(PublishError::Operation {
                message: format!(
                    "publish conflict: active version changed during _head update (expected {:?}, observed {:?})",
                    request.expected_current_version, observed
                ),
            });
        }

        Ok(PublishResult {
            activated_version_id: request.manifest.version_id,
            previous_version_id: current_head.map(|head| head.version_id),
        })
    }

    pub async fn rollback(&self, request: &RollbackRequest) -> Result<PublishResult, PublishError> {
        validate_publish_version(request.target_version_id)?;
        validate_updated_at(request.updated_at)?;

        let current_head_bytes = self.storage.read(INDEX_HEAD_KEY).await?;
        let current_head = current_head_bytes.as_deref().map(parse_head).transpose()?;

        if current_head.as_ref().map(|head| head.version_id) != request.expected_current_version {
            return Err(PublishError::Operation {
                message: format!(
                    "rollback conflict: expected current version {:?}, found {:?}",
                    request.expected_current_version,
                    current_head.as_ref().map(|head| head.version_id)
                ),
            });
        }

        let target_manifest_key = version_manifest_key(request.target_version_id);
        let target_manifest_bytes =
            self.storage
                .read(&target_manifest_key)
                .await?
                .ok_or_else(|| PublishError::Operation {
                    message: format!("rollback target manifest missing: {target_manifest_key}"),
                })?;
        validate_stored_manifest(
            &target_manifest_key,
            &target_manifest_bytes,
            request.target_version_id,
        )?;

        let new_head = HeadDocument {
            version_id: request.target_version_id,
            manifest_path: target_manifest_key,
            updated_at: request.updated_at,
        };
        let new_head_bytes =
            serde_json::to_vec_pretty(&new_head).map_err(|source| PublishError::Operation {
                message: format!("failed to serialize manifest head: {source}"),
            })?;

        let swapped = self
            .storage
            .compare_and_swap(
                INDEX_HEAD_KEY,
                current_head_bytes.as_deref(),
                &new_head_bytes,
            )
            .await?;
        if !swapped {
            let observed = self
                .storage
                .read(INDEX_HEAD_KEY)
                .await?
                .as_deref()
                .map(parse_head)
                .transpose()?
                .map(|head| head.version_id);
            return Err(PublishError::Operation {
                message: format!(
                    "rollback conflict: active version changed during _head update (expected {:?}, observed {:?})",
                    request.expected_current_version, observed
                ),
            });
        }

        Ok(PublishResult {
            activated_version_id: request.target_version_id,
            previous_version_id: current_head.map(|head| head.version_id),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HeadDocument {
    version_id: u64,
    manifest_path: String,
    updated_at: i64,
}

fn parse_head(bytes: &[u8]) -> Result<ManifestHead, PublishError> {
    let head = serde_json::from_slice::<ManifestHead>(bytes).map_err(|source| {
        PublishError::Operation {
            message: format!("failed to parse current manifest head: {source}"),
        }
    })?;

    if head.version_id == 0 {
        return Err(PublishError::Validation(ValidationError::InvalidValue {
            field: "version_id",
        }));
    }
    validate_updated_at(head.updated_at)?;
    if head.manifest_path != version_manifest_key(head.version_id) {
        return Err(PublishError::Validation(ValidationError::Mismatch {
            field: "manifest_path",
            expected: "version_manifest_key(version_id)",
        }));
    }

    Ok(head)
}

fn s3_key(value: &str) -> Result<String, PublishError> {
    let Some(suffix) = value.strip_prefix("s3://") else {
        return Err(PublishError::Validation(ValidationError::InvalidValue {
            field: "s3_path",
        }));
    };
    let Some((bucket, key)) = suffix.split_once('/') else {
        return Err(PublishError::Validation(ValidationError::InvalidValue {
            field: "s3_path",
        }));
    };
    if bucket.is_empty() || key.is_empty() {
        return Err(PublishError::Validation(ValidationError::InvalidValue {
            field: "s3_path",
        }));
    }
    validate_relative_storage_key(key)?;

    Ok(key.to_string())
}

fn artifact_source_path(artifact_root: &Path, key: &str) -> Result<PathBuf, PublishError> {
    validate_relative_storage_key(key)?;

    let root = artifact_root
        .canonicalize()
        .map_err(|source| PublishError::Operation {
            message: format!(
                "failed to canonicalize artifact root {}: {source}",
                artifact_root.display()
            ),
        })?;
    let joined = artifact_root.join(key);
    let canonical = joined
        .canonicalize()
        .map_err(|source| PublishError::Operation {
            message: format!(
                "failed to canonicalize artifact source {}: {source}",
                joined.display()
            ),
        })?;

    if !canonical.starts_with(&root) {
        return Err(PublishError::Operation {
            message: format!(
                "artifact source escapes artifact root: {} -> {}",
                joined.display(),
                canonical.display()
            ),
        });
    }

    Ok(canonical)
}

fn validate_publish_version(version_id: u64) -> Result<(), PublishError> {
    if version_id == 0 {
        return Err(PublishError::Validation(ValidationError::MustBePositive {
            field: "version_id",
        }));
    }

    Ok(())
}

fn validate_relative_storage_key(key: &str) -> Result<(), PublishError> {
    if key.is_empty() {
        return Err(PublishError::Validation(ValidationError::InvalidValue {
            field: "artifact_key",
        }));
    }

    let path = Path::new(key);
    if path.is_absolute() {
        return Err(PublishError::Validation(ValidationError::InvalidValue {
            field: "artifact_key",
        }));
    }

    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(PublishError::Validation(ValidationError::InvalidValue {
                field: "artifact_key",
            }));
        }
    }

    Ok(())
}

fn validate_updated_at(updated_at: i64) -> Result<(), PublishError> {
    if updated_at < 1_000_000_000_000 {
        return Err(PublishError::Validation(ValidationError::InvalidValue {
            field: "updated_at",
        }));
    }

    Ok(())
}

fn static_artifact_source(artifact_root: &Path) -> Result<Option<PathBuf>, PublishError> {
    let static_dir = artifact_root.join("static");
    if !static_dir.exists() {
        return Ok(None);
    }

    let canonical = static_dir
        .canonicalize()
        .map_err(|source| PublishError::Operation {
            message: format!(
                "failed to canonicalize static artifact directory {}: {source}",
                static_dir.display()
            ),
        })?;
    validate_directory_tree_within_root(artifact_root, &canonical)?;
    Ok(Some(canonical))
}

fn validate_manifest_file_matches_request(
    path: &Path,
    expected_manifest: &IndexManifest,
) -> Result<(), PublishError> {
    let bytes = fs::read(path).map_err(|source| PublishError::Operation {
        message: format!(
            "failed to read manifest source {}: {source}",
            path.display()
        ),
    })?;
    let manifest = serde_json::from_slice::<IndexManifest>(&bytes).map_err(|source| {
        PublishError::Operation {
            message: format!(
                "failed to parse manifest source {}: {source}",
                path.display()
            ),
        }
    })?;

    if &manifest != expected_manifest {
        return Err(PublishError::Operation {
            message: format!(
                "manifest source {} does not match publish request manifest",
                path.display()
            ),
        });
    }

    Ok(())
}

fn validate_stored_manifest(
    key: &str,
    bytes: &[u8],
    expected_version_id: u64,
) -> Result<(), PublishError> {
    let manifest = serde_json::from_slice::<IndexManifest>(bytes).map_err(|source| {
        PublishError::Operation {
            message: format!("failed to parse stored manifest {key}: {source}"),
        }
    })?;
    manifest.validate()?;
    if manifest.version_id != expected_version_id {
        return Err(PublishError::Operation {
            message: format!(
                "stored manifest {key} does not match rollback target version {expected_version_id}"
            ),
        });
    }

    Ok(())
}

fn validate_directory_tree_within_root(
    artifact_root: &Path,
    directory: &Path,
) -> Result<(), PublishError> {
    let directory_metadata = fs::metadata(directory).map_err(|source| PublishError::Operation {
        message: format!(
            "failed to inspect artifact source {}: {source}",
            directory.display()
        ),
    })?;
    if !directory_metadata.is_dir() {
        return Err(PublishError::Operation {
            message: format!("publish source is not a directory: {}", directory.display()),
        });
    }

    let root = artifact_root
        .canonicalize()
        .map_err(|source| PublishError::Operation {
            message: format!(
                "failed to canonicalize artifact root {}: {source}",
                artifact_root.display()
            ),
        })?;
    let mut stack = vec![directory.to_path_buf()];

    while let Some(current) = stack.pop() {
        let current_metadata =
            fs::symlink_metadata(&current).map_err(|source| PublishError::Operation {
                message: format!(
                    "failed to inspect artifact source {}: {source}",
                    current.display()
                ),
            })?;
        if current_metadata.file_type().is_symlink() {
            let canonical = current
                .canonicalize()
                .map_err(|source| PublishError::Operation {
                    message: format!(
                        "failed to canonicalize artifact source {}: {source}",
                        current.display()
                    ),
                })?;
            if !canonical.starts_with(&root) {
                return Err(PublishError::Operation {
                    message: format!(
                        "artifact source escapes artifact root: {} -> {}",
                        current.display(),
                        canonical.display()
                    ),
                });
            }

            continue;
        }

        if current.is_dir() {
            for entry in fs::read_dir(&current).map_err(|source| PublishError::Operation {
                message: format!(
                    "failed to read artifact directory {}: {source}",
                    current.display()
                ),
            })? {
                let entry = entry.map_err(|source| PublishError::Operation {
                    message: format!(
                        "failed to read artifact directory {}: {source}",
                        current.display()
                    ),
                })?;
                stack.push(entry.path());
            }
        }
    }

    Ok(())
}

#[allow(dead_code)]
fn ensure_file_exists(path: &Path) -> Result<(), PublishError> {
    let metadata = fs::metadata(path).map_err(|source| PublishError::Operation {
        message: format!("missing publish source {}: {source}", path.display()),
    })?;
    if !metadata.is_file() {
        return Err(PublishError::Operation {
            message: format!("publish source is not a file: {}", path.display()),
        });
    }

    Ok(())
}
