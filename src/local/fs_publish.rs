//! 文件系统制品存储：把 `key` 当作 `root` 下相对路径。etag 用内容 FNV-1a 哈希
//! 的十六进制。`compare_and_swap` 必须是原子的（`PublishStorage` 契约，见
//! `src/indexing/publisher.rs`）；这里用一把进程内锁把整个读-比较-写串起来，
//! 与 S3 条件写的原子语义同构。作用域为单节点本地进程内的非 head 键。
//!
//! **`index/_head` 在此适配器中已退役**（#123）：活跃版本指针唯一活在 SQLite 的
//! `active_head` 行（见 `sqlite::head::LocalPublishStorage`，它拦截该 key 并把其余
//! key 委托到这里）。为使退役成立于代码而非仅组合根接线，本适配器对 `index/_head`
//! 的 `read`/`compare_and_swap` 一律报错拒绝。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::PublishError;
use crate::indexing::{PublishStorage, UploadMode, VersionedObject};
use crate::storage::{INDEX_HEAD_KEY, STATIC_HEAD_KEY};

/// 指针 key 的文件系统路径已退役：活跃版本指针 `index/_head`（#123）与静态发布
/// 指针 `static/_head`（#112）唯一活在 SQLite，只经 `sqlite::LocalPublishStorage`
/// 的 CAS 变更。任何经文件适配器触达这些 key（upload/read/CAS）都是接线错误，报错
/// 而非静默落盘或服务——否则指针可能绕过 CAS 被 upload 直接写盘。
///
/// 精确相等匹配：只拦这两个指针 key 本身，不误伤 `static/releases/<id>/…` 这类
/// 以 `static/` 起头的不可变制品字节。
fn reject_head_key(key: &str, operation: &str) -> Result<(), PublishError> {
    if key == INDEX_HEAD_KEY || key == STATIC_HEAD_KEY {
        return Err(PublishError::Operation {
            message: format!(
                "filesystem {operation} of {key} is retired: the pointer lives in SQLite; \
                 route through sqlite::LocalPublishStorage"
            ),
        });
    }
    Ok(())
}

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
        reject_head_key(key, "upload_directory")?;
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
        reject_head_key(key, "upload_file")?;
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
        reject_head_key(key, "read")?;
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
        reject_head_key(key, "compare_and_swap")?;
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
    use crate::storage::{static_release_dir_key, STATIC_HEAD_KEY};

    // 非 head 键的通用 CAS 语义（head 已退役到 SQLite，此处用制品类键验证）。
    const CAS_KEY: &str = "index/versions/1/manifest.json";

    #[tokio::test]
    async fn cas_creates_then_rejects_stale_then_updates() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsPublishStorage::new(dir.path());

        // create (expected None)
        assert!(store.compare_and_swap(CAS_KEY, None, b"v1").await.unwrap());
        // stale expectation is rejected
        assert!(!store.compare_and_swap(CAS_KEY, None, b"v2").await.unwrap());
        // read back etag and CAS with it
        let object = store.read(CAS_KEY).await.unwrap().unwrap();
        assert_eq!(object.bytes, b"v1");
        assert!(store
            .compare_and_swap(CAS_KEY, Some(&object.etag), b"v2")
            .await
            .unwrap());
        assert_eq!(store.read(CAS_KEY).await.unwrap().unwrap().bytes, b"v2");
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
            .compare_and_swap(CAS_KEY, None, b"seed")
            .await
            .unwrap());
        let seed_etag = store.read(CAS_KEY).await.unwrap().unwrap().etag;

        let mut handles = Vec::new();
        for i in 0..8u32 {
            let store = store.clone();
            let etag = seed_etag.clone();
            handles.push(tokio::spawn(async move {
                store
                    .compare_and_swap(CAS_KEY, Some(&etag), format!("v{i}").as_bytes())
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

    // #123 退役断言：文件系统适配器对 index/_head 的 read / CAS 一律报错拒绝，
    // 活跃指针唯一活在 SQLite（sqlite::LocalPublishStorage 拦截该 key）。
    #[tokio::test]
    async fn head_key_is_retired_and_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsPublishStorage::new(dir.path());

        let read_err = store.read(INDEX_HEAD_KEY).await.unwrap_err().to_string();
        assert!(read_err.contains("retired"), "got: {read_err}");

        let cas_err = store
            .compare_and_swap(INDEX_HEAD_KEY, None, b"v1")
            .await
            .unwrap_err()
            .to_string();
        assert!(cas_err.contains("retired"), "got: {cas_err}");
        // 拒绝必须发生在写盘之前：不得留下 _head 文件。
        assert!(!dir.path().join(INDEX_HEAD_KEY).exists());
    }

    // #112 携带项①：static/_head 指针同样唯一活在 SQLite。文件适配器对该 key 的
    // upload_file / upload_directory / read / compare_and_swap 一律报错拒绝，堵住
    // 「指针经目录/文件上传绕过 CAS 落盘」的口子。
    #[tokio::test]
    async fn static_head_key_upload_file_is_rejected_before_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsPublishStorage::new(dir.path());

        let source = dir.path().join("payload");
        std::fs::write(&source, b"pointer bytes").unwrap();

        let err = store
            .upload_file(STATIC_HEAD_KEY, &source, UploadMode::Overwrite)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("retired"), "got: {err}");
        // 拒绝必须发生在写盘之前：不得留下 static/_head 文件。
        assert!(!dir.path().join(STATIC_HEAD_KEY).exists());
    }

    #[tokio::test]
    async fn static_head_key_read_and_cas_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsPublishStorage::new(dir.path());

        let read_err = store.read(STATIC_HEAD_KEY).await.unwrap_err().to_string();
        assert!(read_err.contains("retired"), "got: {read_err}");

        let cas_err = store
            .compare_and_swap(STATIC_HEAD_KEY, None, b"ptr")
            .await
            .unwrap_err()
            .to_string();
        assert!(cas_err.contains("retired"), "got: {cas_err}");
        assert!(!dir.path().join(STATIC_HEAD_KEY).exists());
    }

    // 精确相等匹配不得误伤 release 制品目录（static/releases/<id>）：这些是不可变
    // 制品字节，照常走文件系统。
    #[tokio::test]
    async fn static_release_dir_upload_is_not_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsPublishStorage::new(dir.path());

        let source = dir.path().join("release-src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("release_manifest.json"), b"{}").unwrap();

        let key = static_release_dir_key(&"a".repeat(64));
        store
            .upload_directory(&key, &source, UploadMode::CreateOnly)
            .await
            .expect("release artifact directory must upload normally");
        assert!(dir.path().join(&key).join("release_manifest.json").exists());
    }
}
