use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ltsearch::error::{IngestError, ValidationError};
use ltsearch::models::{Document, WalOperation, WalRecord};
use ltsearch::write::{segment_key, WalStorage, WriteAheadLog};

#[derive(Clone, Debug, Default)]
struct MemoryWalStorage {
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl MemoryWalStorage {
    fn read_utf8(&self, key: &str) -> String {
        String::from_utf8(
            self.objects
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .unwrap_or_default(),
        )
        .unwrap()
    }

    fn contains_key(&self, key: &str) -> bool {
        self.objects.lock().unwrap().contains_key(key)
    }
}

#[async_trait]
impl WalStorage for MemoryWalStorage {
    async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        let mut objects = self.objects.lock().unwrap();
        objects
            .entry(key.to_string())
            .or_default()
            .extend_from_slice(bytes);
        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Vec<u8>, IngestError> {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or_else(|| IngestError::Operation {
                message: format!("missing WAL segment: {key}"),
            })
    }
}

fn sample_document(doc_id: &str) -> Document {
    Document {
        doc_id: doc_id.into(),
        text: format!("document {doc_id}"),
        embedding: None,
        metadata: HashMap::new(),
        timestamp: 1_700_000_000_000,
    }
}

fn sample_record(event_id: &str, doc_id: &str) -> WalRecord {
    WalRecord {
        event_id: event_id.into(),
        doc_id: doc_id.into(),
        op: WalOperation::Upsert,
        document: Some(sample_document(doc_id)),
        timestamp: 1_700_000_000_000,
    }
}

#[test]
fn segment_key_uses_canonical_date_partitioned_jsonl_layout() {
    assert_eq!(
        segment_key(1_700_000_000_000, "segment-000001").unwrap(),
        "wal/2023/11/14/segment-000001.jsonl"
    );
    assert_eq!(
        segment_key(1_735_689_600_000, "batch-a").unwrap(),
        "wal/2025/01/01/batch-a.jsonl"
    );
}

#[tokio::test]
async fn append_writes_jsonl_records_in_order_and_read_returns_them() {
    let storage = MemoryWalStorage::default();
    let wal = WriteAheadLog::new(storage.clone());
    let key = segment_key(1_700_000_000_000, "segment-000007").unwrap();
    let first = sample_record("evt-1", "doc-1");
    let second = sample_record("evt-2", "doc-2");

    wal.append(&key, &first).await.unwrap();
    wal.append(&key, &second).await.unwrap();

    let contents = storage.read_utf8(&key);
    let lines: Vec<_> = contents.lines().collect();
    let read_back = wal.read(&key).await.unwrap();

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], serde_json::to_string(&first).unwrap());
    assert_eq!(lines[1], serde_json::to_string(&second).unwrap());
    assert_eq!(read_back, vec![first, second]);
    assert!(contents.ends_with('\n'));
}

#[tokio::test]
async fn append_rejects_invalid_records_before_persistence() {
    let storage = MemoryWalStorage::default();
    let wal = WriteAheadLog::new(storage.clone());
    let key = segment_key(1_700_000_000_000, "segment-000003").unwrap();
    let invalid = WalRecord {
        event_id: String::new(),
        ..sample_record("evt-1", "doc-1")
    };

    let error = wal.append(&key, &invalid).await.unwrap_err();

    assert!(matches!(
        error,
        IngestError::Validation(ValidationError::Required { field: "event_id" })
    ));
    assert!(!storage.contains_key(&key));
}

#[tokio::test]
async fn read_rejects_invalid_records_already_present_in_storage() {
    let storage = MemoryWalStorage::default();
    let wal = WriteAheadLog::new(storage.clone());
    let key = segment_key(1_700_000_000_000, "segment-000005").unwrap();

    storage
        .append(
            &key,
            br#"{"event_id":"","doc_id":"doc-1","op":"upsert","document":{"doc_id":"doc-1","text":"document doc-1","embedding":null,"metadata":{},"timestamp":1700000000000},"timestamp":1700000000000}
"#,
        )
        .await
        .unwrap();

    let error = wal.read(&key).await.unwrap_err();

    assert!(matches!(
        error,
        IngestError::Validation(ValidationError::Required { field: "event_id" })
    ));
}

#[test]
fn segment_key_rejects_path_like_segment_ids() {
    let error = segment_key(1_700_000_000_000, "../segment").unwrap_err();

    assert!(matches!(
        error,
        IngestError::Validation(ValidationError::InvalidValue {
            field: "segment_id"
        })
    ));
}
