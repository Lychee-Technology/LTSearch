//! SQLite 本地 durability 后端（#108）。一个 `<LTSEARCH_LOCAL_ROOT>/ltsearch.db`
//! 同时承载耐久文档事件日志、构建作业队列与活跃发布指针，替换 #116 落地的文件型
//! `LocalFs*` durability 实现，但仍站在同一批供应商中立契约之后（`WalStorage` /
//! `BuildQueue`+`BuildJobSource` / `PublishStorage` 的 head-CAS / `ManifestStore`）。
//!
//! 所有契约实现共享同一个 [`SqliteDb`]（内部 `Arc<Mutex<Connection>>`），因此写路径
//! 的「事件写入 + 作业入队」可以在 #123 中合入同一事务。rusqlite 是阻塞 API，统一经
//! [`SqliteDb::call`] 在 `spawn_blocking` 中持锁执行。

mod head;
mod manifest;
mod queue;
mod schema;
mod static_release;
mod wal;

pub use head::LocalPublishStorage;
pub use manifest::SqliteManifestStore;
pub use queue::{SqliteBuildJobSource, SqliteBuildQueue};
pub use static_release::SqliteStaticReleaseStore;
pub use wal::SqliteWalStorage;

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// busy/locked 判定：并发初始化的可重试错误码。
fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::DatabaseBusy
                || failure.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

/// 共享的 SQLite 连接句柄。`Clone` 后所有副本共用同一把锁与连接，满足
/// `WalStorage`/`BuildQueue`/`PublishStorage` 的 `Clone + Send + Sync + 'static`
/// 约束，并让多个契约实现落到同一事务边界内。
#[derive(Clone)]
pub struct SqliteDb {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDb {
    /// 打开（或创建）`path` 处的数据库，设置 WAL 模式并幂等建表。
    ///
    /// 首启竞态：三个角色进程同时启动、同时切 WAL 模式/建表时，`journal_mode=WAL`
    /// 需要独占访问，可能绕过 busy_timeout 直接返回 busy/locked。`init` 幂等，
    /// 因此这里做有界重试（100ms × 50 ≈ 5s），后到者等先到者初始化完成。
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        let mut attempts = 0;
        loop {
            match schema::init(&conn) {
                Ok(()) => break,
                Err(error) if is_busy(&error) && attempts < 50 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 两个句柄是否指向同一个共享连接（同一 `Arc`）。原子写路径用它强制
    /// 「WAL 与队列共库」不变式：只有共用一个 `SqliteDb` 的构造才能在单事务内落库。
    pub fn ptr_eq(&self, other: &SqliteDb) -> bool {
        Arc::ptr_eq(&self.conn, &other.conn)
    }

    /// 在 `spawn_blocking` 中持锁执行一段阻塞的 rusqlite 逻辑。集中封装
    /// 「clone Arc → spawn_blocking → lock → 运行闭包」这一模式，让契约实现的
    /// async 方法体保持简洁。
    pub(crate) async fn call<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut Connection) -> T + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|poison| poison.into_inner());
            f(&mut guard)
        })
        .await
        .expect("sqlite blocking task panicked")
    }

    /// 同步持锁执行一段 rusqlite 逻辑（不经 `spawn_blocking`）。供必须同步返回的
    /// [`crate::storage::ManifestStore`] 实现使用——那是同步 trait，且只做单行小查询，
    /// 短暂持锁在本地单用户场景可接受。
    pub(crate) fn with_conn_blocking<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&Connection) -> T,
    {
        let guard = self
            .conn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        f(&guard)
    }

    /// 测试辅助：在临时目录建库，返回 `(db, tempdir)`；tempdir 需保活到用完。
    #[cfg(test)]
    pub(crate) fn open_temp() -> (Self, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Self::open(dir.path().join("ltsearch.db")).unwrap();
        (db, dir)
    }

    /// 测试辅助：对同一个 `.db` 文件打开两个**独立连接**（各自的 `Arc<Mutex>`），
    /// 模拟多进程共享卷场景，用于跨连接并发（WAL append / head CAS）竞争测试。
    #[cfg(test)]
    pub(crate) fn open_two_temp() -> (Self, Self, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ltsearch.db");
        let a = Self::open(&path).unwrap();
        let b = Self::open(&path).unwrap();
        (a, b, dir)
    }
}
