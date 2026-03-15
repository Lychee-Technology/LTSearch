use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::error::{IngestError, ValidationError};
use crate::models::{DeleteResponse, Document, IngestResponse, WalOperation, WalRecord};

use super::wal::{segment_key, WalStorage, WriteAheadLog};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueBatch {
    pub batch_id: String,
    pub wal_key: String,
    pub accepted_count: usize,
    pub wal_event_ids: Vec<String>,
}

pub trait BuildQueue: Clone + Send + Sync + 'static {
    fn enqueue(&self, batch: QueueBatch) -> Result<(), IngestError>;
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

    pub fn ingest(&self, documents: Vec<Document>) -> Result<IngestResponse, IngestError> {
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

        self.append_and_enqueue(&batch_id, &wal_key, &records)?;

        Ok(IngestResponse {
            accepted_count: records.len(),
            wal_event_ids: records
                .iter()
                .map(|record| record.event_id.clone())
                .collect(),
            batch_id,
        })
    }

    pub fn delete(&self, doc_ids: Vec<String>) -> Result<DeleteResponse, IngestError> {
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

        self.append_and_enqueue(&batch_id, &wal_key, &records)?;

        Ok(DeleteResponse {
            accepted_count: records.len(),
            wal_event_ids: records
                .iter()
                .map(|record| record.event_id.clone())
                .collect(),
            batch_id,
        })
    }

    fn append_and_enqueue(
        &self,
        batch_id: &str,
        wal_key: &str,
        records: &[WalRecord],
    ) -> Result<(), IngestError> {
        append_records(&self.wal, wal_key, records)?;

        self.queue
            .enqueue(QueueBatch {
                batch_id: batch_id.to_string(),
                wal_key: wal_key.to_string(),
                accepted_count: records.len(),
                wal_event_ids: records
                    .iter()
                    .map(|record| record.event_id.clone())
                    .collect(),
            })
            .map_err(|error| IngestError::Operation {
                message: format!(
                    "{} (batch_id={batch_id}, wal_key={wal_key}, wal_persisted=true)",
                    error
                ),
            })
    }

    fn next_batch_id(&self) -> String {
        format!("batch-{}", Uuid::new_v4().simple())
    }
}

fn append_records<S>(
    wal: &WriteAheadLog<S>,
    wal_key: &str,
    records: &[WalRecord],
) -> Result<(), IngestError>
where
    S: WalStorage,
{
    let mut bytes = Vec::new();
    for record in records {
        record.validate()?;
        let mut line = serde_json::to_vec(record).map_err(|error| IngestError::Operation {
            message: error.to_string(),
        })?;
        line.push(b'\n');
        bytes.extend_from_slice(&line);
    }

    wal.append_bytes(wal_key, &bytes)
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
