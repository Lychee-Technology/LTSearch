use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_config::retry::RetryConfig;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sqs::Client as SqsClient;
use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;
use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::indexing::PublishStorage;
use ltsearch::indexing::{
    BuildIndexRequest, BuildIndexResult, IndexPublisher, LocalIndexBuilder, PublishRequest,
};
use ltsearch::write::{BuildQueue, WalStorage};

struct LocalstackHarness {
    artifact_root: std::path::PathBuf,
    bucket: String,
    queue_url: String,
    s3: S3Client,
    sqs: SqsClient,
}

#[tokio::test]
async fn localstack_smoke_test_can_create_bucket_and_queue() {
    let harness = LocalstackHarness::new("bootstrap-smoke").await;
    assert!(harness.bucket_exists().await);
    assert!(harness.queue_exists().await);
}

#[tokio::test]
async fn write_api_ingest_can_be_awaited_in_integration_context() {
    let api = test_write_api();
    let response = api.ingest(Vec::new()).await;
    assert!(response.is_err());
}

#[tokio::test]
async fn index_publisher_publish_can_be_awaited_in_integration_context() {
    let publisher = test_publisher();
    let request = test_publish_request();
    let result = publisher.publish(&request).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn localstack_harness_can_construct_all_adapter_types() {
    let harness = LocalstackHarness::new("adapter-constructors").await;
    let _ = AwsS3WalStorage::new(harness.bucket.clone(), harness.s3.clone());
    let _ = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());
    let _ = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
}

#[tokio::test]
async fn s3_wal_storage_first_append_creates_object() {
    let harness = LocalstackHarness::new("s3-wal-create").await;
    let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
        harness.bucket.clone(),
        harness.s3.clone(),
    ));

    wal.append_bytes("wal/2023/11/14/batch-test.jsonl", b"line-1\n")
        .await
        .unwrap();

    let stored = harness
        .read_s3_text("wal/2023/11/14/batch-test.jsonl")
        .await;
    assert_eq!(stored, "line-1\n");
}

#[tokio::test]
async fn s3_wal_storage_round_trips_jsonl_bytes_against_localstack() {
    let harness = LocalstackHarness::new("s3-wal-roundtrip").await;
    let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
        harness.bucket.clone(),
        harness.s3.clone(),
    ));
    let key = "wal/2023/11/14/batch-test.jsonl";

    wal.append_bytes(
        key,
        b"{\"event_id\":\"e1\",\"doc_id\":\"doc-1\",\"op\":\"delete\",\"document\":null,\"timestamp\":1700000000000}\n",
    )
    .await
    .unwrap();

    let records = wal.read(key).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].doc_id, "doc-1");
}

#[tokio::test]
async fn sqs_build_queue_enqueues_batch_metadata_against_localstack() {
    let harness = LocalstackHarness::new("sqs-build-queue").await;
    let queue = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());

    queue
        .enqueue(ltsearch::write::QueueBatch {
            batch_id: "batch-123".into(),
            wal_key: "wal/2023/11/14/batch-123.jsonl".into(),
            accepted_count: 2,
            wal_event_ids: vec!["batch-123-000001".into(), "batch-123-000002".into()],
        })
        .await
        .unwrap();

    let message = harness.receive_one_message_body().await;
    assert!(message.contains("batch-123"));
    assert!(message.contains("wal/2023/11/14/batch-123.jsonl"));
}

#[tokio::test]
async fn publish_storage_uploads_and_reads_manifest_bytes() {
    let harness = LocalstackHarness::new("publish-storage-read").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
    let artifact_root = harness.new_artifact_root();
    let manifest_path = artifact_root.join("index/versions/7/manifest.json");
    std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    std::fs::write(&manifest_path, b"{}\n").unwrap();

    storage
        .upload_file("index/versions/7/manifest.json", &manifest_path)
        .await
        .unwrap();
    assert!(storage
        .read("index/versions/7/manifest.json")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn publish_storage_compare_and_swap_updates_head_when_expected_matches() {
    let harness = LocalstackHarness::new("publish-storage-cas").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    let swapped = storage
        .compare_and_swap("index/_head", None, b"{}")
        .await
        .unwrap();
    assert!(swapped);
}

#[tokio::test]
async fn publish_storage_compare_and_swap_returns_false_when_expected_mismatches() {
    let harness = LocalstackHarness::new("publish-storage-cas-mismatch").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    assert!(storage
        .compare_and_swap("index/_head", None, b"old")
        .await
        .unwrap());
    assert!(!storage
        .compare_and_swap("index/_head", Some(b"different"), b"new")
        .await
        .unwrap());
}

#[tokio::test]
async fn publish_storage_read_propagates_non_missing_object_errors() {
    let server = MockS3Server::start(vec![MockHttpResponse::access_denied()]);
    let storage = AwsPublishStorage::new(
        "test-bucket",
        s3_client_for_endpoint(&server.endpoint_url).await,
    );

    let error = storage
        .read("index/_head")
        .await
        .expect_err("expected read to fail");

    assert!(error
        .to_string()
        .contains("failed to load object index/_head"));
    assert_eq!(
        server.finish(),
        vec!["GET /test-bucket/index/_head?x-id=GetObject".to_string()]
    );
}

#[tokio::test]
async fn s3_wal_append_stops_before_put_when_existing_read_fails() {
    let server = MockS3Server::start(vec![MockHttpResponse::access_denied()]);
    let wal = AwsS3WalStorage::new(
        "test-bucket",
        s3_client_for_endpoint(&server.endpoint_url).await,
    );

    let error = wal
        .append("wal/2023/11/14/batch-test.jsonl", b"line-2\n")
        .await
        .expect_err("expected append to fail");

    assert!(error
        .to_string()
        .contains("failed to load existing WAL object wal/2023/11/14/batch-test.jsonl"));
    assert_eq!(
        server.finish(),
        vec!["GET /test-bucket/wal/2023/11/14/batch-test.jsonl?x-id=GetObject".to_string()]
    );
}

#[tokio::test]
async fn localstack_harness_receives_and_decodes_one_queue_batch() {
    let harness = LocalstackHarness::new("decode-batch").await;
    AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone())
        .enqueue(ltsearch::write::QueueBatch {
            batch_id: "batch-xyz".into(),
            wal_key: "wal/2023/11/14/batch-xyz.jsonl".into(),
            accepted_count: 1,
            wal_event_ids: vec!["batch-xyz-000001".into()],
        })
        .await
        .unwrap();

    let batch = harness.receive_batch().await;
    assert_eq!(batch.batch_id, "batch-xyz");
    assert_eq!(batch.accepted_count, 1);
}

#[tokio::test]
async fn write_build_publish_flow_runs_end_to_end_against_localstack() {
    let harness = LocalstackHarness::new("write-build-publish").await;
    let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
        harness.bucket.clone(),
        harness.s3.clone(),
    ));
    let queue = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());
    let api = ltsearch::write::WriteApi::new(wal, queue).with_clock(|| 1_700_000_000_000);

    let response = api.ingest(vec![sample_document("doc-1")]).await.unwrap();
    harness.assert_wal_object_exists(&response.batch_id).await;

    let batch = harness.receive_batch().await;
    let build_result = harness.consume_build_and_publish(batch).await;

    assert_eq!(build_result.manifest.version_id, 1);
    harness.assert_manifest_exists(1).await;
    harness.assert_head_points_to(1).await;
}

#[tokio::test]
async fn publish_step_uses_original_build_artifacts_instead_of_rebuilding_documents() {
    let harness = LocalstackHarness::new("publish-original-build").await;
    let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
        harness.bucket.clone(),
        harness.s3.clone(),
    ));
    let queue = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());
    let api = ltsearch::write::WriteApi::new(wal, queue).with_clock(|| 1_700_000_000_000);

    let response = api.ingest(vec![sample_document("doc-1")]).await.unwrap();
    let batch = harness.receive_batch().await;
    let build_request = harness.build_request_from_batch(batch).await;
    let mut build_result = harness.build_from_batch(&build_request);
    let original_document_count = build_result.manifest.document_count;

    assert_eq!(response.accepted_count, original_document_count);

    let artifact_root = harness.latest_build_artifact_root();
    build_result.documents.clear();
    harness
        .publish_build_result(&build_result, artifact_root)
        .await;

    let manifest = harness.read_manifest(1).await;
    assert_eq!(manifest.document_count, original_document_count);
}

impl LocalstackHarness {
    async fn new(name: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let bucket = format!("ltsearch-{name}-{suffix}").to_lowercase();
        let queue_name = format!("ltsearch-{name}-{suffix}");
        let artifact_root =
            std::env::temp_dir().join(format!("ltsearch-build-publish-artifacts-{name}-{suffix}"));

        let credentials = Credentials::new("test", "test", None, None, "localstack");
        let region = Region::new("us-east-1");

        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .region(region.clone())
            .credentials_provider(credentials)
            .endpoint_url("http://localhost:4566")
            .load()
            .await;

        let s3_config = aws_sdk_s3::config::Builder::from(&shared_config)
            .force_path_style(true)
            .build();
        let s3 = S3Client::from_conf(s3_config);
        let sqs = SqsClient::new(&shared_config);

        let queue_url = wait_until_ready(&s3, &sqs, &bucket, &queue_name).await;

        Self {
            artifact_root,
            bucket,
            queue_url,
            s3,
            sqs,
        }
    }

    async fn bucket_exists(&self) -> bool {
        self.s3
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .is_ok()
    }

    async fn queue_exists(&self) -> bool {
        self.sqs
            .get_queue_attributes()
            .queue_url(&self.queue_url)
            .attribute_names(aws_sdk_sqs::types::QueueAttributeName::QueueArn)
            .send()
            .await
            .is_ok()
    }

    async fn read_s3_text(&self, key: &str) -> String {
        let object = self
            .s3
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .unwrap();
        let bytes = object.body.collect().await.unwrap().into_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn receive_one_message_body(&self) -> String {
        let response = self
            .sqs
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(5)
            .send()
            .await
            .unwrap();

        response
            .messages
            .unwrap_or_default()
            .into_iter()
            .next()
            .and_then(|message| message.body)
            .expect("expected one queue message")
    }

    async fn receive_batch(&self) -> ltsearch::write::QueueBatch {
        let response = self
            .sqs
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(5)
            .send()
            .await
            .unwrap();

        let message = response
            .messages
            .unwrap_or_default()
            .into_iter()
            .next()
            .expect("expected one queue message");

        let receipt_handle = message
            .receipt_handle
            .clone()
            .expect("missing receipt handle");
        let body = message.body.clone().expect("missing message body");
        let batch = serde_json::from_str(&body).unwrap();

        self.sqs
            .delete_message()
            .queue_url(&self.queue_url)
            .receipt_handle(receipt_handle)
            .send()
            .await
            .unwrap();

        batch
    }

    fn new_artifact_root(&self) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let path = std::env::temp_dir().join(format!("ltsearch-artifacts-{suffix}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    async fn assert_wal_object_exists(&self, batch_id: &str) {
        let key = format!("wal/2023/11/14/{batch_id}.jsonl");
        let _ = self.read_s3_text(&key).await;
    }

    async fn consume_build_and_publish(
        &self,
        batch: ltsearch::write::QueueBatch,
    ) -> ltsearch::indexing::BuildIndexResult {
        let build_request = self.build_request_from_batch(batch).await;
        let build_result = self.build_from_batch(&build_request);
        self.publish_build_result(&build_result, self.latest_build_artifact_root())
            .await;
        build_result
    }

    fn latest_build_artifact_root(&self) -> std::path::PathBuf {
        self.artifact_root.clone()
    }

    async fn assert_manifest_exists(&self, version_id: u64) {
        let key = format!("index/versions/{version_id}/manifest.json");
        let object = AwsPublishStorage::new(self.bucket.clone(), self.s3.clone())
            .read(&key)
            .await
            .unwrap();
        assert!(object.is_some(), "missing manifest object at {key}");
    }

    async fn read_manifest(&self, version_id: u64) -> ltsearch::models::IndexManifest {
        let key = format!("index/versions/{version_id}/manifest.json");
        let bytes = AwsPublishStorage::new(self.bucket.clone(), self.s3.clone())
            .read(&key)
            .await
            .unwrap()
            .expect("missing manifest object");
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn assert_head_points_to(&self, version_id: u64) {
        let bytes = AwsPublishStorage::new(self.bucket.clone(), self.s3.clone())
            .read(ltsearch::storage::INDEX_HEAD_KEY)
            .await
            .unwrap()
            .expect("missing _head object");
        let head: ltsearch::storage::ManifestHead = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(head.version_id, version_id);
        assert_eq!(
            head.manifest_path,
            format!("index/versions/{version_id}/manifest.json")
        );
    }

    async fn build_request_from_batch(
        &self,
        batch: ltsearch::write::QueueBatch,
    ) -> BuildIndexRequest {
        let wal = ltsearch::write::WriteAheadLog::new(AwsS3WalStorage::new(
            self.bucket.clone(),
            self.s3.clone(),
        ));
        let records = wal.read(&batch.wal_key).await.unwrap();

        BuildIndexRequest {
            version_id: 1,
            created_at: 1_700_000_000_500,
            embedding_dim: 1,
            records,
        }
    }

    fn build_from_batch(&self, request: &BuildIndexRequest) -> BuildIndexResult {
        let artifact_root = self.latest_build_artifact_root();
        let _ = std::fs::remove_dir_all(&artifact_root);
        std::fs::create_dir_all(&artifact_root).unwrap();
        let builder = LocalIndexBuilder::new(&artifact_root, FixedEmbeddingGenerator);
        builder.build(request).unwrap()
    }

    async fn publish_build_result(
        &self,
        build_result: &BuildIndexResult,
        artifact_root: std::path::PathBuf,
    ) {
        let publisher = IndexPublisher::new(
            &artifact_root,
            AwsPublishStorage::new(self.bucket.clone(), self.s3.clone()),
        );
        publisher
            .publish(&PublishRequest {
                manifest: build_result.manifest.clone(),
                expected_current_version: None,
                updated_at: 1_700_000_000_900,
            })
            .await
            .unwrap();
    }
}

async fn wait_until_ready(
    s3: &S3Client,
    sqs: &SqsClient,
    bucket: &str,
    queue_name: &str,
) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut bucket_ready = false;
    let mut queue_url = None;
    let mut last_error = String::new();

    loop {
        if !bucket_ready {
            match s3.create_bucket().bucket(bucket).send().await {
                Ok(_) => bucket_ready = true,
                Err(error) => {
                    if s3.head_bucket().bucket(bucket).send().await.is_ok() {
                        bucket_ready = true;
                    } else {
                        last_error = format!("bucket={error:?}");
                    }
                }
            }
        }

        if queue_url.is_none() {
            match sqs.create_queue().queue_name(queue_name).send().await {
                Ok(queue) => queue_url = queue.queue_url,
                Err(error) => match sqs.get_queue_url().queue_name(queue_name).send().await {
                    Ok(existing) => queue_url = existing.queue_url,
                    Err(_) => {
                        last_error = format!("queue={error:?}");
                    }
                },
            }
        }

        if let (true, Some(queue_url)) = (bucket_ready, queue_url.clone()) {
            return queue_url;
        }

        if std::time::Instant::now() >= deadline {
            panic!("LocalStack did not become ready: {last_error}");
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn test_write_api() -> ltsearch::write::WriteApi<TestWalStorage, TestBuildQueue> {
    let wal = ltsearch::write::WriteAheadLog::new(TestWalStorage);
    let queue = TestBuildQueue;
    ltsearch::write::WriteApi::new(wal, queue)
}

fn test_publisher() -> ltsearch::indexing::IndexPublisher<TestPublishStorage> {
    let artifact_root = std::env::temp_dir().join("ltsearch-publisher-smoke");
    let request = test_publish_request();
    std::fs::create_dir_all(artifact_root.join("index/versions/1")).unwrap();
    std::fs::create_dir_all(artifact_root.join("index/versions/1/shards/0.lance")).unwrap();
    std::fs::create_dir_all(artifact_root.join("index/versions/1/shards/0.tantivy")).unwrap();
    std::fs::write(
        artifact_root.join("index/versions/1/shards/0.lance/data.bin"),
        b"lance",
    )
    .unwrap();
    std::fs::write(
        artifact_root.join("index/versions/1/shards/0.tantivy/meta.json"),
        b"tantivy",
    )
    .unwrap();
    std::fs::write(
        artifact_root.join("index/versions/1/manifest.json"),
        serde_json::to_vec_pretty(&request.manifest).unwrap(),
    )
    .unwrap();
    ltsearch::indexing::IndexPublisher::new(artifact_root, TestPublishStorage)
}

fn test_publish_request() -> ltsearch::indexing::PublishRequest {
    ltsearch::indexing::PublishRequest {
        manifest: ltsearch::models::IndexManifest {
            version_id: 1,
            created_at: 1_700_000_000_000,
            embedding_dim: 1,
            document_count: 0,
            num_shards: 1,
            shards: vec![ltsearch::models::ShardManifest {
                shard_id: 0,
                document_count: 0,
                lance_path: "s3://bucket/index/versions/1/shards/0.lance".into(),
                tantivy_path: "s3://bucket/index/versions/1/shards/0.tantivy".into(),
            }],
        },
        expected_current_version: None,
        updated_at: 1_700_000_000_100,
    }
}

#[derive(Clone)]
struct TestWalStorage;

#[async_trait]
impl ltsearch::write::WalStorage for TestWalStorage {
    async fn append(&self, _key: &str, _bytes: &[u8]) -> Result<(), ltsearch::error::IngestError> {
        Ok(())
    }

    async fn read(&self, _key: &str) -> Result<Vec<u8>, ltsearch::error::IngestError> {
        Ok(Vec::new())
    }
}

#[derive(Clone)]
struct TestBuildQueue;

#[async_trait]
impl ltsearch::write::BuildQueue for TestBuildQueue {
    async fn enqueue(
        &self,
        _batch: ltsearch::write::QueueBatch,
    ) -> Result<(), ltsearch::error::IngestError> {
        Ok(())
    }
}

struct FixedEmbeddingGenerator;

impl EmbeddingGenerator for FixedEmbeddingGenerator {
    fn generate(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(vec![1.0])
    }
}

fn sample_document(doc_id: &str) -> ltsearch::models::Document {
    ltsearch::models::Document {
        doc_id: doc_id.into(),
        text: format!("document {doc_id}"),
        embedding: None,
        metadata: std::collections::HashMap::new(),
        timestamp: 1_700_000_000_000,
    }
}

struct MockS3Server {
    endpoint_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockS3Server {
    fn start(responses: Vec<MockHttpResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded_requests = Arc::clone(&requests);

        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request_line = read_http_request_line(&mut stream);
                recorded_requests.lock().unwrap().push(request_line);
                stream.write_all(&response.to_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        Self {
            endpoint_url: format!("http://{address}"),
            requests,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> Vec<String> {
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
        Arc::try_unwrap(self.requests)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

struct MockHttpResponse {
    status_line: &'static str,
    body: &'static str,
}

impl MockHttpResponse {
    fn access_denied() -> Self {
        Self {
            status_line: "HTTP/1.1 403 Forbidden",
            body: "<Error><Code>AccessDenied</Code><Message>denied</Message></Error>",
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        format!(
            "{}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/xml\r\n\r\n{}",
            self.status_line,
            self.body.len(),
            self.body
        )
        .into_bytes()
    }
}

fn read_http_request_line(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = [0_u8; 4096];
    let mut request = Vec::new();

    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let first_line = request
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default()
        .strip_suffix(b"\r")
        .unwrap_or_default();
    let request_line = String::from_utf8(first_line.to_vec()).unwrap();
    let mut parts = request_line.split_whitespace();

    format!(
        "{} {}",
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default()
    )
}

async fn s3_client_for_endpoint(endpoint_url: &str) -> S3Client {
    let credentials = Credentials::new("test", "test", None, None, "mock-s3");
    let region = Region::new("us-east-1");
    let shared_config = aws_config::defaults(BehaviorVersion::latest())
        .region(region)
        .credentials_provider(credentials)
        .retry_config(RetryConfig::disabled())
        .endpoint_url(endpoint_url)
        .load()
        .await;

    let s3_config = aws_sdk_s3::config::Builder::from(&shared_config)
        .force_path_style(true)
        .build();
    S3Client::from_conf(s3_config)
}

#[derive(Clone)]
struct TestPublishStorage;

#[async_trait]
impl ltsearch::indexing::PublishStorage for TestPublishStorage {
    async fn upload_directory(
        &self,
        _key: &str,
        _source: &std::path::Path,
    ) -> Result<(), ltsearch::error::PublishError> {
        Ok(())
    }

    async fn upload_file(
        &self,
        _key: &str,
        _source: &std::path::Path,
    ) -> Result<(), ltsearch::error::PublishError> {
        Ok(())
    }

    async fn read(&self, _key: &str) -> Result<Option<Vec<u8>>, ltsearch::error::PublishError> {
        Ok(None)
    }

    async fn compare_and_swap(
        &self,
        _key: &str,
        _expected: Option<&[u8]>,
        _new_value: &[u8],
    ) -> Result<bool, ltsearch::error::PublishError> {
        Ok(true)
    }
}
