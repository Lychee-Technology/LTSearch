use serde::{Deserialize, Serialize};

use crate::error::IngestError;
use crate::models::{DeleteResponse, Document, IngestResponse};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum WriteRequest {
    Ingest { documents: Vec<Document> },
    Delete { doc_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WriteResponse {
    pub accepted_count: usize,
    pub wal_event_ids: Vec<String>,
    pub batch_id: String,
    pub wal_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteLambdaError {
    pub error_type: String,
    pub message: String,
}

impl From<IngestError> for WriteLambdaError {
    fn from(error: IngestError) -> Self {
        match error {
            IngestError::Validation(source) => Self {
                error_type: "validation_error".into(),
                message: source.to_string(),
            },
            IngestError::Operation { message } => Self {
                error_type: "operation_error".into(),
                message: IngestError::Operation { message }.to_string(),
            },
        }
    }
}

pub async fn handle_write_request<I, D>(
    ingest_handler: I,
    delete_handler: D,
    request: WriteRequest,
) -> Result<WriteResponse, WriteLambdaError>
where
    I: AsyncFnOnce(Vec<Document>) -> Result<IngestResponse, IngestError>,
    D: AsyncFnOnce(Vec<String>) -> Result<DeleteResponse, IngestError>,
{
    match request {
        WriteRequest::Ingest { documents } => {
            let response = ingest_handler(documents)
                .await
                .map_err(WriteLambdaError::from)?;
            Ok(WriteResponse {
                accepted_count: response.accepted_count,
                wal_event_ids: response.wal_event_ids,
                batch_id: response.batch_id,
                wal_key: response.wal_key,
            })
        }
        WriteRequest::Delete { doc_ids } => {
            let response = delete_handler(doc_ids)
                .await
                .map_err(WriteLambdaError::from)?;
            Ok(WriteResponse {
                accepted_count: response.accepted_count,
                wal_event_ids: response.wal_event_ids,
                batch_id: response.batch_id,
                wal_key: response.wal_key,
            })
        }
    }
}
