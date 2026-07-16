use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{IngestError, ValidationError};
use crate::models::{DeleteResponse, Document, IngestResponse, WalOperation, WalRecord};

use super::wal::{segment_key, WalStorage, WriteAheadLog};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueBatch {
    pub batch_id: String,
    pub wal_key: String,
    pub accepted_count: usize,
    pub wal_event_ids: Vec<String>,
}

/// 对象安全的 WAL 追加句柄：让 [`BuildQueue::append_and_enqueue`] 的默认实现能在不
/// 泛型化整个 trait 的前提下回调真正的 WAL 存储。`WriteAheadLog<S>` 自动实现它。
#[async_trait]
pub trait WalAppend: Send + Sync {
    async fn append_bytes(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError>;
}

#[async_trait]
impl<S> WalAppend for WriteAheadLog<S>
where
    S: WalStorage,
{
    async fn append_bytes(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        WriteAheadLog::append_bytes(self, key, bytes).await
    }
}

#[async_trait]
pub trait BuildQueue: Clone + Send + Sync + 'static {
    async fn enqueue(&self, batch: QueueBatch) -> Result<(), IngestError>;

    /// 原子地「持久化本批 WAL 字节 + 入队构建作业」（AC-1）。
    ///
    /// **默认实现是非原子的**：先经 `wal` 追加、再 `enqueue`——S3+SQS / 文件系统本就
    /// 无法跨表事务，保持改造前逐字行为（含入队失败时的 batch 上下文与 `wal_persisted`
    /// 提示）。SQLite 后端 override 它，在同一 `BEGIN IMMEDIATE` 事务内提交事件与作业，
    /// 任一步失败则全部回滚，从而在 ack 前原子落库。
    ///
    /// SQLite 的 override 会忽略 `wal` 参数、直接在自己的连接上写 WAL 段与作业行——
    /// 因此本地组合根必须用**同一个** `SqliteDb` 构造 WAL 与队列（PR3 组合根如此接线）。
    async fn append_and_enqueue(
        &self,
        wal: &dyn WalAppend,
        wal_key: &str,
        wal_bytes: &[u8],
        batch: QueueBatch,
    ) -> Result<(), IngestError> {
        wal.append_bytes(wal_key, wal_bytes).await?;
        let batch_id = batch.batch_id.clone();
        self.enqueue(batch)
            .await
            .map_err(|error| IngestError::Operation {
                message: format!(
                    "{error} (batch_id={batch_id}, wal_key={wal_key}, wal_persisted=true)"
                ),
            })
    }
}

#[derive(Clone)]
pub struct WriteApi<S, Q, C = fn() -> i64>
where
    S: WalStorage,
    Q: BuildQueue,
    C: Fn() -> i64 + Send + Sync + 'static,
{
    wal: WriteAheadLog<S>,
    queue: Q,
    clock: C,
}

impl<S, Q> WriteApi<S, Q>
where
    S: WalStorage,
    Q: BuildQueue,
{
    pub fn new(wal: WriteAheadLog<S>, queue: Q) -> Self {
        Self {
            wal,
            queue,
            clock: current_time_millis,
        }
    }
}

impl<S, Q, C> WriteApi<S, Q, C>
where
    S: WalStorage,
    Q: BuildQueue,
    C: Fn() -> i64 + Send + Sync + 'static,
{
    pub fn with_clock<NC>(self, clock: NC) -> WriteApi<S, Q, NC>
    where
        NC: Fn() -> i64 + Send + Sync + 'static,
    {
        WriteApi {
            wal: self.wal,
            queue: self.queue,
            clock,
        }
    }

    pub async fn ingest(&self, documents: Vec<Document>) -> Result<IngestResponse, IngestError> {
        if documents.is_empty() {
            return Err(IngestError::Validation(ValidationError::Required {
                field: "documents",
            }));
        }

        for document in &documents {
            document.validate()?;
        }

        let timestamp = (self.clock)();
        validate_timestamp(timestamp)?;

        let batch_id = self.next_batch_id();
        let wal_key = segment_key(timestamp, &batch_id)?;
        let records = documents
            .into_iter()
            .enumerate()
            .map(|(index, document)| WalRecord {
                event_id: event_id(&batch_id, index),
                doc_id: document.doc_id.clone(),
                op: WalOperation::Upsert,
                document: Some(document),
                timestamp,
            })
            .collect::<Vec<_>>();

        self.append_and_enqueue(&batch_id, &wal_key, &records)
            .await?;

        Ok(IngestResponse {
            accepted_count: records.len(),
            wal_event_ids: records
                .iter()
                .map(|record| record.event_id.clone())
                .collect(),
            batch_id,
        })
    }

    pub async fn delete(&self, doc_ids: Vec<String>) -> Result<DeleteResponse, IngestError> {
        if doc_ids.is_empty() {
            return Err(IngestError::Validation(ValidationError::Required {
                field: "doc_ids",
            }));
        }

        for doc_id in &doc_ids {
            validate_doc_id(doc_id)?;
        }

        let timestamp = (self.clock)();
        validate_timestamp(timestamp)?;

        let batch_id = self.next_batch_id();
        let wal_key = segment_key(timestamp, &batch_id)?;
        let records = doc_ids
            .into_iter()
            .enumerate()
            .map(|(index, doc_id)| WalRecord {
                event_id: event_id(&batch_id, index),
                doc_id,
                op: WalOperation::Delete,
                document: None,
                timestamp,
            })
            .collect::<Vec<_>>();

        self.append_and_enqueue(&batch_id, &wal_key, &records)
            .await?;

        Ok(DeleteResponse {
            accepted_count: records.len(),
            wal_event_ids: records
                .iter()
                .map(|record| record.event_id.clone())
                .collect(),
            batch_id,
        })
    }

    async fn append_and_enqueue(
        &self,
        batch_id: &str,
        wal_key: &str,
        records: &[WalRecord],
    ) -> Result<(), IngestError> {
        let bytes = serialize_records(records)?;
        let batch = QueueBatch {
            batch_id: batch_id.to_string(),
            wal_key: wal_key.to_string(),
            accepted_count: records.len(),
            wal_event_ids: records
                .iter()
                .map(|record| record.event_id.clone())
                .collect(),
        };
        // 委托给队列后端的合并写：默认非原子（append→enqueue），SQLite 原子（单事务）。
        self.queue
            .append_and_enqueue(&self.wal, wal_key, &bytes, batch)
            .await
    }

    fn next_batch_id(&self) -> String {
        format!("batch-{}", Uuid::new_v4().simple())
    }
}

/// 校验每条记录并拼成一段 JSONL 字节（每条一行）。
fn serialize_records(records: &[WalRecord]) -> Result<Vec<u8>, IngestError> {
    let mut bytes = Vec::new();
    for record in records {
        record.validate()?;
        let mut line = serde_json::to_vec(record).map_err(|error| IngestError::Operation {
            message: error.to_string(),
        })?;
        line.push(b'\n');
        bytes.extend_from_slice(&line);
    }
    Ok(bytes)
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn event_id(batch_id: &str, index: usize) -> String {
    format!("{batch_id}-{:06}", index + 1)
}

fn validate_timestamp(timestamp: i64) -> Result<(), IngestError> {
    if timestamp < 1_000_000_000_000 {
        return Err(IngestError::Validation(ValidationError::InvalidValue {
            field: "timestamp",
        }));
    }

    Ok(())
}

fn validate_doc_id(doc_id: &str) -> Result<(), IngestError> {
    if doc_id.is_empty() {
        return Err(IngestError::Validation(ValidationError::Required {
            field: "doc_id",
        }));
    }

    if doc_id.len() > 256 {
        return Err(IngestError::Validation(ValidationError::LengthOutOfRange {
            field: "doc_id",
            min: 1,
            max: 256,
        }));
    }

    Ok(())
}
