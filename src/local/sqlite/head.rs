//! 混合发布存储：指针 key 的 read/CAS 落到 SQLite 的单行表，其余 key（lance/tantivy/
//! manifest 制品字节）仍走文件系统。活跃版本指针 `index/_head` 路由到 `active_head`、
//! 静态发布指针 `static/_head` 路由到独立的 `static_release_head` 表——二者同机制不同表，
//! 互不干扰。这样 head 的比较-交换（CAS）借 SQLite 的事务原子性实现：本地 index-builder
//! 的队列 worker 与 `POST /build` 处理器会并发对同一 head 做 CAS，单行上的「读现值-比对
//! etag-条件写」在单连接单事务内不可分割，等价于 S3 的条件写（If-Match）语义——而大体量
//! 不可变制品继续按文件读写，避免灌入 DB。
//!
//! etag 沿用 `LocalFsPublishStorage` 的内容哈希方案（FNV-1a），且直接存原始 head 字节、
//! 在 read 时现算 etag，保证 publisher「read 拿 etag → 带 etag CAS」的一轮字节稳定。

use std::path::Path;

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::SqliteDb;
use crate::error::PublishError;
use crate::indexing::{PublishStorage, UploadMode, VersionedObject};
use crate::local::fs_publish::etag_of;
use crate::local::LocalFsPublishStorage;
use crate::storage::{INDEX_HEAD_KEY, STATIC_HEAD_KEY};

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

    /// 将指针 key 映射到其 SQLite 单行表名；非指针 key 返回 `None`（走文件系统）。
    /// 表名是本函数内的常量字面量，绝不来自外部输入——下游 SQL 拼接因此安全。
    fn head_table(key: &str) -> Option<&'static str> {
        match key {
            INDEX_HEAD_KEY => Some("active_head"),
            STATIC_HEAD_KEY => Some("static_release_head"),
            _ => None,
        }
    }

    /// 读指定 head 表的单行现值（不存在则 `None`）。
    fn head_row_read(conn: &Connection, table: &str) -> Result<Option<Vec<u8>>, PublishError> {
        conn.query_row(
            &format!("SELECT head_bytes FROM {table} WHERE id = 1"),
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| Self::cas_err("failed to read head", e))
    }

    /// 对指定 head 表的单行做条件写：IMMEDIATE 事务内读现值算 etag、与 `expected` 比对，
    /// 不符即拒绝（`Ok(false)`），相符则写入并提交（`Ok(true)`）。
    fn head_row_cas(
        conn: &mut Connection,
        table: &str,
        expected: Option<&str>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        // IMMEDIATE：事务开始即取写锁，跨进程 CAS 被串行化。否则两个独立连接可能
        // 读到同一快照，后提交者在写升级时收到 SQLITE_BUSY_SNAPSHOT——那是可重试的
        // 忙错误，而非语义上的「etag 不符」。IMMEDIATE 让后者等前者提交、读到新值、
        // etag 比对失败而正确返回 Ok(false)。
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Self::cas_err("failed to open head CAS tx", e))?;
        // 读现值算 etag，与期望比对：不符即拒绝（Ok(false)）。
        let current = Self::head_row_read(&tx, table)?;
        let current_etag = current.as_deref().map(etag_of);
        if current_etag.as_deref() != expected {
            return Ok(false);
        }
        tx.execute(
            &format!(
                "INSERT INTO {table} (id, head_bytes) VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET head_bytes = excluded.head_bytes"
            ),
            rusqlite::params![new_value],
        )
        .map_err(|e| Self::cas_err("failed to write head CAS", e))?;
        tx.commit()
            .map_err(|e| Self::cas_err("failed to commit head CAS", e))?;
        Ok(true)
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
        let Some(table) = Self::head_table(key) else {
            return self.fs.read(key).await;
        };
        self.db
            .call(move |conn| {
                let bytes = Self::head_row_read(conn, table)?;
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
        let Some(table) = Self::head_table(key) else {
            return self
                .fs
                .compare_and_swap(key, expected_etag, new_value)
                .await;
        };
        let expected = expected_etag.map(|s| s.to_string());
        let new_value = new_value.to_vec();
        self.db
            .call(move |conn| Self::head_row_cas(conn, table, expected.as_deref(), &new_value))
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cross_connection_cas_yields_conflict_not_busy_error() {
        // 两个独立连接（模拟多进程）对同一 seed etag 并发 CAS：恰好一个 Ok(true)，
        // 另一个必须是 Ok(false)（etag 冲突），而不是 SQLITE_BUSY_SNAPSHOT → PublishError。
        let (db_a, db_b, dir) = SqliteDb::open_two_temp();
        let store_a = LocalPublishStorage::new(db_a, dir.path());
        let store_b = LocalPublishStorage::new(db_b, dir.path());
        assert!(store_a
            .compare_and_swap(INDEX_HEAD_KEY, None, b"seed")
            .await
            .unwrap());
        let seed_etag = store_a.read(INDEX_HEAD_KEY).await.unwrap().unwrap().etag;

        let ea = seed_etag.clone();
        let a = tokio::spawn(async move {
            store_a
                .compare_and_swap(INDEX_HEAD_KEY, Some(&ea), b"va")
                .await
        });
        let eb = seed_etag.clone();
        let b = tokio::spawn(async move {
            store_b
                .compare_and_swap(INDEX_HEAD_KEY, Some(&eb), b"vb")
                .await
        });
        // 两侧都不得返回 Err（忙错误被 IMMEDIATE 消化成串行化）。
        let ra = a.await.unwrap().expect("CAS must not surface a busy error");
        let rb = b.await.unwrap().expect("CAS must not surface a busy error");
        assert_eq!([ra, rb].iter().filter(|w| **w).count(), 1, "恰好一个胜出");
    }

    #[tokio::test]
    async fn static_head_cas_is_independent_from_index_head() {
        let (store, _dir) = store();
        // 两个 key 各自 CAS 创建，互不干扰。
        assert!(store
            .compare_and_swap(INDEX_HEAD_KEY, None, b"idx")
            .await
            .unwrap());
        assert!(store
            .compare_and_swap(STATIC_HEAD_KEY, None, b"stat")
            .await
            .unwrap());
        assert_eq!(
            store.read(STATIC_HEAD_KEY).await.unwrap().unwrap().bytes,
            b"stat"
        );
        assert_eq!(
            store.read(INDEX_HEAD_KEY).await.unwrap().unwrap().bytes,
            b"idx"
        );
        // 陈旧 expectation（None 但静态指针已存在）→ lost CAS，且不动 active_head。
        assert!(!store
            .compare_and_swap(STATIC_HEAD_KEY, None, b"stat2")
            .await
            .unwrap());
        assert_eq!(
            store.read(STATIC_HEAD_KEY).await.unwrap().unwrap().bytes,
            b"stat"
        );
        assert_eq!(
            store.read(INDEX_HEAD_KEY).await.unwrap().unwrap().bytes,
            b"idx"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_static_head_cas_lets_exactly_one_win() {
        let (store, _dir) = store();
        assert!(store
            .compare_and_swap(STATIC_HEAD_KEY, None, b"seed")
            .await
            .unwrap());
        let etag = store.read(STATIC_HEAD_KEY).await.unwrap().unwrap().etag;

        let mut handles = Vec::new();
        for i in 0..8u32 {
            let store = store.clone();
            let etag = etag.clone();
            handles.push(tokio::spawn(async move {
                store
                    .compare_and_swap(STATIC_HEAD_KEY, Some(&etag), format!("v{i}").as_bytes())
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cross_connection_static_cas_yields_conflict_not_busy_error() {
        // 两个独立连接（模拟多进程）对同一 seed etag 并发 CAS 静态指针：恰好一个 Ok(true)，
        // 另一个必须是 Ok(false)（etag 冲突），而不是 SQLITE_BUSY_SNAPSHOT → PublishError。
        let (db_a, db_b, dir) = SqliteDb::open_two_temp();
        let store_a = LocalPublishStorage::new(db_a, dir.path());
        let store_b = LocalPublishStorage::new(db_b, dir.path());
        assert!(store_a
            .compare_and_swap(STATIC_HEAD_KEY, None, b"seed")
            .await
            .unwrap());
        let seed_etag = store_a.read(STATIC_HEAD_KEY).await.unwrap().unwrap().etag;

        let ea = seed_etag.clone();
        let a = tokio::spawn(async move {
            store_a
                .compare_and_swap(STATIC_HEAD_KEY, Some(&ea), b"va")
                .await
        });
        let eb = seed_etag.clone();
        let b = tokio::spawn(async move {
            store_b
                .compare_and_swap(STATIC_HEAD_KEY, Some(&eb), b"vb")
                .await
        });
        // 两侧都不得返回 Err（忙错误被 IMMEDIATE 消化成串行化）。
        let ra = a.await.unwrap().expect("CAS must not surface a busy error");
        let rb = b.await.unwrap().expect("CAS must not surface a busy error");
        assert_eq!([ra, rb].iter().filter(|w| **w).count(), 1, "恰好一个胜出");
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
