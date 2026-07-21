use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

use super::index::Document;

const MIN_PLAUSIBLE_EPOCH_MILLIS: i64 = 1_000_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalOperation {
    Upsert,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalRecord {
    pub event_id: String,
    pub doc_id: String,
    pub op: WalOperation,
    pub document: Option<Document>,
    pub timestamp: i64,
}

impl WalRecord {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.event_id.is_empty() {
            return Err(ValidationError::Required { field: "event_id" });
        }
        if self.doc_id.is_empty() {
            return Err(ValidationError::Required { field: "doc_id" });
        }
        if !is_plausible_epoch_millis(self.timestamp) {
            return Err(ValidationError::InvalidValue { field: "timestamp" });
        }

        match (&self.op, &self.document) {
            (WalOperation::Upsert, None) => Err(ValidationError::Required { field: "document" }),
            (WalOperation::Delete, Some(_)) => {
                Err(ValidationError::InvalidValue { field: "document" })
            }
            (_, Some(document)) if document.doc_id != self.doc_id => {
                Err(ValidationError::Mismatch {
                    field: "doc_id",
                    expected: "document.doc_id",
                })
            }
            (_, Some(document)) => document.validate(),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestResponse {
    pub accepted_count: usize,
    pub wal_event_ids: Vec<String>,
    pub batch_id: String,
    pub wal_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteResponse {
    pub accepted_count: usize,
    pub wal_event_ids: Vec<String>,
    pub batch_id: String,
    pub wal_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    pub status: String,
    pub index_version: Option<u64>,
    pub cache: Option<super::index::CacheStats>,
}

fn is_plausible_epoch_millis(value: i64) -> bool {
    value >= MIN_PLAUSIBLE_EPOCH_MILLIS
}
