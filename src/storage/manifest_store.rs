use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::models::IndexManifest;

use super::s3_paths::{version_manifest_key, INDEX_HEAD_KEY};

const MIN_PLAUSIBLE_EPOCH_MILLIS: i64 = 1_000_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestHead {
    pub version_id: u64,
    pub manifest_path: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveManifest {
    pub head: ManifestHead,
    pub manifest: IndexManifest,
}

#[derive(Debug, Error)]
pub enum ManifestStoreError {
    #[error("manifest head is missing at {path}")]
    MissingHead { path: PathBuf },
    #[error("active manifest is missing at {path}")]
    MissingManifest { path: PathBuf },
    #[error("manifest head is invalid: {message}")]
    InvalidHead { message: String },
    #[error("manifest file at {path} is invalid: {message}")]
    InvalidManifest { path: PathBuf, message: String },
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub trait ManifestStore {
    fn load_head(&self) -> Result<ManifestHead, ManifestStoreError>;
    fn load_active_manifest(&self) -> Result<ActiveManifest, ManifestStoreError>;

    fn load_active_version(&self) -> Result<u64, ManifestStoreError> {
        self.load_head().map(|head| head.version_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalManifestStore {
    root: PathBuf,
}

impl LocalManifestStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn head_path(&self) -> PathBuf {
        self.root.join(INDEX_HEAD_KEY)
    }

    fn manifest_fs_path(&self, head: &ManifestHead) -> PathBuf {
        self.root.join(&head.manifest_path)
    }
}

impl ManifestStore for LocalManifestStore {
    fn load_head(&self) -> Result<ManifestHead, ManifestStoreError> {
        let path = self.head_path();
        let contents = read_to_string(&path, true)?;
        let head: ManifestHead =
            serde_json::from_str(&contents).map_err(|source| ManifestStoreError::InvalidHead {
                message: source.to_string(),
            })?;

        if head.version_id == 0 {
            return Err(ManifestStoreError::InvalidHead {
                message: "version_id must be positive".into(),
            });
        }
        if head.updated_at < MIN_PLAUSIBLE_EPOCH_MILLIS {
            return Err(ManifestStoreError::InvalidHead {
                message: "updated_at must be a plausible epoch millis value".into(),
            });
        }

        let expected_manifest_path = version_manifest_key(head.version_id);
        if head.manifest_path != expected_manifest_path {
            return Err(ManifestStoreError::InvalidHead {
                message: format!(
                    "manifest_path must match version_id; expected {expected_manifest_path}"
                ),
            });
        }

        Ok(head)
    }

    fn load_active_manifest(&self) -> Result<ActiveManifest, ManifestStoreError> {
        let head = self.load_head()?;
        let path = self.manifest_fs_path(&head);
        let contents = read_to_string(&path, false)?;
        let manifest: IndexManifest = serde_json::from_str(&contents).map_err(|source| {
            ManifestStoreError::InvalidManifest {
                path: path.clone(),
                message: source.to_string(),
            }
        })?;

        manifest
            .validate()
            .map_err(|source| ManifestStoreError::InvalidManifest {
                path: path.clone(),
                message: source.to_string(),
            })?;

        if manifest.version_id != head.version_id {
            return Err(ManifestStoreError::InvalidManifest {
                path,
                message: "manifest version_id must match _head version_id".into(),
            });
        }

        Ok(ActiveManifest { head, manifest })
    }
}

fn read_to_string(path: &Path, is_head: bool) -> Result<String, ManifestStoreError> {
    fs::read_to_string(path).map_err(|source| match source.kind() {
        ErrorKind::NotFound if is_head => ManifestStoreError::MissingHead {
            path: path.to_path_buf(),
        },
        ErrorKind::NotFound => ManifestStoreError::MissingManifest {
            path: path.to_path_buf(),
        },
        _ => ManifestStoreError::Io {
            path: path.to_path_buf(),
            source,
        },
    })
}
