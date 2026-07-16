//! SQLite 版构建作业队列。生产侧 [`SqliteBuildQueue`]（`enqueue`）与消费侧
//! [`SqliteBuildJobSource`]（`receive`/`ack`/`nack`）拆成两个结构：本地部署里 write
//! 进程与 index-builder 进程分别构造它们，只共享同一张 `build_jobs` 表。二者都持有
//! 同一个 [`SqliteDb`]，因此 #123 的写路径可让 WAL 追加与作业入队落到同一事务。
//!
//! 队列语义：`receive` 用 `UPDATE … RETURNING` 原子领取（claim）就绪作业，并把租约
//! 过期（claimed 超过 `lease_ms`）的作业回收为 ready；`ack` 删除；`nack` 做重试退避
//! 与死信（达到 `max_attempts` 移入 `dead_jobs`）。领取/租约/退避都基于可注入的时钟，
//! 便于确定性单测。

use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use super::SqliteDb;
use crate::contracts::{BuildJob, BuildJobSource};
use crate::error::IngestError;
use crate::write::{BuildQueue, QueueBatch};

/// 默认最大尝试次数（含首次）。达到后 `nack` 把作业移入 `dead_jobs`。
pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// 默认可见性租约：领取后多久未 ack/nack 即视为 worker 失联，作业被回收重投。
pub(crate) const DEFAULT_LEASE_MS: i64 = 300_000;
/// 默认退避基数：`nack` 重投时 `available_at = now + base * attempts`。
pub(crate) const DEFAULT_BACKOFF_MS: i64 = 1_000;

/// 单调毫秒时钟；生产用系统时间，测试注入可控时钟以驱动租约/退避。
pub(crate) type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub(crate) fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// 从 `LTSEARCH_BUILD_MAX_ATTEMPTS` 读取上限，缺失/非法回落到默认值。
pub(crate) fn max_attempts_from_env() -> u32 {
    std::env::var("LTSEARCH_BUILD_MAX_ATTEMPTS")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value >= 1)
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

#[derive(Clone)]
pub struct SqliteBuildQueue {
    db: SqliteDb,
}

impl SqliteBuildQueue {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }
}

#[async_trait]
impl BuildQueue for SqliteBuildQueue {
    async fn enqueue(&self, batch: QueueBatch) -> Result<(), IngestError> {
        let batch_id = batch.batch_id.clone();
        let body = serde_json::to_string(&batch).map_err(|error| IngestError::Operation {
            message: format!("failed to encode queue batch: {error}"),
        })?;
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO build_jobs (batch_id, body, state, attempts, available_at, claimed_at)
                     VALUES (?1, ?2, 'ready', 0, 0, NULL)
                     ON CONFLICT(batch_id) DO UPDATE SET
                        body = excluded.body, state = 'ready', attempts = 0,
                        available_at = 0, claimed_at = NULL",
                    rusqlite::params![batch_id, body],
                )
                .map(|_| ())
                .map_err(|error| IngestError::Operation {
                    message: format!("failed to enqueue build job {batch_id}: {error}"),
                })
            })
            .await
    }
}

#[derive(Clone)]
pub struct SqliteBuildJobSource {
    pub(crate) db: SqliteDb,
    pub(crate) clock: Clock,
    pub(crate) lease_ms: i64,
    pub(crate) max_attempts: u32,
    pub(crate) base_backoff_ms: i64,
}

impl SqliteBuildJobSource {
    pub fn new(db: SqliteDb) -> Self {
        Self {
            db,
            clock: system_clock(),
            lease_ms: DEFAULT_LEASE_MS,
            max_attempts: max_attempts_from_env(),
            base_backoff_ms: DEFAULT_BACKOFF_MS,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(db: SqliteDb, clock: Clock) -> Self {
        Self {
            db,
            clock,
            lease_ms: DEFAULT_LEASE_MS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            base_backoff_ms: DEFAULT_BACKOFF_MS,
        }
    }

    pub(crate) fn now(&self) -> i64 {
        (self.clock)()
    }
}

#[async_trait]
impl BuildJobSource for SqliteBuildJobSource {
    async fn receive(&self) -> Result<Vec<BuildJob>, String> {
        let now = self.now();
        let lease_ms = self.lease_ms;
        self.db
            .call(move |conn| {
                let tx = conn
                    .transaction()
                    .map_err(|e| format!("failed to open receive tx: {e}"))?;
                // 回收租约过期的 claimed 作业：worker 领取后 lease_ms 内未 ack/nack，
                // 视为失联，重新变为 ready 以便被再次领取（崩溃/重启安全）。
                tx.execute(
                    "UPDATE build_jobs SET state = 'ready', claimed_at = NULL
                     WHERE state = 'claimed' AND claimed_at IS NOT NULL AND claimed_at <= ?1",
                    [now - lease_ms],
                )
                .map_err(|e| format!("failed to reclaim expired leases: {e}"))?;
                // 原子领取全部就绪作业：UPDATE … RETURNING 让「判定 + 置 claimed」不可分割。
                let jobs = {
                    let mut stmt = tx
                        .prepare(
                            "UPDATE build_jobs SET state = 'claimed', claimed_at = ?1
                             WHERE state = 'ready' AND available_at <= ?1
                             RETURNING batch_id, body",
                        )
                        .map_err(|e| format!("failed to prepare claim: {e}"))?;
                    let rows = stmt
                        .query_map([now], |row| {
                            Ok(BuildJob {
                                receipt: row.get::<_, String>(0)?,
                                body: row.get::<_, String>(1)?,
                            })
                        })
                        .map_err(|e| format!("failed to claim build jobs: {e}"))?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| format!("failed to read claimed job: {e}"))?;
                    rows
                };
                tx.commit()
                    .map_err(|e| format!("failed to commit receive tx: {e}"))?;
                Ok(jobs)
            })
            .await
    }

    async fn ack(&self, job: &BuildJob) -> Result<(), String> {
        let batch_id = job.receipt.clone();
        self.db
            .call(move |conn| {
                conn.execute("DELETE FROM build_jobs WHERE batch_id = ?1", [&batch_id])
                    .map(|_| ())
                    .map_err(|e| format!("failed to ack job {batch_id}: {e}"))
            })
            .await
    }

    async fn nack(&self, job: &BuildJob, error: &str) -> Result<(), String> {
        let now = self.now();
        let batch_id = job.receipt.clone();
        let error = error.to_string();
        let max_attempts = self.max_attempts;
        let base_backoff_ms = self.base_backoff_ms;
        self.db
            .call(move |conn| {
                let tx = conn
                    .transaction()
                    .map_err(|e| format!("failed to open nack tx: {e}"))?;
                // 作业可能已被 ack 或被租约回收后由他人领取——此时无当前行可退避，直接返回。
                let current: Option<(i64, String)> = tx
                    .query_row(
                        "SELECT attempts, body FROM build_jobs WHERE batch_id = ?1",
                        [&batch_id],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(|e| format!("failed to read job {batch_id} for nack: {e}"))?;
                let (attempts, body) = match current {
                    Some(value) => value,
                    None => {
                        tx.commit()
                            .map_err(|e| format!("failed to commit no-op nack: {e}"))?;
                        return Ok(());
                    }
                };
                let new_attempts = attempts + 1;
                if new_attempts as u32 >= max_attempts {
                    // 用尽重试：移入死信并从活动队列删除。
                    tx.execute(
                        "INSERT INTO dead_jobs (batch_id, body, attempts, last_error, died_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(batch_id) DO UPDATE SET
                            body = excluded.body, attempts = excluded.attempts,
                            last_error = excluded.last_error, died_at = excluded.died_at",
                        rusqlite::params![batch_id, body, new_attempts, error, now],
                    )
                    .map_err(|e| format!("failed to dead-letter job {batch_id}: {e}"))?;
                    tx.execute("DELETE FROM build_jobs WHERE batch_id = ?1", [&batch_id])
                        .map_err(|e| format!("failed to remove dead job {batch_id}: {e}"))?;
                } else {
                    // 退避重投：available_at 随尝试次数线性推后，claimed 释放为 ready。
                    let available_at = now + base_backoff_ms * new_attempts;
                    tx.execute(
                        "UPDATE build_jobs SET state = 'ready', attempts = ?2,
                            available_at = ?3, claimed_at = NULL
                         WHERE batch_id = ?1",
                        rusqlite::params![batch_id, new_attempts, available_at],
                    )
                    .map_err(|e| format!("failed to requeue job {batch_id}: {e}"))?;
                }
                tx.commit()
                    .map_err(|e| format!("failed to commit nack tx: {e}"))?;
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn sample_batch(id: &str) -> QueueBatch {
        QueueBatch {
            batch_id: id.to_string(),
            wal_key: format!("wal/2026/07/14/{id}.jsonl"),
            accepted_count: 1,
            wal_event_ids: vec!["evt-1".to_string()],
        }
    }

    /// 返回 (clock, handle)：handle 可推进被 clock 读取的当前时间（毫秒）。
    fn controllable_clock(start: i64) -> (Clock, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(start));
        let handle = now.clone();
        let clock: Clock = Arc::new(move || now.load(Ordering::SeqCst));
        (clock, handle)
    }

    #[tokio::test]
    async fn enqueue_then_receive_then_ack() {
        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let source = SqliteBuildJobSource::new(db);

        queue.enqueue(sample_batch("batch-1")).await.unwrap();
        let jobs = source.receive().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].body.contains("batch-1"));

        source.ack(&jobs[0]).await.unwrap();
        assert!(source.receive().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn claimed_job_is_not_re_delivered_before_lease_expiry() {
        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let (clock, _handle) = controllable_clock(1_000);
        let source = SqliteBuildJobSource::with_clock(db, clock);

        queue.enqueue(sample_batch("batch-1")).await.unwrap();
        assert_eq!(source.receive().await.unwrap().len(), 1);
        // 未 ack，租约未过期 → 第二次 receive 拿不到。
        assert!(source.receive().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn expired_lease_is_reclaimed_and_redelivered() {
        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let (clock, handle) = controllable_clock(1_000);
        let source = SqliteBuildJobSource::with_clock(db, clock);

        queue.enqueue(sample_batch("batch-1")).await.unwrap();
        assert_eq!(source.receive().await.unwrap().len(), 1);
        // 推进时间超过租约 → 作业被回收重投。
        handle.store(1_000 + DEFAULT_LEASE_MS + 1, Ordering::SeqCst);
        let jobs = source.receive().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].receipt, "batch-1");
    }

    async fn dead_letter_count(db: &SqliteDb) -> i64 {
        db.call(|conn| {
            conn.query_row("SELECT COUNT(*) FROM dead_jobs", [], |row| row.get(0))
                .unwrap()
        })
        .await
    }

    #[tokio::test]
    async fn nack_below_max_retries_with_backoff() {
        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let (clock, handle) = controllable_clock(1_000);
        let source = SqliteBuildJobSource::with_clock(db.clone(), clock);

        queue.enqueue(sample_batch("batch-1")).await.unwrap();
        let jobs = source.receive().await.unwrap();
        source.nack(&jobs[0], "boom").await.unwrap();

        // 退避未到：available_at = 1000 + backoff*1 > now → 领取不到，且未死信。
        assert!(source.receive().await.unwrap().is_empty());
        assert_eq!(dead_letter_count(&db).await, 0);

        // 推进过退避 → 重新可领取。
        handle.store(1_000 + DEFAULT_BACKOFF_MS + 1, Ordering::SeqCst);
        let redelivered = source.receive().await.unwrap();
        assert_eq!(redelivered.len(), 1);
        assert_eq!(redelivered[0].receipt, "batch-1");
    }

    #[tokio::test]
    async fn nack_at_max_attempts_dead_letters() {
        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        // 大步长时钟：每轮 nack 后都跨过退避，便于连续领取直到死信。
        let (clock, handle) = controllable_clock(0);
        let source = SqliteBuildJobSource::with_clock(db.clone(), clock);
        queue.enqueue(sample_batch("batch-1")).await.unwrap();

        // DEFAULT_MAX_ATTEMPTS 次失败后进入死信。
        for round in 0..DEFAULT_MAX_ATTEMPTS {
            handle.store((round as i64 + 1) * 1_000_000, Ordering::SeqCst);
            let jobs = source.receive().await.unwrap();
            assert_eq!(jobs.len(), 1, "round {round} should still deliver the job");
            source.nack(&jobs[0], "boom").await.unwrap();
        }

        // 活动队列清空，死信记录一条并保留最后错误。
        handle.store(999_000_000, Ordering::SeqCst);
        assert!(source.receive().await.unwrap().is_empty());
        assert_eq!(dead_letter_count(&db).await, 1);
        let last_error: String = db
            .call(|conn| {
                conn.query_row(
                    "SELECT last_error FROM dead_jobs WHERE batch_id = 'batch-1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
            })
            .await;
        assert_eq!(last_error, "boom");
    }

    #[tokio::test]
    async fn concurrent_receive_claims_each_job_at_most_once() {
        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let source = SqliteBuildJobSource::new(db);
        queue.enqueue(sample_batch("only-one")).await.unwrap();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let source = source.clone();
            handles.push(tokio::spawn(async move { source.receive().await.unwrap() }));
        }
        let mut total = 0;
        for handle in handles {
            total += handle.await.unwrap().len();
        }
        // 原子领取：无论多少并发 receive，唯一的就绪作业只被领取一次。
        assert_eq!(total, 1);
    }
}
