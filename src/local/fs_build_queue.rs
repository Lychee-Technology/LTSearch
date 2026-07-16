//! 文件系统构建队列：`enqueue` 把 `QueueBatch` 序列化成 `queue/<batch_id>.json`；
//! `receive` 读回全部待处理文件，`ack` 删除。同时实现生产侧 `BuildQueue` 与
//! 消费侧 `BuildJobSource`，本地单进程即可闭环 write→build 触发。

use std::path::PathBuf;

use async_trait::async_trait;

use crate::contracts::{BuildJob, BuildJobSource};
use crate::error::IngestError;
use crate::write::{BuildQueue, QueueBatch};

#[derive(Debug, Clone)]
pub struct LocalFsBuildQueue {
    dir: PathBuf,
}

impl LocalFsBuildQueue {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            dir: root.into().join("queue"),
        }
    }
}

#[async_trait]
impl BuildQueue for LocalFsBuildQueue {
    async fn enqueue(&self, batch: QueueBatch) -> Result<(), IngestError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to create local queue dir: {error}"),
            })?;
        let body = serde_json::to_vec(&batch).map_err(|error| IngestError::Operation {
            message: format!("failed to encode queue batch: {error}"),
        })?;
        let path = self.dir.join(format!("{}.json", batch.batch_id));
        tokio::fs::write(&path, body)
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to write queue file: {error}"),
            })
    }
}

#[async_trait]
impl BuildJobSource for LocalFsBuildQueue {
    async fn receive(&self) -> Result<Vec<BuildJob>, String> {
        let mut jobs = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(_) => return Ok(jobs), // empty/absent queue dir → no jobs
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| format!("failed to scan queue dir: {error}"))?
        {
            let path = entry.path();
            let body = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("failed to read queue file: {error}"))?;
            jobs.push(BuildJob {
                receipt: path.to_string_lossy().into_owned(),
                body,
            });
        }
        Ok(jobs)
    }

    async fn ack(&self, job: &BuildJob) -> Result<(), String> {
        tokio::fs::remove_file(&job.receipt)
            .await
            .map_err(|error| format!("failed to ack queue job: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enqueue_then_receive_then_ack() {
        let dir = tempfile::tempdir().unwrap();
        let queue = LocalFsBuildQueue::new(dir.path());
        let batch = QueueBatch {
            batch_id: "batch-1".to_string(),
            wal_key: "wal/2026/07/14/batch-1.jsonl".to_string(),
            accepted_count: 1,
            wal_event_ids: vec!["evt-1".to_string()],
        };

        queue.enqueue(batch).await.unwrap();
        let jobs = queue.receive().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].body.contains("batch-1"));

        queue.ack(&jobs[0]).await.unwrap();
        assert!(queue.receive().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn nack_defaults_to_ack() {
        // 回归保护：BuildJobSource::nack 的默认实现等价于 ack，本地/AWS 侧失败照常删除，
        // 行为与新增 nack 契约前逐字一致。
        let dir = tempfile::tempdir().unwrap();
        let queue = LocalFsBuildQueue::new(dir.path());
        queue
            .enqueue(QueueBatch {
                batch_id: "batch-1".to_string(),
                wal_key: "wal/2026/07/14/batch-1.jsonl".to_string(),
                accepted_count: 1,
                wal_event_ids: vec!["evt-1".to_string()],
            })
            .await
            .unwrap();
        let jobs = queue.receive().await.unwrap();

        queue.nack(&jobs[0], "boom").await.unwrap();

        assert!(queue.receive().await.unwrap().is_empty());
    }
}
