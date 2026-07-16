//! SQLite 版 WAL 存储：把 `WalStorage` 的字节契约落到 `wal_segments(segment_key, data)`。
//! `append` 对同一 `segment_key` 追加（读-拼接-写，即 append 而非覆盖语义）；`read` 读回
//! 整段字节。注意 `WalStorage` 是**字节级**契约——`append` 收到的是
//! 已序列化的 JSONL 字节（可能一批多条），因此这里按不透明字节存储，不解构成列，避免
//! 反序列化-再序列化带来的字节漂移。
//!
//! 追加的「读-拼接-写」必须在一个 `BEGIN IMMEDIATE` 事务内完成：多个写进程共享同一
//! `.db` 时，若读与写各自 autocommit，两个 append 可能读到同一旧值、后写者覆盖前者、
//! 丢失已接收的文档事件。IMMEDIATE 在事务开始即取写锁，配合 `busy_timeout` 让并发写者
//! 串行化（后者等待前者提交后再读到最新值），从而跨进程也不丢事件。

use async_trait::async_trait;
use rusqlite::{OptionalExtension, TransactionBehavior};

use super::SqliteDb;
use crate::error::IngestError;
use crate::write::WalStorage;

#[derive(Clone)]
pub struct SqliteWalStorage {
    db: SqliteDb,
}

impl SqliteWalStorage {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    /// 底层数据库句柄，供原子写路径校验 WAL 与队列是否共库。
    pub(super) fn db(&self) -> &SqliteDb {
        &self.db
    }

    /// 列出所有 WAL 段的 `segment_key`（升序），供 index-builder 的 worker 在每次
    /// 构建前取全量快照输入——对齐 AWS 侧 `ListObjectsV2(prefix="wal/")` 与文件型
    /// `list_local_wal_keys` 的语义。PR3 的组合根会把它包成 `ListWalKeysFn` 闭包。
    pub async fn list_wal_keys(&self) -> Result<Vec<String>, String> {
        self.db
            .call(|conn| {
                let mut stmt = conn
                    .prepare("SELECT segment_key FROM wal_segments ORDER BY segment_key")
                    .map_err(|e| format!("failed to prepare list_wal_keys: {e}"))?;
                let keys = stmt
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(|e| format!("failed to query wal_segments: {e}"))?
                    .collect::<Result<Vec<String>, _>>()
                    .map_err(|e| format!("failed to read wal_segments row: {e}"))?;
                Ok(keys)
            })
            .await
    }
}

pub(super) fn op_err(context: &str, error: rusqlite::Error) -> IngestError {
    IngestError::Operation {
        message: format!("{context}: {error}"),
    }
}

/// 在给定连接（可为事务）上对 `segment_key` 追加字节：读现值、Rust 侧拼接、整体 UPSERT
/// （避免 SQLite `||` 对 BLOB 的文本强转）。不管理事务边界——由调用方决定 autocommit
/// 还是并入更大的事务（原子写路径复用它，确保与 `SqliteWalStorage::append` 语义一致）。
pub(super) fn append_wal_segment(
    conn: &rusqlite::Connection,
    key: &str,
    bytes: &[u8],
) -> rusqlite::Result<()> {
    let existing: Option<Vec<u8>> = conn
        .query_row(
            "SELECT data FROM wal_segments WHERE segment_key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    let mut buf = existing.unwrap_or_default();
    buf.extend_from_slice(bytes);
    conn.execute(
        "INSERT INTO wal_segments (segment_key, data) VALUES (?1, ?2)
         ON CONFLICT(segment_key) DO UPDATE SET data = excluded.data",
        rusqlite::params![key, buf],
    )?;
    Ok(())
}

#[async_trait]
impl WalStorage for SqliteWalStorage {
    async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        let key = key.to_string();
        let bytes = bytes.to_vec();
        self.db
            .call(move |conn| {
                // 读-拼接-写在一个 IMMEDIATE 事务内原子完成，跨进程不丢事件（见模块注释）。
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(|e| op_err(&format!("failed to open WAL append tx for {key}"), e))?;
                append_wal_segment(&tx, &key, &bytes)
                    .map_err(|e| op_err(&format!("failed to append WAL {key}"), e))?;
                tx.commit()
                    .map_err(|e| op_err(&format!("failed to commit WAL append for {key}"), e))?;
                Ok(())
            })
            .await
    }

    async fn read(&self, key: &str) -> Result<Vec<u8>, IngestError> {
        let key = key.to_string();
        self.db
            .call(move |conn| {
                conn.query_row(
                    "SELECT data FROM wal_segments WHERE segment_key = ?1",
                    [&key],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(|e| op_err(&format!("failed to read WAL {key}"), e))?
                .ok_or_else(|| IngestError::Operation {
                    message: format!("failed to read WAL {key}: segment not found"),
                })
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_then_read_round_trips_bytes() {
        let (db, _dir) = SqliteDb::open_temp();
        let wal = SqliteWalStorage::new(db);
        let key = "wal/2026/07/14/batch-abc.jsonl";

        wal.append(key, b"{\"doc\":1}\n").await.unwrap();
        let bytes = wal.read(key).await.unwrap();

        assert_eq!(bytes, b"{\"doc\":1}\n");
    }

    #[tokio::test]
    async fn repeated_appends_to_same_key_accumulate() {
        let (db, _dir) = SqliteDb::open_temp();
        let wal = SqliteWalStorage::new(db);
        let key = "wal/2026/07/14/batch-abc.jsonl";

        wal.append(key, b"{\"doc\":1}\n").await.unwrap();
        wal.append(key, b"{\"doc\":2}\n").await.unwrap();
        let bytes = wal.read(key).await.unwrap();

        // 追加语义：第二条不得覆盖第一条（对齐 LocalFsWalStorage）。
        assert_eq!(bytes, b"{\"doc\":1}\n{\"doc\":2}\n");
    }

    #[tokio::test]
    async fn read_missing_segment_errors() {
        let (db, _dir) = SqliteDb::open_temp();
        let wal = SqliteWalStorage::new(db);
        assert!(wal.read("wal/nope.jsonl").await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cross_connection_appends_do_not_lose_events() {
        // 两个独立连接（模拟多进程）并发 append 同一段：IMMEDIATE 事务 + busy_timeout
        // 让「读-拼接-写」串行化，任何一方的写入都不得被覆盖丢失。
        let (db_a, db_b, _dir) = SqliteDb::open_two_temp();
        let wal_a = SqliteWalStorage::new(db_a);
        let wal_b = SqliteWalStorage::new(db_b);
        let key = "wal/2026/07/15/shared.jsonl";
        const N: usize = 25;

        let ta = {
            let wal_a = wal_a.clone();
            tokio::spawn(async move {
                for i in 0..N {
                    wal_a
                        .append(key, format!("{{\"a\":{i}}}\n").as_bytes())
                        .await
                        .unwrap();
                }
            })
        };
        let tb = tokio::spawn(async move {
            for i in 0..N {
                wal_b
                    .append(key, format!("{{\"b\":{i}}}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        ta.await.unwrap();
        tb.await.unwrap();

        let bytes = wal_a.read(key).await.unwrap();
        let lines = bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .count();
        assert_eq!(lines, 2 * N, "并发追加不得丢事件");
    }

    #[tokio::test]
    async fn list_wal_keys_returns_distinct_sorted_segments() {
        let (db, _dir) = SqliteDb::open_temp();
        let wal = SqliteWalStorage::new(db);
        wal.append("wal/2026/07/15/b.jsonl", b"{}\n").await.unwrap();
        wal.append("wal/2026/07/15/a.jsonl", b"{}\n").await.unwrap();
        // Re-appending the same segment must not produce a duplicate key.
        wal.append("wal/2026/07/15/b.jsonl", b"{}\n").await.unwrap();

        let keys = wal.list_wal_keys().await.unwrap();

        assert_eq!(
            keys,
            vec![
                "wal/2026/07/15/a.jsonl".to_string(),
                "wal/2026/07/15/b.jsonl".to_string(),
            ]
        );
    }
}
