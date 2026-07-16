//! SQLite 本地 durability 后端的 schema 与连接初始化。每个子命令启动时都幂等
//! 建表；WAL journal 模式 + busy_timeout 让共享同一 `.db` 的 write/builder/query
//! 三个进程并发读、单写者提交而不互相阻塞报错。

use rusqlite::Connection;

/// 打开连接时统一设置的 busy_timeout（毫秒）：多进程共享卷时，写者提交期间
/// 其它连接等待而非立刻 `SQLITE_BUSY` 失败。
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// 幂等地初始化 pragma 与全部表。可对同一连接重复调用。
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    // journal_mode=WAL 返回结果行，必须用 query_row 读走，不能用 execute。
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS wal_segments (
            segment_key TEXT PRIMARY KEY,
            data        BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS build_jobs (
            batch_id     TEXT PRIMARY KEY,
            body         TEXT NOT NULL,
            state        TEXT NOT NULL,
            attempts     INTEGER NOT NULL DEFAULT 0,
            available_at INTEGER NOT NULL DEFAULT 0,
            claimed_at   INTEGER
         );
         CREATE TABLE IF NOT EXISTS dead_jobs (
            batch_id  TEXT PRIMARY KEY,
            body      TEXT NOT NULL,
            attempts  INTEGER NOT NULL,
            last_error TEXT,
            died_at   INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS active_head (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            head_bytes BLOB NOT NULL
         );",
    )?;
    Ok(())
}
