//! 文件系统 WAL：把 `key`（形如 `wal/2026/07/14/batch-<uuid>.jsonl`）当作
//! `root` 下的相对路径落盘。`WalStorage::append` 的契约是**追加**语义——
//! `WriteAheadLog` 会对同一 segment key 反复 append 每条 JSONL 记录（见
//! `AwsS3WalStorage` 的读-拼接-写实现），因此这里必须以 append 模式打开而非
//! 覆盖写，否则同一段的后续记录会丢掉先前记录。read 读回整段。

use std::path::PathBuf;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

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
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to open WAL {key}: {error}"),
            })?;
        file.write_all(bytes)
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to append WAL {key}: {error}"),
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

    #[tokio::test]
    async fn repeated_appends_to_same_key_accumulate() {
        let dir = tempfile::tempdir().unwrap();
        let wal = LocalFsWalStorage::new(dir.path());
        let key = "wal/2026/07/14/batch-abc.jsonl";

        wal.append(key, b"{\"doc\":1}\n").await.unwrap();
        wal.append(key, b"{\"doc\":2}\n").await.unwrap();
        let bytes = wal.read(key).await.unwrap();

        // 追加语义：第二条不得覆盖第一条。
        assert_eq!(bytes, b"{\"doc\":1}\n{\"doc\":2}\n");
    }
}
