//! 文件系统 WAL：把 `key`（形如 `wal/2026/07/14/batch-<uuid>.jsonl`）当作
//! `root` 下的相对路径落盘。本地单进程场景不需要 S3 的条件写；append 直接创建
//! 父目录并写文件，read 读回。

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::IngestError;
use crate::write::WalStorage;

#[derive(Debug, Clone)]
pub struct LocalFsWalStorage {
    root: PathBuf,
}

impl LocalFsWalStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl WalStorage for LocalFsWalStorage {
    async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| IngestError::Operation {
                    message: format!("failed to create WAL dir for {key}: {error}"),
                })?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to write WAL {key}: {error}"),
            })
    }

    async fn read(&self, key: &str) -> Result<Vec<u8>, IngestError> {
        tokio::fs::read(self.path_for(key))
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to read WAL {key}: {error}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_then_read_round_trips_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let wal = LocalFsWalStorage::new(dir.path());
        let key = "wal/2026/07/14/batch-abc.jsonl";

        wal.append(key, b"{\"doc\":1}\n").await.unwrap();
        let bytes = wal.read(key).await.unwrap();

        assert_eq!(bytes, b"{\"doc\":1}\n");
    }
}
