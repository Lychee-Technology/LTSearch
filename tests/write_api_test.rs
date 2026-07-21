use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ltsearch::error::{IngestError, ValidationError};
use ltsearch::models::{Document, WalOperation, WalRecord};
use ltsearch::write::{BuildQueue, QueueBatch, WalStorage, WriteAheadLog, WriteApi};

#[derive(Clone, Debug, Default)]
struct MemoryWalStorage {
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    append_calls: Arc<Mutex<usize>>,
}

#[derive(Clone, Debug)]
struct FileWalStorage {
    root: Arc<PathBuf>,
}

impl FileWalStorage {
    fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(root),
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl WalStorage for FileWalStorage {
    async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| IngestError::Operation {
                message: error.to_string(),
            })?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| IngestError::Operation {
                message: error.to_string(),
            })?;
        file.write_all(bytes)
            .map_err(|error| IngestError::Operation {
                message: error.to_string(),
            })
    }

    async fn read(&self, key: &str) -> Result<Vec<u8>, IngestError> {
        fs::read(self.path_for(key)).map_err(|error| IngestError::Operation {
            message: error.to_string(),
        })
    }
}

impl MemoryWalStorage {
    fn has_contents(&self, key: &str) -> bool {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .is_some_and(|bytes| !bytes.is_empty())
    }

    fn append_count(&self) -> usize {
        *self.append_calls.lock().unwrap()
    }

    fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }
}

#[async_trait]
impl WalStorage for MemoryWalStorage {
    async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        *self.append_calls.lock().unwrap() += 1;
        self.objects
            .lock()
            .unwrap()
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

#[derive(Clone, Debug, Default)]
struct RecordingQueue {
    batches: Arc<Mutex<Vec<QueueBatch>>>,
}

impl RecordingQueue {
    fn batches(&self) -> Vec<QueueBatch> {
        self.batches.lock().unwrap().clone()
    }
}

#[async_trait]
impl BuildQueue for RecordingQueue {
    async fn enqueue(&self, batch: QueueBatch) -> Result<(), IngestError> {
        self.batches.lock().unwrap().push(batch);
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct FailingQueue {
    message: &'static str,
}

#[async_trait]
impl BuildQueue for FailingQueue {
    async fn enqueue(&self, _batch: QueueBatch) -> Result<(), IngestError> {
        Err(IngestError::Operation {
            message: self.message.into(),
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

fn unique_temp_dir(test_name: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ltsearch-{test_name}-{}-{suffix}",
        std::process::id()
    ))
}

fn collect_relative_file_paths(root: &Path) -> Vec<String> {
    fn visit(root: &Path, dir: &Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }

    let mut files = Vec::new();
    if root.exists() {
        visit(root, root, &mut files);
    }
    files.sort();
    files
}

#[tokio::test]
async fn ingest_appends_upsert_records_and_enqueues_batch_metadata() {
    let storage = MemoryWalStorage::default();
    let queue = RecordingQueue::default();
    let wal = WriteAheadLog::new(storage.clone());
    let api = WriteApi::new(wal.clone(), queue.clone()).with_clock(|| 1_700_000_000_000);
    let first = sample_document("doc-1");
    let second = sample_document("doc-2");

    let response = api
        .ingest(vec![first.clone(), second.clone()])
        .await
        .unwrap();
    let wal_key = format!("wal/2023/11/14/{}.jsonl", response.batch_id);
    let records = wal.read(&wal_key).await.unwrap();
    let queued = queue.batches();

    assert_eq!(response.accepted_count, 2);
    assert_eq!(
        response.wal_event_ids,
        vec![
            format!("{}-000001", response.batch_id),
            format!("{}-000002", response.batch_id)
        ]
    );
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].op, WalOperation::Upsert);
    assert_eq!(records[0].doc_id, "doc-1");
    assert_eq!(records[0].document, Some(first));
    assert_eq!(records[1].op, WalOperation::Upsert);
    assert_eq!(records[1].doc_id, "doc-2");
    assert_eq!(records[1].document, Some(second));
    assert!(!response.wal_key.is_empty());
    assert_eq!(response.wal_key, wal_key);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].batch_id, response.batch_id);
    assert_eq!(queued[0].wal_key, wal_key);
    assert_eq!(queued[0].accepted_count, 2);
    assert_eq!(queued[0].wal_event_ids, response.wal_event_ids);
}

#[tokio::test]
async fn delete_appends_delete_records_and_enqueues_batch_metadata() {
    let storage = MemoryWalStorage::default();
    let queue = RecordingQueue::default();
    let wal = WriteAheadLog::new(storage.clone());
    let api = WriteApi::new(wal.clone(), queue.clone()).with_clock(|| 1_700_000_086_400);

    let response = api
        .delete(vec!["doc-1".into(), "doc-2".into()])
        .await
        .unwrap();
    let wal_key = format!("wal/2023/11/14/{}.jsonl", response.batch_id);
    let records = wal.read(&wal_key).await.unwrap();
    let queued = queue.batches();

    assert_eq!(response.accepted_count, 2);
    assert_eq!(
        response.wal_event_ids,
        vec![
            format!("{}-000001", response.batch_id),
            format!("{}-000002", response.batch_id)
        ]
    );
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].op, WalOperation::Delete);
    assert_eq!(records[0].doc_id, "doc-1");
    assert_eq!(records[0].document, None);
    assert_eq!(records[0].timestamp, 1_700_000_086_400);
    assert_eq!(records[1].op, WalOperation::Delete);
    assert_eq!(records[1].doc_id, "doc-2");
    assert_eq!(records[1].document, None);
    assert_eq!(records[1].timestamp, 1_700_000_086_400);
    assert!(!response.wal_key.is_empty());
    assert_eq!(response.wal_key, wal_key);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].batch_id, response.batch_id);
    assert_eq!(queued[0].wal_key, wal_key);
    assert_eq!(queued[0].accepted_count, 2);
}

#[tokio::test]
async fn wal_append_happens_before_enqueue() {
    let storage = MemoryWalStorage::default();
    let queue = RecordingQueue::default();
    let api = WriteApi::new(WriteAheadLog::new(storage.clone()), queue.clone())
        .with_clock(|| 1_700_000_000_000);

    let response = api.ingest(vec![sample_document("doc-1")]).await.unwrap();
    let wal_key = format!("wal/2023/11/14/{}.jsonl", response.batch_id);

    assert!(storage.has_contents(&wal_key));
    assert_eq!(queue.batches()[0].wal_key, wal_key);
}

#[tokio::test]
async fn ingest_rejects_invalid_documents_before_side_effects() {
    let storage = MemoryWalStorage::default();
    let queue = RecordingQueue::default();
    let wal = WriteAheadLog::new(storage.clone());
    let api = WriteApi::new(wal, queue.clone()).with_clock(|| 1_700_000_000_000);
    let mut invalid = sample_document("doc-1");
    invalid.text = String::new();

    let error = api.ingest(vec![invalid]).await.unwrap_err();

    assert!(matches!(
        error,
        IngestError::Validation(ValidationError::Required { field: "text" })
    ));
    assert!(queue.batches().is_empty());
    assert!(storage.objects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn delete_rejects_invalid_doc_ids_before_side_effects() {
    let storage = MemoryWalStorage::default();
    let queue = RecordingQueue::default();
    let wal = WriteAheadLog::new(storage.clone());
    let api = WriteApi::new(wal, queue.clone()).with_clock(|| 1_700_000_000_000);

    let error = api.delete(vec![String::new()]).await.unwrap_err();

    assert!(matches!(
        error,
        IngestError::Validation(ValidationError::Required { field: "doc_id" })
    ));
    assert!(queue.batches().is_empty());
    assert!(storage.objects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn delete_rejects_invalid_clock_before_side_effects() {
    let storage = MemoryWalStorage::default();
    let queue = RecordingQueue::default();
    let wal = WriteAheadLog::new(storage.clone());
    let api = WriteApi::new(wal, queue.clone()).with_clock(|| 1_700_000_000);

    let error = api.delete(vec!["doc-1".into()]).await.unwrap_err();

    assert!(matches!(
        error,
        IngestError::Validation(ValidationError::InvalidValue { field: "timestamp" })
    ));
    assert!(queue.batches().is_empty());
    assert!(storage.objects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ingest_emits_expected_upsert_wal_records() {
    let storage = MemoryWalStorage::default();
    let wal = WriteAheadLog::new(storage.clone());
    let api =
        WriteApi::new(wal.clone(), RecordingQueue::default()).with_clock(|| 1_700_000_000_000);

    api.ingest(vec![sample_document("doc-1")]).await.unwrap();

    let batch_id = storage
        .keys()
        .into_iter()
        .next()
        .unwrap()
        .trim_start_matches("wal/2023/11/14/")
        .trim_end_matches(".jsonl")
        .to_string();

    assert_eq!(
        wal.read(&format!("wal/2023/11/14/{batch_id}.jsonl"))
            .await
            .unwrap(),
        vec![WalRecord {
            event_id: format!("{batch_id}-000001"),
            doc_id: "doc-1".into(),
            op: WalOperation::Upsert,
            document: Some(sample_document("doc-1")),
            timestamp: 1_700_000_000_000,
        }]
    );
}

#[tokio::test]
async fn ingest_uses_distinct_batch_ids_across_write_api_instances() {
    let first_storage = MemoryWalStorage::default();
    let second_storage = MemoryWalStorage::default();
    let first_api = WriteApi::new(
        WriteAheadLog::new(first_storage.clone()),
        RecordingQueue::default(),
    )
    .with_clock(|| 1_700_000_000_000);
    let second_api = WriteApi::new(
        WriteAheadLog::new(second_storage.clone()),
        RecordingQueue::default(),
    )
    .with_clock(|| 1_700_000_000_001);

    let first_response = first_api
        .ingest(vec![sample_document("doc-1")])
        .await
        .unwrap();
    let second_response = second_api
        .ingest(vec![sample_document("doc-2")])
        .await
        .unwrap();

    assert_ne!(first_response.batch_id, second_response.batch_id);
    assert_ne!(
        first_response.wal_event_ids[0],
        second_response.wal_event_ids[0]
    );
}

#[tokio::test]
async fn ingest_writes_one_wal_object_per_batch() {
    let storage = MemoryWalStorage::default();
    let wal = WriteAheadLog::new(storage.clone());
    let api = WriteApi::new(wal, RecordingQueue::default()).with_clock(|| 1_700_000_000_000);

    api.ingest(vec![sample_document("doc-1"), sample_document("doc-2")])
        .await
        .unwrap();

    assert_eq!(storage.append_count(), 1);
}

#[tokio::test]
async fn restart_safe_batch_ids_produce_distinct_same_day_wal_keys() {
    const CHILD_DIR: &str = "LTSEARCH_RESTART_SAFE_CHILD_DIR";
    const CHILD_DOC_ID: &str = "LTSEARCH_RESTART_SAFE_CHILD_DOC_ID";

    if let Ok(root) = std::env::var(CHILD_DIR) {
        let storage = FileWalStorage::new(PathBuf::from(root));
        let wal = WriteAheadLog::new(storage);
        let api = WriteApi::new(wal, RecordingQueue::default()).with_clock(|| 1_700_000_000_000);

        api.ingest(vec![sample_document(
            &std::env::var(CHILD_DOC_ID).unwrap_or_else(|_| "doc-child".into()),
        )])
        .await
        .unwrap();
        return;
    }

    let root = unique_temp_dir("restart-safe-batch-ids");
    let test_binary = std::env::current_exe().unwrap();
    let test_name = "restart_safe_batch_ids_produce_distinct_same_day_wal_keys";

    for doc_id in ["doc-1", "doc-2"] {
        let status = Command::new(&test_binary)
            .arg("--exact")
            .arg(test_name)
            .env(CHILD_DIR, &root)
            .env(CHILD_DOC_ID, doc_id)
            .status()
            .unwrap();
        assert!(status.success());
    }

    let paths = collect_relative_file_paths(&root);

    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1]);
    for path in paths {
        assert!(path.starts_with("wal/2023/11/14/batch-"));
        assert!(path.ends_with(".jsonl"));
        assert!(!path.contains(".."));
        assert!(!path.contains('\\'));
    }
}

#[tokio::test]
async fn ingest_reports_batch_context_when_enqueue_fails_after_wal_append() {
    let storage = MemoryWalStorage::default();
    let wal = WriteAheadLog::new(storage.clone());
    let api = WriteApi::new(
        wal,
        FailingQueue {
            message: "queue unavailable",
        },
    )
    .with_clock(|| 1_700_000_000_000);

    let error = api
        .ingest(vec![sample_document("doc-1")])
        .await
        .unwrap_err();

    let message = error.to_string();
    assert!(message.contains("queue unavailable"));
    assert!(message.contains("batch-"));
    assert!(message.contains("wal/2023/11/14/"));
}
