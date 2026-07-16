//! 文件系统制品存储：把 `key` 当作 `root` 下相对路径。etag 用内容 FNV-1a 哈希
//! 的十六进制。`compare_and_swap` 必须是原子的（`PublishStorage` 契约，见
//! `src/indexing/publisher.rs`）：本地 index-builder 会有队列 worker 与
//! `POST /build` 处理器并发对 `index/_head` 做 CAS，读-比较-写若不互斥则两个
//! contender 可能都看到同一 etag、都判定通过、后写者静默覆盖，从而激活相互竞争
//! 的版本。这里用一把进程内锁把整个读-比较-写串起来，与 S3 条件写的原子语义
//! 同构，让 `next_version_id` / `IndexPublisher` 的发布路径无需改动即可跑在本地。
//! 作用域为单节点本地进程；跨进程共享同一卷不在本地部署模型内。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::PublishError;
use crate::indexing::{PublishStorage, UploadMode, VersionedObject};

#[derive(Debug, Clone)]
pub struct LocalFsPublishStorage {
    root: PathBuf,
    /// 串行化 `compare_and_swap` 的读-比较-写临界区；跨 clone 共享同一把锁。
    cas_lock: Arc<Mutex<()>>,
}

impl LocalFsPublishStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cas_lock: Arc::new(Mutex::new(())),
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

pub(crate) fn etag_of(bytes: &[u8]) -> String {
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
        // 持锁跨整个读-比较-写；临界区内无 `.await`，future 仍是 Send。
        let _guard = self
            .cas_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cas_lets_exactly_one_contender_win() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsPublishStorage::new(dir.path());

        // Seed a value and capture its etag; every contender races on this etag.
        // The seed content ("seed") is distinct from every contender value so the
        // first write genuinely changes the etag — otherwise a contender writing
        // byte-identical content would leave the etag at the seed value and also
        // pass (correct content-etag CAS semantics, but not what this test probes).
        assert!(store
            .compare_and_swap("index/_head", None, b"seed")
            .await
            .unwrap());
        let seed_etag = store.read("index/_head").await.unwrap().unwrap().etag;

        let mut handles = Vec::new();
        for i in 0..8u32 {
            let store = store.clone();
            let etag = seed_etag.clone();
            handles.push(tokio::spawn(async move {
                store
                    .compare_and_swap("index/_head", Some(&etag), format!("v{i}").as_bytes())
                    .await
                    .unwrap()
            }));
        }

        let mut winners = 0;
        for handle in handles {
            if handle.await.unwrap() {
                winners += 1;
            }
        }

        // Atomic CAS: only the first contender sees the seed etag; the write
        // changes it, so every later contender must observe a mismatch.
        assert_eq!(winners, 1);
    }
}
