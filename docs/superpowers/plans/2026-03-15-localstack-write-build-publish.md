# LocalStack Write-Build-Publish Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add LocalStack-backed end-to-end integration coverage for the write-build-publish flow using real S3 and SQS interactions without implementing Lambda in this issue.

**Architecture:** Refactor the write/build/publish storage and queue traits to async so AWS SDK adapters can be implemented naturally. Then add only the smallest S3/SQS adapter layer needed to talk to LocalStack, while keeping queue consumption orchestration in the integration test as a one-shot harness that models the future SQS consumer Lambda without turning it into production runtime code.

**Tech Stack:** Rust, `tokio`, `aws-config`, `aws-sdk-s3`, `aws-sdk-sqs`, LocalStack, Docker Compose, existing LTSearch write/indexing traits

---

## File Structure

- Create: `src/adapters/s3_wal.rs` - LocalStack/AWS-backed `WalStorage`
- Create: `src/adapters/sqs_build_queue.rs` - LocalStack/AWS-backed `BuildQueue`
- Create: `src/adapters/s3_publish.rs` - LocalStack/AWS-backed `PublishStorage`
- Modify: `src/adapters/mod.rs` - export the new adapter modules
- Modify: `src/write/wal.rs` - convert `WalStorage` and `WriteAheadLog` to async
- Modify: `src/write/api.rs` - convert `BuildQueue` and `WriteApi` ingest/delete flow to async
- Modify: `src/indexing/publisher.rs` - convert `PublishStorage` and `IndexPublisher` publish/rollback flow to async
- Modify: `Cargo.toml` - add AWS SDK dependencies exactly once
- Create: `tests/write_build_publish_test.rs` - LocalStack integration tests and one-shot consumer harness
- Create: `docker-compose.localstack.yml` - LocalStack S3/SQS services for local integration runs

## Chunk 1: Bootstrap, dependencies, and readiness

### Task 1: Add LocalStack bootstrap smoke coverage and SDK dependencies

**Files:**
- Modify: `Cargo.toml`
- Create: `tests/write_build_publish_test.rs`
- Create: `docker-compose.localstack.yml`

- [ ] **Step 1: Write the failing LocalStack readiness smoke test**

```rust
#[tokio::test]
async fn localstack_smoke_test_can_create_bucket_and_queue() {
    let harness = LocalstackHarness::new("bootstrap-smoke").await;
    assert!(harness.bucket_exists().await);
    assert!(harness.queue_exists().await);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test localstack_smoke_test_can_create_bucket_and_queue -- --exact --nocapture`
Expected: FAIL with missing test file or missing `LocalstackHarness`.

- [ ] **Step 3: Add AWS dependencies in `Cargo.toml`**

```toml
aws-config = "1"
aws-sdk-s3 = "1"
aws-sdk-sqs = "1"
```

- [ ] **Step 4: Add `docker-compose.localstack.yml`**

```yaml
services:
  localstack:
    image: localstack/localstack:3.8
    ports:
      - "4566:4566"
    environment:
      SERVICES: s3,sqs
      AWS_DEFAULT_REGION: us-east-1
```

- [ ] **Step 5: Start LocalStack**

Run: `docker compose -f docker-compose.localstack.yml up -d`
Expected: container starts.

- [ ] **Step 6: Add the minimal smoke-test harness and readiness check**

In `tests/write_build_publish_test.rs`, add only:

- `LocalstackHarness { bucket, queue_url, s3, sqs }`
- AWS SDK config using:
  - endpoint `http://localhost:4566`
  - fixed region `us-east-1`
  - static test credentials
  - path-style S3
- a readiness helper that repeatedly attempts real S3 `create_bucket` and SQS `create_queue` calls until LocalStack responds or times out
- `bucket_exists()` and `queue_exists()` helpers that confirm those resources exist

- [ ] **Step 7: Run the smoke test to verify it passes**

Run: `cargo test --test write_build_publish_test localstack_smoke_test_can_create_bucket_and_queue -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 8: Commit the LocalStack bootstrap**

```bash
git add Cargo.toml tests/write_build_publish_test.rs docker-compose.localstack.yml
git commit -m "test: add localstack bootstrap smoke coverage"
```

## Chunk 2: Async trait refactor and adapter scaffolding

### Task 2: Refactor the write-path traits to async with TDD

**Files:**
- Modify: `src/write/wal.rs`
- Modify: `src/write/api.rs`
- Modify: `tests/write_api_test.rs`
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write a failing async smoke test for `WriteApi::ingest` in `tests/write_build_publish_test.rs`**

```rust
#[tokio::test]
async fn write_api_ingest_can_be_awaited_in_integration_context() {
    let api = test_write_api();
    let response = api.ingest(vec![sample_document("doc-1")]).await.unwrap();
    assert_eq!(response.accepted_count, 1);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test write_api_ingest_can_be_awaited_in_integration_context -- --exact --nocapture`
Expected: FAIL because `ingest` is still synchronous and/or traits are not async.

- [ ] **Step 3: Convert `WalStorage`, `WriteAheadLog`, `BuildQueue`, and `WriteApi` to async**

Use `async-trait` if needed. Update direct callers only as required by compilation.

- [ ] **Step 4: Run the async smoke test to verify it passes**

Run: `cargo test --test write_build_publish_test write_api_ingest_can_be_awaited_in_integration_context -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 5: Run `cargo test --test write_api_test` and fix the async mechanical fallout**

Run: `cargo test --test write_api_test`
Expected: FAIL initially if test call sites still need `.await`; PASS after updates.

- [ ] **Step 6: Commit the async write-path refactor**

```bash
git add src/write/wal.rs src/write/api.rs tests/write_api_test.rs tests/write_build_publish_test.rs Cargo.toml
git commit -m "refactor: make write path traits async"
```

### Task 3: Refactor publish storage and publisher flow to async with TDD

**Files:**
- Modify: `src/indexing/publisher.rs`
- Modify: `tests/publisher_test.rs`
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write a failing async smoke test for `IndexPublisher::publish`**

```rust
#[tokio::test]
async fn index_publisher_publish_can_be_awaited_in_integration_context() {
    let publisher = test_publisher();
    let request = test_publish_request();
    let _ = publisher.publish(&request).await;
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test index_publisher_publish_can_be_awaited_in_integration_context -- --exact --nocapture`
Expected: FAIL because `publish` and `PublishStorage` are still synchronous.

- [ ] **Step 3: Convert `PublishStorage`, `IndexPublisher::publish`, and `IndexPublisher::rollback` to async**

Update direct test callers as needed.

- [ ] **Step 4: Run the async smoke test to verify it passes**

Run: `cargo test --test write_build_publish_test index_publisher_publish_can_be_awaited_in_integration_context -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 5: Run `cargo test --test publisher_test` and fix the async mechanical fallout**

Run: `cargo test --test publisher_test`
Expected: FAIL initially if test call sites still need `.await`; PASS after updates.

- [ ] **Step 6: Commit the async publisher refactor**

```bash
git add src/indexing/publisher.rs tests/publisher_test.rs tests/write_build_publish_test.rs
git commit -m "refactor: make publisher storage async"
```

### Task 4: Add failing adapter construction test and minimal module scaffolding

**Files:**
- Modify: `src/adapters/mod.rs`
- Create: `src/adapters/s3_wal.rs`
- Create: `src/adapters/sqs_build_queue.rs`
- Create: `src/adapters/s3_publish.rs`
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write the failing adapter construction test**

```rust
use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;

#[tokio::test]
async fn localstack_harness_can_construct_all_adapter_types() {
    let harness = LocalstackHarness::new("adapter-constructors").await;
    let _ = AwsS3WalStorage::new(harness.bucket.clone(), harness.s3.clone());
    let _ = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());
    let _ = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test localstack_harness_can_construct_all_adapter_types -- --exact --nocapture`
Expected: FAIL with missing adapter modules or missing constructors.

- [ ] **Step 3: Add adapter module exports in `src/adapters/mod.rs`**

```rust
pub mod aws_runtime;
pub mod s3_publish;
pub mod s3_wal;
pub mod sqs_build_queue;
```

- [ ] **Step 4: Add only the struct definitions and constructors in the three adapter files**

No trait methods yet. Just `new(...)` and stored fields.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --test write_build_publish_test localstack_harness_can_construct_all_adapter_types -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit the adapter scaffolding**

```bash
git add src/adapters/mod.rs src/adapters/s3_wal.rs src/adapters/sqs_build_queue.rs src/adapters/s3_publish.rs tests/write_build_publish_test.rs
git commit -m "feat: scaffold localstack aws adapters"
```

## Chunk 3: S3 WAL and SQS queue adapters

### Task 3: Add failing S3 WAL tests and implement append/read semantics

**Files:**
- Modify: `src/adapters/s3_wal.rs`
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write a failing test for first append creating a WAL object**

```rust
#[tokio::test]
async fn s3_wal_storage_first_append_creates_object() {
    let harness = LocalstackHarness::new("s3-wal-create").await;
    let wal = WriteAheadLog::new(AwsS3WalStorage::new(harness.bucket.clone(), harness.s3.clone()));

    wal.append_bytes("wal/2023/11/14/batch-test.jsonl", b"line-1\n").unwrap();

    let stored = harness.read_s3_text("wal/2023/11/14/batch-test.jsonl").await;
    assert_eq!(stored, "line-1\n");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test s3_wal_storage_first_append_creates_object -- --exact --nocapture`
Expected: FAIL with `WalStorage::append` not implemented.

- [ ] **Step 3: Implement only `append` create-on-first-write behavior**

In `src/adapters/s3_wal.rs`, implement `append` so that:

- missing object on first write creates the object
- existing object bytes are loaded, concatenated, and overwritten on subsequent writes
- only true SDK failures become `IngestError::Operation`

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test write_build_publish_test s3_wal_storage_first_append_creates_object -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 5: Write a failing test for reading back WAL records**

```rust
#[tokio::test]
async fn s3_wal_storage_round_trips_jsonl_bytes_against_localstack() {
    let harness = LocalstackHarness::new("s3-wal-roundtrip").await;
    let wal = WriteAheadLog::new(AwsS3WalStorage::new(harness.bucket.clone(), harness.s3.clone()));
    let key = "wal/2023/11/14/batch-test.jsonl";

    wal.append_bytes(key, b"{\"event_id\":\"e1\",\"doc_id\":\"doc-1\",\"op\":\"delete\",\"document\":null,\"timestamp\":1700000000000}\n")
        .unwrap();

    let records = wal.read(key).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].doc_id, "doc-1");
}
```

- [ ] **Step 6: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test s3_wal_storage_round_trips_jsonl_bytes_against_localstack -- --exact --nocapture`
Expected: FAIL with `WalStorage::read` not implemented.

- [ ] **Step 7: Implement only `read`**

In `src/adapters/s3_wal.rs`, implement `read` by returning raw object bytes so `WriteAheadLog::read(...)` can parse them into `WalRecord` values.

- [ ] **Step 8: Run the test to verify it passes**

Run: `cargo test --test write_build_publish_test s3_wal_storage_round_trips_jsonl_bytes_against_localstack -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 9: Commit the S3 WAL adapter**

```bash
git add src/adapters/s3_wal.rs tests/write_build_publish_test.rs
git commit -m "feat: add s3-backed wal adapter"
```

### Task 4: Add a failing SQS queue adapter test and implement send semantics

**Files:**
- Modify: `src/adapters/sqs_build_queue.rs`
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write a failing test for SQS batch enqueue**

```rust
#[tokio::test]
async fn sqs_build_queue_enqueues_batch_metadata_against_localstack() {
    let harness = LocalstackHarness::new("sqs-build-queue").await;
    let queue = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());

    queue.enqueue(QueueBatch {
        batch_id: "batch-123".into(),
        wal_key: "wal/2023/11/14/batch-123.jsonl".into(),
        accepted_count: 2,
        wal_event_ids: vec!["batch-123-000001".into(), "batch-123-000002".into()],
    }).unwrap();

    let message = harness.receive_one_message_body().await;
    assert!(message.contains("batch-123"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test sqs_build_queue_enqueues_batch_metadata_against_localstack -- --exact --nocapture`
Expected: FAIL with `BuildQueue::enqueue` not implemented.

- [ ] **Step 3: Implement only `enqueue`**

In `src/adapters/sqs_build_queue.rs`, serialize `QueueBatch` to JSON and send it as one SQS message.

- [ ] **Step 4: Add the smallest receive helper needed for the test**

In `tests/write_build_publish_test.rs`, implement only a helper that receives one SQS message body without decoding it.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --test write_build_publish_test sqs_build_queue_enqueues_batch_metadata_against_localstack -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit the SQS queue adapter**

```bash
git add src/adapters/sqs_build_queue.rs tests/write_build_publish_test.rs
git commit -m "feat: add sqs-backed build queue adapter"
```

## Chunk 4: Publish storage and one-shot consumer flow

### Task 5: Add failing publish-storage tests and implement upload/read/CAS behavior

**Files:**
- Modify: `src/adapters/s3_publish.rs`
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write a failing test for publish storage file upload and read**

```rust
#[tokio::test]
async fn publish_storage_uploads_and_reads_manifest_bytes() {
    let harness = LocalstackHarness::new("publish-storage-read").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());
    let artifact_root = harness.new_artifact_root();
    let manifest_path = artifact_root.join("index/versions/7/manifest.json");
    std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    std::fs::write(&manifest_path, b"{}\n").unwrap();

    storage.upload_file("index/versions/7/manifest.json", &manifest_path).unwrap();
    assert!(storage.read("index/versions/7/manifest.json").unwrap().is_some());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test publish_storage_uploads_and_reads_manifest_bytes -- --exact --nocapture`
Expected: FAIL because `upload_file` or `read` is not implemented.

- [ ] **Step 3: Implement only `upload_file` and `read`**

In `src/adapters/s3_publish.rs`, add those two methods first.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test write_build_publish_test publish_storage_uploads_and_reads_manifest_bytes -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 5: Write a failing test for publish storage compare-and-swap success**

```rust
#[tokio::test]
async fn publish_storage_compare_and_swap_updates_head_when_expected_matches() {
    let harness = LocalstackHarness::new("publish-storage-cas").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    let swapped = storage.compare_and_swap("index/_head", None, b"{}").unwrap();
    assert!(swapped);
}
```

- [ ] **Step 6: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test publish_storage_compare_and_swap_updates_head_when_expected_matches -- --exact --nocapture`
Expected: FAIL because `compare_and_swap` is not implemented.

- [ ] **Step 7: Implement only success-path compare-and-swap**

In `src/adapters/s3_publish.rs`, implement LocalStack test-scope byte-compare-put behavior:

- read current object bytes
- compare to `expected`
- write `new_value` only when bytes match

Document in code that this is test-scope integration behavior and not a claim of distributed atomicity.

- [ ] **Step 8: Run the success-path test to verify it passes**

Run: `cargo test --test write_build_publish_test publish_storage_compare_and_swap_updates_head_when_expected_matches -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 9: Write a failing mismatch test for compare-and-swap**

```rust
#[tokio::test]
async fn publish_storage_compare_and_swap_returns_false_when_expected_mismatches() {
    let harness = LocalstackHarness::new("publish-storage-cas-mismatch").await;
    let storage = AwsPublishStorage::new(harness.bucket.clone(), harness.s3.clone());

    assert!(storage.compare_and_swap("index/_head", None, b"old").unwrap());
    assert!(!storage.compare_and_swap("index/_head", Some(b"different"), b"new").unwrap());
}
```

- [ ] **Step 10: Run the mismatch test to verify it fails**

Run: `cargo test --test write_build_publish_test publish_storage_compare_and_swap_returns_false_when_expected_mismatches -- --exact --nocapture`
Expected: FAIL because mismatch handling is not implemented correctly yet.

- [ ] **Step 11: Implement mismatch-no-op semantics**

Keep the stored `_head` unchanged and return `false` when `expected` does not match current bytes.

- [ ] **Step 12: Run both compare-and-swap tests**

Run: `cargo test --test write_build_publish_test publish_storage_compare_and_swap -- --nocapture`
Expected: PASS.

- [ ] **Step 13: Commit the publish storage adapter**

```bash
git add src/adapters/s3_publish.rs tests/write_build_publish_test.rs
git commit -m "feat: add s3-backed publish storage adapter"
```

### Task 6: Add a failing queue decode test and implement one-shot message decoding

**Files:**
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write a failing test for receiving and decoding one queue batch**

```rust
#[tokio::test]
async fn localstack_harness_receives_and_decodes_one_queue_batch() {
    let harness = LocalstackHarness::new("decode-batch").await;
    AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone())
        .enqueue(QueueBatch {
            batch_id: "batch-xyz".into(),
            wal_key: "wal/2023/11/14/batch-xyz.jsonl".into(),
            accepted_count: 1,
            wal_event_ids: vec!["batch-xyz-000001".into()],
        })
        .unwrap();

    let batch = harness.receive_batch().await;
    assert_eq!(batch.batch_id, "batch-xyz");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test localstack_harness_receives_and_decodes_one_queue_batch -- --exact --nocapture`
Expected: FAIL because `receive_batch()` does not exist.

- [ ] **Step 3: Implement only `receive_batch()`**

In `tests/write_build_publish_test.rs`, add a helper that:

- reads one SQS message body
- deserializes it into `QueueBatch`
- deletes the message from the queue after successful decode

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test write_build_publish_test localstack_harness_receives_and_decodes_one_queue_batch -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit the queue decode helper**

```bash
git add tests/write_build_publish_test.rs
git commit -m "test: add localstack queue batch decode helper"
```

### Task 7: Add the failing end-to-end integration test and implement the one-shot consumer harness in strict slices

**Files:**
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write the failing end-to-end integration test first**

```rust
#[tokio::test]
async fn write_build_publish_flow_runs_end_to_end_against_localstack() {
    let harness = LocalstackHarness::new("write-build-publish").await;
    let wal = WriteAheadLog::new(AwsS3WalStorage::new(harness.bucket.clone(), harness.s3.clone()));
    let queue = AwsSqsBuildQueue::new(harness.queue_url.clone(), harness.sqs.clone());
    let api = WriteApi::new(wal, queue).with_clock(|| 1_700_000_000_000);

    let response = api.ingest(vec![sample_document("doc-1")]).unwrap();
    harness.assert_wal_object_exists(&response.batch_id).await;

    let batch = harness.receive_batch().await;
    let build_result = harness.consume_build_and_publish(batch).await;

    assert_eq!(build_result.manifest.version_id, 1);
    harness.assert_manifest_exists(1).await;
    harness.assert_head_points_to(1).await;
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test write_build_publish_test write_build_publish_flow_runs_end_to_end_against_localstack -- --exact --nocapture`
Expected: FAIL with missing `assert_wal_object_exists` or missing `consume_build_and_publish`.

- [ ] **Step 3: Implement only `assert_wal_object_exists()`**

- [ ] **Step 4: Run the same test again**

Run: `cargo test --test write_build_publish_test write_build_publish_flow_runs_end_to_end_against_localstack -- --exact --nocapture`
Expected: FAIL with missing `consume_build_and_publish`.

- [ ] **Step 5: Implement only `build_request_from_batch(...)`**

Use fixed values:

- `version_id = 1`
- `created_at = 1_700_000_000_500`
- `embedding_dim = 1`

- [ ] **Step 6: Run the same test again**

Run: `cargo test --test write_build_publish_test write_build_publish_flow_runs_end_to_end_against_localstack -- --exact --nocapture`
Expected: FAIL with missing `build_from_batch(...)` or missing `consume_build_and_publish`.

- [ ] **Step 7: Implement only `build_from_batch(...)`**

Add:

- deterministic test embedding generator returning `vec![1.0]`
- local artifact root creation
- `LocalIndexBuilder::build(...)`

- [ ] **Step 8: Run the same test again**

Run: `cargo test --test write_build_publish_test write_build_publish_flow_runs_end_to_end_against_localstack -- --exact --nocapture`
Expected: FAIL with missing `publish_build_result(...)` or missing `consume_build_and_publish(...)`.

- [ ] **Step 9: Implement only `publish_build_result(...)`**

Add:

- `AwsPublishStorage`
- `IndexPublisher::publish(...)`

- [ ] **Step 10: Run the same test again**

Run: `cargo test --test write_build_publish_test write_build_publish_flow_runs_end_to_end_against_localstack -- --exact --nocapture`
Expected: FAIL with missing `consume_build_and_publish(...)`, `assert_manifest_exists(...)`, or `assert_head_points_to(...)`.

- [ ] **Step 11: Implement only `consume_build_and_publish()` as a thin composition of `build_request_from_batch(...)`, `build_from_batch(...)`, and `publish_build_result(...)`**

- [ ] **Step 12: Run the same test again**

Run: `cargo test --test write_build_publish_test write_build_publish_flow_runs_end_to_end_against_localstack -- --exact --nocapture`
Expected: FAIL with missing `assert_manifest_exists(...)` or `assert_head_points_to(...)`.

- [ ] **Step 13: Implement only `assert_manifest_exists()`**

- [ ] **Step 14: Run the same test again**

Run: `cargo test --test write_build_publish_test write_build_publish_flow_runs_end_to_end_against_localstack -- --exact --nocapture`
Expected: FAIL with missing `assert_head_points_to(...)`.

- [ ] **Step 15: Implement only `assert_head_points_to()`**

- [ ] **Step 16: Run the same test to verify it passes**

Run: `cargo test --test write_build_publish_test write_build_publish_flow_runs_end_to_end_against_localstack -- --exact --nocapture`
Expected: PASS.

- [ ] **Step 17: Commit the end-to-end coverage**

```bash
git add tests/write_build_publish_test.rs
git commit -m "test: add write-build-publish localstack flow"
```

## Chunk 5: Regression verification

### Task 8: Run deterministic verification commands

**Files:**
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Run the full LocalStack integration test file**

Run: `cargo test --test write_build_publish_test -- --nocapture`
Expected: PASS.

- [ ] **Step 2: Tighten any stage-specific assertions required to keep failures localized**

Ensure the integration file still isolates:

- WAL object presence
- queue message receipt
- build result creation
- `_head` activation

- [ ] **Step 3: Re-run the LocalStack integration test file**

Run: `cargo test --test write_build_publish_test -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Run the required regression suite**

Run: `cargo test --test wal_test --test write_api_test --test index_builder_test --test publisher_test`
Expected: PASS.

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit the final verification state**

```bash
git add tests/write_build_publish_test.rs
git commit -m "test: finalize localstack integration verification"
```
