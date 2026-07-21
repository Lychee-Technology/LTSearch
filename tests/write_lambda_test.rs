use ltsearch::error::{IngestError, ValidationError};
use ltsearch::models::{DeleteResponse, Document, IngestResponse};
use ltsearch::write_lambda::{handle_write_request, WriteRequest};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

fn sample_ingest_request() -> WriteRequest {
    WriteRequest::Ingest {
        documents: vec![Document {
            doc_id: "doc-1".into(),
            text: "hello world".into(),
            embedding: None,
            metadata: Default::default(),
            timestamp: 1700000000000,
        }],
    }
}

fn sample_delete_request() -> WriteRequest {
    WriteRequest::Delete {
        doc_ids: vec!["doc-1".into(), "doc-2".into()],
    }
}

#[test]
fn write_lambda_maps_successful_ingest_to_response_envelope() {
    let result = block_on(handle_write_request(
        async |documents: Vec<Document>| {
            Ok(IngestResponse {
                accepted_count: documents.len(),
                wal_event_ids: vec!["evt-1".into()],
                batch_id: "batch-abc".into(),
                wal_key: "wal/2023/11/14/batch-abc.jsonl".into(),
            })
        },
        async |_doc_ids| unreachable!("ingest should not call delete handler"),
        sample_ingest_request(),
    ));

    let response = result.unwrap();
    assert_eq!(response.accepted_count, 1);
    assert_eq!(response.batch_id, "batch-abc");
    assert_eq!(response.wal_key, "wal/2023/11/14/batch-abc.jsonl");
}

#[test]
fn write_lambda_maps_successful_delete_to_response_envelope() {
    let result = block_on(handle_write_request(
        async |_documents| unreachable!("delete should not call ingest handler"),
        async |doc_ids: Vec<String>| {
            Ok(DeleteResponse {
                accepted_count: doc_ids.len(),
                wal_event_ids: vec!["evt-1".into(), "evt-2".into()],
                batch_id: "batch-def".into(),
                wal_key: "wal/2023/11/14/batch-def.jsonl".into(),
            })
        },
        sample_delete_request(),
    ));

    let response = result.unwrap();
    assert_eq!(response.accepted_count, 2);
    assert_eq!(response.batch_id, "batch-def");
    assert_eq!(response.wal_event_ids, vec!["evt-1", "evt-2"]);
    assert_eq!(response.wal_key, "wal/2023/11/14/batch-def.jsonl");
}

#[test]
fn write_lambda_maps_validation_error_to_error_envelope() {
    let result = block_on(handle_write_request(
        async |_documents| {
            Err(IngestError::Validation(ValidationError::Required {
                field: "documents",
            }))
        },
        async |_doc_ids| unreachable!("validation error test should not call delete handler"),
        sample_ingest_request(),
    ));

    let error = result.unwrap_err();
    assert_eq!(error.error_type, "validation_error");
    assert_eq!(error.message, "documents is required");
}

#[test]
fn write_lambda_maps_operation_error_to_error_envelope() {
    let result = block_on(handle_write_request(
        async |_documents| {
            Err(IngestError::Operation {
                message: "S3 write failed".into(),
            })
        },
        async |_doc_ids| unreachable!("operation error test should not call delete handler"),
        sample_ingest_request(),
    ));

    let error = result.unwrap_err();
    assert_eq!(error.error_type, "operation_error");
    assert_eq!(error.message, "ingest operation failed: S3 write failed");
}
