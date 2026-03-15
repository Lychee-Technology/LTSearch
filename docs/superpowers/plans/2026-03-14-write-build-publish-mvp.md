# Write Build Publish MVP Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the durable write path, snapshot builder, publish activation flow, and supporting integration tests for the MVP index lifecycle.

**Architecture:** Keep writes append-only and durable by treating WAL records in S3 as the source of truth before asynchronous build work begins. Separate snapshot materialization from publish activation so build correctness and atomic version switching are tested independently.

**Tech Stack:** Rust, `tokio`, `serde_json`, `aws-sdk-s3`, `aws-sdk-sqs`, LocalStack, `mockall`, `lancedb`, `tantivy`

---

## Scope

Prerequisite: complete `docs/superpowers/plans/2026-03-14-query-core-mvp.md` first so shared models, errors, and manifest types already exist.

- `#16` Implement IndexBuilder snapshot materialization and dual-index build
- `#17` Implement S3-backed WAL segment storage
- `#18` Implement WriteAPI ingest and delete flow
- `#19` Implement publish flow for IndexManifest and _head activation
- `#20` Implement rollback and version conflict handling
- `#21` Add LocalStack integration tests for write-build-publish flow

## File Structure

- Create: `src/write/mod.rs`
- Create: `src/write/wal.rs`
- Create: `src/write/api.rs`
- Create: `src/indexing/mod.rs`
- Create: `src/indexing/builder.rs`
- Create: `src/indexing/publisher.rs`
- Create: `tests/wal_test.rs`
- Create: `tests/write_api_test.rs`
- Create: `tests/index_builder_test.rs`
- Create: `tests/publisher_test.rs`
- Create: `tests/write_build_publish_test.rs`
- Create: `docker-compose.localstack.yml`

## Chunk 1: Durable write path

### Task 1: Implement WAL storage (`#17`)

**Files:**
- Create: `src/write/mod.rs`
- Create: `src/write/wal.rs`
- Create: `tests/wal_test.rs`

- [ ] **Step 1: Write failing tests in `tests/wal_test.rs` for path generation, append semantics, and invalid record rejection**
- [ ] **Step 2: Run WAL tests to verify failure**

Run: `cargo test --test wal_test`
Expected: FAIL with missing WAL module.

- [ ] **Step 3: Implement minimal segment path + JSONL append behavior**
- [ ] **Step 4: Re-run WAL tests**

Run: `cargo test --test wal_test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/write/mod.rs src/write/wal.rs tests/wal_test.rs
git commit -m "feat: add s3-backed wal storage"
```

### Task 2: Implement WriteAPI (`#18`)

**Files:**
- Create: `src/write/api.rs`
- Create: `tests/write_api_test.rs`

- [ ] **Step 1: Write failing tests in `tests/write_api_test.rs` for ingest, delete, and WAL-before-enqueue ordering using fake queue and fake WAL adapters**
- [ ] **Step 2: Run WriteAPI tests to verify failure**

Run: `cargo test --test write_api_test`
Expected: FAIL with missing `WriteApi`.

- [ ] **Step 3: Implement minimal `WriteApi::ingest` and `WriteApi::delete`**
- [ ] **Step 4: Re-run WriteAPI tests**

Run: `cargo test --test write_api_test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/write/api.rs tests/write_api_test.rs
git commit -m "feat: add wal-backed write api"
```

## Chunk 2: Snapshot build and publish

### Task 3: Implement snapshot builder (`#16`)

**Files:**
- Create: `src/indexing/mod.rs`
- Create: `src/indexing/builder.rs`
- Create: `tests/index_builder_test.rs`

- [ ] **Step 1: Write failing tests in `tests/index_builder_test.rs` for latest-event-wins snapshot materialization, delete handling, and manifest document-count consistency**
- [ ] **Step 2: Run builder tests to verify failure**

Run: `cargo test --test index_builder_test`
Expected: FAIL with missing `IndexBuilder`.

- [ ] **Step 3: Implement minimal pure snapshot materialization**
- [ ] **Step 4: Add minimal artifact-build coordination for LanceDB and Tantivy outputs**
- [ ] **Step 5: Re-run builder tests**

Run: `cargo test --test index_builder_test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/indexing/mod.rs src/indexing/builder.rs tests/index_builder_test.rs
git commit -m "feat: add index snapshot builder"
```

### Task 4: Implement publish activation (`#19`)

**Files:**
- Create: `src/indexing/publisher.rs`
- Create: `tests/publisher_test.rs`

- [ ] **Step 1: Write failing tests in `tests/publisher_test.rs` for upload order, `_head` conditional update, conflict rejection, and previous-version preservation**
- [ ] **Step 2: Run publisher tests to verify failure**

Run: `cargo test --test publisher_test`
Expected: FAIL with missing publisher module.

- [ ] **Step 3: Implement minimal publish flow**
- [ ] **Step 4: Re-run publisher tests**

Run: `cargo test --test publisher_test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/indexing/publisher.rs tests/publisher_test.rs
git commit -m "feat: add index publish activation flow"
```

### Task 5: Implement rollback support (`#20`)

**Files:**
- Modify: `src/indexing/publisher.rs`
- Modify: `tests/publisher_test.rs`

- [ ] **Step 1: Write the failing rollback test**
- [ ] **Step 2: Run publisher tests to verify rollback failure**

Run: `cargo test rollback_restores_previous_active_version -- --exact`
Expected: FAIL with missing rollback behavior.

- [ ] **Step 3: Implement minimal rollback handling**
- [ ] **Step 4: Re-run publisher tests**

Run: `cargo test --test publisher_test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/indexing/publisher.rs tests/publisher_test.rs
git commit -m "feat: add rollback for published index versions"
```

## Chunk 3: Integration coverage

### Task 6: Add LocalStack coverage (`#21`)

**Files:**
- Create: `tests/write_build_publish_test.rs`
- Create: `docker-compose.localstack.yml`

- [ ] **Step 1: Write failing integration tests for WAL append, SQS enqueue, build, and publish activation**
- [ ] **Step 2: Start LocalStack and run the integration test to verify failure**

Run: `docker compose -f docker-compose.localstack.yml up -d`
Expected: LocalStack starts.

Run: `aws --endpoint-url http://localhost:4566 s3 mb s3://ltsearch-test`
Expected: test bucket created.

Run: `aws --endpoint-url http://localhost:4566 sqs create-queue --queue-name ltsearch-builds`
Expected: test queue created.

Run: `cargo test --test write_build_publish_test -- --nocapture`
Expected: FAIL because the end-to-end flow is not fully wired yet.

- [ ] **Step 3: Add only the minimal harness/configuration needed to make the test pass**
- [ ] **Step 4: Re-run the integration test**

Run: `cargo test --test write_build_publish_test -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add tests/write_build_publish_test.rs docker-compose.localstack.yml
git commit -m "test: add write-build-publish integration coverage"
```

## Verification

- Run: `cargo test --test wal_test --test write_api_test --test index_builder_test --test publisher_test`
- Expected: PASS
- Run: `cargo test --test write_build_publish_test -- --nocapture`
- Expected: PASS
- Run: `cargo test wal_append_happens_before_enqueue -- --exact`
- Expected: PASS
- Run: `cargo test publish_preserves_previous_version_on_conflict -- --exact`
- Expected: PASS
- Run: `cargo test`
- Expected: PASS

Plan complete and saved to `docs/superpowers/plans/2026-03-14-write-build-publish-mvp.md`. Ready to execute?
