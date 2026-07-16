//! 混合发布存储：活跃版本指针 `index/_head` 的 read/CAS 落到 SQLite 的 `active_head`
//! 单行，其余 key（lance/tantivy/manifest 制品字节）仍走文件系统。这样 head 的比较-
//! 交换（CAS）借 SQLite 的事务原子性实现——本地 index-builder 的队列 worker 与
//! `POST /build` 处理器会并发对 head 做 CAS，`active_head` 行上的「读现值-比对 etag-
//! 条件写」在单连接单事务内不可分割——而大体量不可变制品继续按文件读写，避免灌入 DB。
//!
//! etag 沿用 `LocalFsPublishStorage` 的内容哈希方案（FNV-1a），且直接存原始 head 字节、
//! 在 read 时现算 etag，保证 publisher「read 拿 etag → 带 etag CAS」的一轮字节稳定。

use std::path::Path;

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use super::SqliteDb;
use crate::error::PublishError;
use crate::indexing::{PublishStorage, UploadMode, VersionedObject};
use crate::local::fs_publish::etag_of;
use crate::local::LocalFsPublishStorage;
use crate::storage::INDEX_HEAD_KEY;

/// 本地混合发布存储：head 走 SQLite，制品字节走文件系统。
#[derive(Clone)]
pub struct LocalPublishStorage {
    db: SqliteDb,
    fs: LocalFsPublishStorage,
}

impl LocalPublishStorage {
    pub fn new(db: SqliteDb, root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            db,
            fs: LocalFsPublishStorage::new(root),
        }
    }

    fn cas_err(context: &'static str, error: rusqlite::Error) -> PublishError {
        PublishError::Operation {
            message: format!("{context}: {error}"),
        }
    }
}

#[async_trait]
impl PublishStorage for LocalPublishStorage {
    async fn upload_directory(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
        // head 从不以目录/文件形式上传，只经 compare_and_swap 变更。
        self.fs.upload_directory(key, source, mode).await
    }

    async fn upload_file(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
        self.fs.upload_file(key, source, mode).await
    }

    async fn read(&self, key: &str) -> Result<Option<VersionedObject>, PublishError> {
        if key != INDEX_HEAD_KEY {
            return self.fs.read(key).await;
        }
        self.db
            .call(|conn| {
                let bytes: Option<Vec<u8>> = conn
                    .query_row(
                        "SELECT head_bytes FROM active_head WHERE id = 1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| Self::cas_err("failed to read active head", e))?;
                Ok(bytes.map(|bytes| VersionedObject {
                    etag: etag_of(&bytes),
                    bytes,
                }))
            })
            .await
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_etag: Option<&str>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        if key != INDEX_HEAD_KEY {
            return self
                .fs
                .compare_and_swap(key, expected_etag, new_value)
                .await;
        }
        let expected = expected_etag.map(|s| s.to_string());
        let new_value = new_value.to_vec();
        self.db
            .call(move |conn| {
                let tx = conn
                    .transaction()
                    .map_err(|e| Self::cas_err("failed to open head CAS tx", e))?;
                // 读现值算 etag，与期望比对：不符即拒绝（Ok(false)）。
                let current: Option<Vec<u8>> = tx
                    .query_row(
                        "SELECT head_bytes FROM active_head WHERE id = 1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| Self::cas_err("failed to read head for CAS", e))?;
                let current_etag = current.as_deref().map(etag_of);
                if current_etag.as_deref() != expected.as_deref() {
                    return Ok(false);
                }
                tx.execute(
                    "INSERT INTO active_head (id, head_bytes) VALUES (1, ?1)
                     ON CONFLICT(id) DO UPDATE SET head_bytes = excluded.head_bytes",
                    rusqlite::params![new_value],
                )
                .map_err(|e| Self::cas_err("failed to write head CAS", e))?;
                tx.commit()
                    .map_err(|e| Self::cas_err("failed to commit head CAS", e))?;
                Ok(true)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (LocalPublishStorage, tempfile::TempDir) {
        let (db, dir) = SqliteDb::open_temp();
        let store = LocalPublishStorage::new(db, dir.path());
        (store, dir)
    }

    #[tokio::test]
    async fn head_cas_creates_then_rejects_stale_then_updates() {
        let (store, _dir) = store();

        // create (expected None)
        assert!(store
            .compare_and_swap(INDEX_HEAD_KEY, None, b"v1")
            .await
            .unwrap());
        // stale expectation rejected
        assert!(!store
            .compare_and_swap(INDEX_HEAD_KEY, None, b"v2")
            .await
            .unwrap());
        // read back etag and CAS with it
        let object = store.read(INDEX_HEAD_KEY).await.unwrap().unwrap();
        assert_eq!(object.bytes, b"v1");
        assert!(store
            .compare_and_swap(INDEX_HEAD_KEY, Some(&object.etag), b"v2")
            .await
            .unwrap());
        assert_eq!(
            store.read(INDEX_HEAD_KEY).await.unwrap().unwrap().bytes,
            b"v2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_head_cas_lets_exactly_one_contender_win() {
        let (store, _dir) = store();
        assert!(store
            .compare_and_swap(INDEX_HEAD_KEY, None, b"seed")
            .await
            .unwrap());
        let seed_etag = store.read(INDEX_HEAD_KEY).await.unwrap().unwrap().etag;

        let mut handles = Vec::new();
        for i in 0..8u32 {
            let store = store.clone();
            let etag = seed_etag.clone();
            handles.push(tokio::spawn(async move {
                store
                    .compare_and_swap(INDEX_HEAD_KEY, Some(&etag), format!("v{i}").as_bytes())
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
        assert_eq!(winners, 1);
    }

    #[tokio::test]
    async fn non_head_keys_route_to_filesystem() {
        let (store, dir) = store();
        // A non-head CAS writes to the filesystem, not active_head.
        assert!(store
            .compare_and_swap("index/v1/manifest.json", None, b"{}")
            .await
            .unwrap());
        assert!(dir.path().join("index/v1/manifest.json").exists());
        // active_head remains empty.
        assert!(store.read(INDEX_HEAD_KEY).await.unwrap().is_none());
    }
}
