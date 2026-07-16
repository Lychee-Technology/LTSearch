//! SQLite 版构建作业队列。生产侧 [`SqliteBuildQueue`]（`enqueue`）与消费侧
//! [`SqliteBuildJobSource`]（`receive`/`ack`/`nack`）拆成两个结构：本地部署里 write
//! 进程与 index-builder 进程分别构造它们，只共享同一张 `build_jobs` 表。二者都持有
//! 同一个 [`SqliteDb`]，因此 #123 的写路径可让 WAL 追加与作业入队落到同一事务。
//!
//! 队列语义：`receive` 用 `UPDATE … RETURNING` 原子领取（claim）就绪作业，并把租约
//! 过期（claimed 超过 `lease_ms`）的作业回收为 ready；`ack` 删除；`nack` 做重试退避
//! 与死信（达到 `max_attempts` 移入 `dead_jobs`）。领取/租约/退避都基于可注入的时钟，
//! 便于确定性单测。
//!
//! **投递句柄（claim token）**：`receive` 每次领取都为作业生成一个唯一 `claim_token`
//! 并作为 `BuildJob.receipt` 返回；`ack`/`nack` 以 token（而非持久 `batch_id`）为条件
//! 变更行。这样当租约过期、作业被他人重新领取（换了新 token）后，上一持有者的迟到
//! `ack`/`nack` 只会匹配到 0 行而成为 no-op，不会误删/误重投别人的新投递——对齐 SQS
//! receipt handle 语义。read-then-write 的事务用 IMMEDIATE 开启，跨进程串行化。

use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{OptionalExtension, TransactionBehavior};

use super::wal::{append_wal_segment, op_err};
use super::SqliteDb;
use crate::contracts::{BuildJob, BuildJobSource};
use crate::error::IngestError;
use crate::write::{BuildQueue, QueueBatch, WalAppend};

/// 默认最大尝试次数（含首次）。达到后 `nack` 把作业移入 `dead_jobs`。
pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// 默认可见性租约：领取后多久未 ack/nack 即视为 worker 失联，作业被回收重投。
pub(crate) const DEFAULT_LEASE_MS: i64 = 300_000;
/// 默认退避基数：`nack` 重投时 `available_at = now + base * attempts`。
pub(crate) const DEFAULT_BACKOFF_MS: i64 = 1_000;
/// 默认空轮询退避：`receive` 未领到作业时先睡这么久再返回，避免 worker 循环对
/// 即时返回的本地队列忙等打爆 SQLite（对齐 SQS 长轮询的节流效果）。测试置 0。
pub(crate) const DEFAULT_IDLE_BACKOFF_MS: u64 = 500;

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

/// 在给定连接（可为事务）上插入/更新一条就绪构建作业。不管理事务边界——供
/// `enqueue`（独立事务）与原子写路径（并入 WAL 追加的同一事务）复用，保证两条路径
/// 的入队语义一致。
pub(super) fn insert_build_job(
    conn: &rusqlite::Connection,
    batch: &QueueBatch,
) -> rusqlite::Result<()> {
    let body = serde_json::to_string(batch).unwrap_or_default();
    conn.execute(
        "INSERT INTO build_jobs (batch_id, body, state, attempts, available_at, claimed_at)
         VALUES (?1, ?2, 'ready', 0, 0, NULL)
         ON CONFLICT(batch_id) DO UPDATE SET
            body = excluded.body, state = 'ready', attempts = 0,
            available_at = 0, claimed_at = NULL",
        rusqlite::params![batch.batch_id, body],
    )?;
    Ok(())
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
        self.db
            .call(move |conn| {
                insert_build_job(conn, &batch).map_err(|error| IngestError::Operation {
                    message: format!("failed to enqueue build job {batch_id}: {error}"),
                })
            })
            .await
    }

    /// AC-1 原子写路径：在同一个 `BEGIN IMMEDIATE` 事务内追加 WAL 段并入队作业，任一步
    /// 失败整体回滚——事件与作业「要么都落库、要么都不落」，在 ack 前完成。忽略 `_wal`
    /// 参数：直接在本队列的连接上写 WAL 段（本地组合根用同一 `SqliteDb` 构造 WAL 与队列，
    /// 见 trait 文档），从而两写共用一条连接、一个事务。
    async fn append_and_enqueue(
        &self,
        _wal: &dyn WalAppend,
        wal_key: &str,
        wal_bytes: &[u8],
        batch: QueueBatch,
    ) -> Result<(), IngestError> {
        let wal_key = wal_key.to_string();
        let wal_bytes = wal_bytes.to_vec();
        let batch_id = batch.batch_id.clone();
        self.db
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(|e| op_err("failed to open atomic write tx", e))?;
                append_wal_segment(&tx, &wal_key, &wal_bytes)
                    .map_err(|e| op_err(&format!("failed to append WAL {wal_key} (atomic)"), e))?;
                insert_build_job(&tx, &batch).map_err(|e| IngestError::Operation {
                    message: format!(
                        "failed to enqueue build job {batch_id} (atomic, wal_persisted=false): {e}"
                    ),
                })?;
                tx.commit()
                    .map_err(|e| op_err(&format!("failed to commit atomic write {batch_id}"), e))?;
                Ok(())
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
    pub(crate) idle_backoff_ms: u64,
}

impl SqliteBuildJobSource {
    pub fn new(db: SqliteDb) -> Self {
        Self {
            db,
            clock: system_clock(),
            lease_ms: DEFAULT_LEASE_MS,
            max_attempts: max_attempts_from_env(),
            base_backoff_ms: DEFAULT_BACKOFF_MS,
            idle_backoff_ms: DEFAULT_IDLE_BACKOFF_MS,
        }
    }

    /// 测试构造器：注入可控时钟，并把空轮询退避置 0 以保持单测快速、确定。
    #[cfg(test)]
    pub(crate) fn with_clock(db: SqliteDb, clock: Clock) -> Self {
        Self {
            db,
            clock,
            lease_ms: DEFAULT_LEASE_MS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            base_backoff_ms: DEFAULT_BACKOFF_MS,
            idle_backoff_ms: 0,
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
        let jobs = self
            .db
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(|e| format!("failed to open receive tx: {e}"))?;
                // 回收租约过期的 claimed 作业：worker 领取后 lease_ms 内未 ack/nack，
                // 视为失联，重新变为 ready 以便被再次领取（崩溃/重启安全）。清空 claim_token
                // 使原持有者的迟到 ack/nack 失效。
                tx.execute(
                    "UPDATE build_jobs SET state = 'ready', claimed_at = NULL, claim_token = NULL
                     WHERE state = 'claimed' AND claimed_at IS NOT NULL AND claimed_at <= ?1",
                    [now - lease_ms],
                )
                .map_err(|e| format!("failed to reclaim expired leases: {e}"))?;
                // 原子领取全部就绪作业：每行生成唯一 claim_token（hex(randomblob(16))），
                // 作为本次投递的句柄经 receipt 返回。UPDATE … RETURNING 让「判定+置 claimed+
                // 发 token」不可分割。
                let jobs = {
                    let mut stmt = tx
                        .prepare(
                            "UPDATE build_jobs
                             SET state = 'claimed', claimed_at = ?1,
                                 claim_token = lower(hex(randomblob(16)))
                             WHERE state = 'ready' AND available_at <= ?1
                             RETURNING claim_token, body",
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
                Ok::<_, String>(jobs)
            })
            .await?;
        // 空轮询退避：避免 worker 循环对即时返回的本地队列忙等（对齐 SQS 长轮询）。
        if jobs.is_empty() && self.idle_backoff_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.idle_backoff_ms)).await;
        }
        Ok(jobs)
    }

    async fn ack(&self, job: &BuildJob) -> Result<(), String> {
        // 以本次投递的 claim_token（而非 batch_id）为条件删除：若该投递已被租约回收、
        // 作业换了新 token，则匹配 0 行、成为 no-op，不会误删他人的新投递。
        let token = job.receipt.clone();
        self.db
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM build_jobs WHERE claim_token = ?1 AND state = 'claimed'",
                    [&token],
                )
                .map(|_| ())
                .map_err(|e| format!("failed to ack claim {token}: {e}"))
            })
            .await
    }

    async fn nack(&self, job: &BuildJob, error: &str) -> Result<(), String> {
        let now = self.now();
        let token = job.receipt.clone();
        let error = error.to_string();
        let max_attempts = self.max_attempts;
        let base_backoff_ms = self.base_backoff_ms;
        self.db
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(|e| format!("failed to open nack tx: {e}"))?;
                // 以 claim_token 定位本次投递。若已被 ack 或被租约回收后由他人重领（token 变更），
                // 这里匹配不到——说明是过期投递，直接 no-op。
                let current: Option<(String, i64, String)> = tx
                    .query_row(
                        "SELECT batch_id, attempts, body FROM build_jobs
                         WHERE claim_token = ?1 AND state = 'claimed'",
                        [&token],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(|e| format!("failed to read claim {token} for nack: {e}"))?;
                let (batch_id, attempts, body) = match current {
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
                    tx.execute("DELETE FROM build_jobs WHERE claim_token = ?1", [&token])
                        .map_err(|e| format!("failed to remove dead job {batch_id}: {e}"))?;
                } else {
                    // 退避重投：available_at 随尝试次数线性推后，claimed 释放为 ready，
                    // 清空 claim_token（本次投递结束）。
                    let available_at = now + base_backoff_ms * new_attempts;
                    tx.execute(
                        "UPDATE build_jobs SET state = 'ready', attempts = ?2,
                            available_at = ?3, claimed_at = NULL, claim_token = NULL
                         WHERE claim_token = ?1",
                        rusqlite::params![token, new_attempts, available_at],
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
        let source = SqliteBuildJobSource::with_clock(db, system_clock());

        queue.enqueue(sample_batch("batch-1")).await.unwrap();
        let jobs = source.receive().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].body.contains("batch-1"));
        // receipt 是本次投递的 claim_token，不是 batch_id。
        assert_ne!(jobs[0].receipt, "batch-1");

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
        assert!(jobs[0].body.contains("batch-1"));
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
        assert!(redelivered[0].body.contains("batch-1"));
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
        let source = SqliteBuildJobSource::with_clock(db, system_clock());
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

    async fn job_row_count(db: &SqliteDb) -> i64 {
        db.call(|conn| {
            conn.query_row("SELECT COUNT(*) FROM build_jobs", [], |row| row.get(0))
                .unwrap()
        })
        .await
    }

    async fn attempts_of(db: &SqliteDb, batch_id: &str) -> i64 {
        let batch_id = batch_id.to_string();
        db.call(move |conn| {
            conn.query_row(
                "SELECT attempts FROM build_jobs WHERE batch_id = ?1",
                [&batch_id],
                |row| row.get(0),
            )
            .unwrap()
        })
        .await
    }

    // 迟到的 ack 携带过期 claim_token，租约已过期、作业被重领（换了新 token），
    // 原持有者的 ack 必须匹配 0 行、成为 no-op，不得误删他人的新投递。
    #[tokio::test]
    async fn stale_ack_does_not_settle_a_reclaimed_delivery() {
        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let (clock, handle) = controllable_clock(1_000);
        let source = SqliteBuildJobSource::with_clock(db.clone(), clock);

        queue.enqueue(sample_batch("batch-1")).await.unwrap();
        let delivery_a = source.receive().await.unwrap().pop().unwrap(); // token A

        // 租约过期 → 重领得到 token B（≠ A）。
        handle.store(1_000 + DEFAULT_LEASE_MS + 1, Ordering::SeqCst);
        let delivery_b = source.receive().await.unwrap().pop().unwrap();
        assert_ne!(delivery_a.receipt, delivery_b.receipt);

        // 迟到的 A.ack 是 no-op：作业（B 的投递）仍在。
        source.ack(&delivery_a).await.unwrap();
        assert_eq!(job_row_count(&db).await, 1);
        // B 的 ack 才真正结算。
        source.ack(&delivery_b).await.unwrap();
        assert_eq!(job_row_count(&db).await, 0);
    }

    // 迟到的 nack 携带过期 token，同样必须是 no-op：不得重投/死信别人的新投递，
    // 也不得篡改其 attempts。
    #[tokio::test]
    async fn stale_nack_does_not_mutate_a_reclaimed_delivery() {
        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let (clock, handle) = controllable_clock(1_000);
        let source = SqliteBuildJobSource::with_clock(db.clone(), clock);

        queue.enqueue(sample_batch("batch-1")).await.unwrap();
        let delivery_a = source.receive().await.unwrap().pop().unwrap();

        handle.store(1_000 + DEFAULT_LEASE_MS + 1, Ordering::SeqCst);
        let delivery_b = source.receive().await.unwrap().pop().unwrap();

        // 迟到的 A.nack 是 no-op：B 投递未被改动（attempts 仍为 0，仍 claimed）。
        source.nack(&delivery_a, "stale").await.unwrap();
        assert_eq!(attempts_of(&db, "batch-1").await, 0);
        assert_eq!(dead_letter_count(&db).await, 0);
        // B 未被回退成 ready → 未到租约不会被重领。
        assert!(source.receive().await.unwrap().is_empty());
        // B 自己的 nack 才真正生效（attempts→1）。
        source.nack(&delivery_b, "real").await.unwrap();
        assert_eq!(attempts_of(&db, "batch-1").await, 1);
    }

    async fn wal_segment_bytes(db: &SqliteDb, key: &str) -> Option<Vec<u8>> {
        let key = key.to_string();
        db.call(move |conn| {
            conn.query_row(
                "SELECT data FROM wal_segments WHERE segment_key = ?1",
                [&key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .unwrap()
        })
        .await
    }

    // AC-1 正路径：原子写把事件字节与作业行都落库。
    #[tokio::test]
    async fn atomic_append_and_enqueue_persists_both_event_and_job() {
        use crate::local::sqlite::SqliteWalStorage;
        use crate::write::WriteAheadLog;

        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let wal = WriteAheadLog::new(SqliteWalStorage::new(db.clone()));
        let key = "wal/2026/07/15/batch-x.jsonl";

        queue
            .append_and_enqueue(&wal, key, b"{\"e\":1}\n", sample_batch("batch-x"))
            .await
            .unwrap();

        assert_eq!(
            wal_segment_bytes(&db, key).await.as_deref(),
            Some(&b"{\"e\":1}\n"[..])
        );
        assert_eq!(job_row_count(&db).await, 1);
    }

    // AC-1 原子性：作业入队失败时，同一事务里的 WAL 追加必须一并回滚——事件不得残留。
    #[tokio::test]
    async fn atomic_append_and_enqueue_rolls_back_wal_when_job_insert_fails() {
        use crate::local::sqlite::SqliteWalStorage;
        use crate::write::WriteAheadLog;

        let (db, _dir) = SqliteDb::open_temp();
        let queue = SqliteBuildQueue::new(db.clone());
        let wal = WriteAheadLog::new(SqliteWalStorage::new(db.clone()));
        let key = "wal/2026/07/15/batch-x.jsonl";

        // 破坏 build_jobs，使事务内的作业插入失败。
        db.call(|conn| conn.execute("DROP TABLE build_jobs", []).unwrap())
            .await;

        let result = queue
            .append_and_enqueue(&wal, key, b"{\"e\":1}\n", sample_batch("batch-x"))
            .await;

        assert!(result.is_err(), "job insert failure must surface an error");
        // 关键断言：WAL 段随失败的作业插入一起回滚，未残留半个写入。
        assert!(
            wal_segment_bytes(&db, key).await.is_none(),
            "WAL append must roll back with the failed job insert"
        );
    }
}
