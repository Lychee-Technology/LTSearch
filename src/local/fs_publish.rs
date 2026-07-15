//! 文件系统制品存储：把 `key` 当作 `root` 下相对路径。etag 用内容 FNV-1a 哈希
//! 的十六进制，`compare_and_swap` 比较目标文件当前 etag 与 `expected_etag`
//! 决定是否写入——本地单进程无并发，但保持与 S3 ETag CAS 同构的语义，让
//! `next_version_id` / `IndexPublisher` 的发布路径无需改动即可跑在本地。

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::PublishError;
use crate::indexing::{PublishStorage, UploadMode, VersionedObject};

#[derive(Debug, Clone)]
pub struct LocalFsPublishStorage {
    root: PathBuf,
}

impl LocalFsPublishStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

fn etag_of(bytes: &[u8]) -> String {
    // FNV-1a 64-bit: 稳定、无依赖，仅作本地 CAS 身份用途，非加密强度。
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn copy_tree(source: &Path, dest: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, dest).map(|_| ())
    }
}

#[async_trait]
impl PublishStorage for LocalFsPublishStorage {
    async fn upload_directory(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
        let dest = self.path_for(key);
        if mode == UploadMode::CreateOnly && dest.exists() {
            return Err(PublishError::Operation {
                message: format!("directory {key} already exists"),
            });
        }
        copy_tree(source, &dest).map_err(|error| PublishError::Operation {
            message: format!("failed to upload dir {key}: {error}"),
        })
    }

    async fn upload_file(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
        let dest = self.path_for(key);
        if mode == UploadMode::CreateOnly && dest.exists() {
            return Err(PublishError::Operation {
                message: format!("file {key} already exists"),
            });
        }
        copy_tree(source, &dest).map_err(|error| PublishError::Operation {
            message: format!("failed to upload file {key}: {error}"),
        })
    }

    async fn read(&self, key: &str) -> Result<Option<VersionedObject>, PublishError> {
        match std::fs::read(self.path_for(key)) {
            Ok(bytes) => {
                let etag = etag_of(&bytes);
                Ok(Some(VersionedObject { bytes, etag }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(PublishError::Operation {
                message: format!("failed to read {key}: {error}"),
            }),
        }
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_etag: Option<&str>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        let path = self.path_for(key);
        let current = match std::fs::read(&path) {
            Ok(bytes) => Some(etag_of(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(PublishError::Operation {
                    message: format!("failed to read {key} for CAS: {error}"),
                })
            }
        };
        if current.as_deref() != expected_etag {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| PublishError::Operation {
                message: format!("failed to create dir for {key}: {error}"),
            })?;
        }
        std::fs::write(&path, new_value).map_err(|error| PublishError::Operation {
            message: format!("failed to CAS-write {key}: {error}"),
        })?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cas_creates_then_rejects_stale_then_updates() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsPublishStorage::new(dir.path());

        // create (expected None)
        assert!(store
            .compare_and_swap("index/_head", None, b"v1")
            .await
            .unwrap());
        // stale expectation is rejected
        assert!(!store
            .compare_and_swap("index/_head", None, b"v2")
            .await
            .unwrap());
        // read back etag and CAS with it
        let object = store.read("index/_head").await.unwrap().unwrap();
        assert_eq!(object.bytes, b"v1");
        assert!(store
            .compare_and_swap("index/_head", Some(&object.etag), b"v2")
            .await
            .unwrap());
        assert_eq!(
            store.read("index/_head").await.unwrap().unwrap().bytes,
            b"v2"
        );
    }
}
