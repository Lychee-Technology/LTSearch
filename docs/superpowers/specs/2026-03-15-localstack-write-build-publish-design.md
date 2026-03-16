## Title

Moto-backed integration coverage for the write-build-publish flow

## Context

Issue `#21` asks for end-to-end integration coverage of the write-build-publish path using S3- and SQS-compatible local infrastructure. The current codebase already has the core domain components needed for this flow:

- `WriteApi` validates requests, appends WAL records, and enqueues batch metadata
- `LocalIndexBuilder` materializes the latest snapshot and writes versioned build artifacts
- `IndexPublisher` uploads artifacts and atomically updates `_head`

What the codebase does not yet have is a real AWS-backed adapter layer. Current tests use in-memory or local filesystem fakes. The environment also does not have the `aws` CLI installed, so test setup cannot depend on shelling out to `aws s3` or `aws sqs` commands. The existing storage and queue traits are also synchronous, while the AWS SDK is async.

## Goal

Add Moto-backed integration coverage that verifies the real infrastructure flow for:

1. WAL append to S3
2. batch enqueue to SQS
3. single-batch consume and processing
4. build artifact generation
5. publish activation through `_head`

This issue does not implement Lambda execution. It validates the infrastructure and orchestration boundaries that a future SQS-triggered Lambda will use.

## Scope

In scope:

- add `docker-compose.moto.yml` for local S3/SQS infrastructure
- add a Moto-backed integration test in `tests/write_build_publish_test.rs`
- add the minimum S3/SQS adapter code needed to exercise the existing write/build/publish components against real infrastructure
- refactor the affected storage and queue traits plus direct callers from sync to async so AWS SDK usage does not rely on runtime-bridging shims
- add a minimal single-message consumer harness used by the integration test
- make setup independent from the external `aws` CLI

Out of scope:

- Lambda handlers
- SAM templates
- a long-running queue worker
- production deployment wiring

## Approaches Considered

### 1. Moto-backed integration with test-owned consumer harness (recommended)

Use Moto for S3 and SQS, add minimal production adapters for S3/SQS traits, and keep the queue consumer orchestration inside the integration test harness.

Pros:

- matches issue `#21` acceptance criteria closely
- keeps scope focused on real infrastructure plus existing domain flow
- avoids expanding the issue into runtime worker architecture
- establishes the exact flow a future SQS consumer Lambda will reuse

Cons:

- the consumer harness lives in test code rather than production code
- some orchestration is verified indirectly through the test instead of through a reusable runtime entry point

### 2. Moto with a production `process_batch(...)` orchestration function

Add a reusable production coordinator that takes batch metadata and performs WAL read, build, and publish. The test would read from SQS and call this function.

Pros:

- cleaner reuse path for future Lambda wiring
- smaller integration test body

Cons:

- expands issue `#21` into new production orchestration API design
- adds more surface area than needed for current acceptance

### 3. Full local worker process

Add a long-running local consumer that continuously polls SQS and performs build/publish work, then verify that worker in integration tests.

Pros:

- closest to eventual deployed topology

Cons:

- too large for the issue
- harder to test deterministically
- mixes infrastructure validation with runtime lifecycle concerns

## Recommended Design

Adopt approach 1.

Add a real-infrastructure integration test that uses Moto for S3 and SQS, while keeping the message-consume/build/publish orchestration as a test-local harness. Introduce only the smallest production adapters needed to let existing trait-based components talk to Moto.

This preserves clear boundaries:

- production code owns infrastructure adapters
- test code owns one-shot orchestration for validation
- future Lambda work can wrap the same adapter and orchestration concepts without rethinking domain behavior

## Architecture

### Production-facing adapters

Add small adapters that implement existing async traits:

- `WalStorage` backed by S3 object reads/appends
- `BuildQueue` backed by SQS `SendMessage`
- `PublishStorage` backed by S3 object reads/writes/CAS-style head update

The adapter behavior must stay narrow and trait-shaped. No new domain logic belongs here.

### Async trait strategy

The existing traits are synchronous, while the AWS SDK is async. Per the final user decision for this issue, the affected traits and direct callers should be refactored to async instead of using runtime-bridging shims.

That refactor is intentionally scoped to the write/build/publish path used by this issue:

- `WalStorage`
- `BuildQueue`
- `PublishStorage`
- `WriteAheadLog`
- `WriteApi`
- `IndexPublisher`

The goal is to make Moto-backed adapters natural and correct, while keeping the rest of the codebase unchanged unless compilation requires a small mechanical update.

### Test harness

`tests/write_build_publish_test.rs` owns a minimal one-shot consumer harness:

1. provision unique bucket/queue names against Moto
2. build real S3 and SQS clients against `http://localhost:5000`
3. run async `WriteApi::ingest(...)`
4. assert WAL bytes are present in S3
5. receive exactly one queue message from SQS
6. decode batch metadata from that message
7. read WAL records from S3
8. build a snapshot with `LocalIndexBuilder`
9. publish artifacts with async `IndexPublisher`
10. validate published artifacts and `_head`

This harness is intentionally single-batch and single-test scoped. It is not a reusable worker.

## Data Flow

1. Test writes sample documents through `WriteApi`
2. `WriteApi` validates request models
3. `WriteApi` asynchronously appends newline-delimited WAL records to S3
4. `WriteApi` asynchronously enqueues batch metadata to SQS
5. Test harness receives queue message from SQS
6. Test harness resolves the WAL object key from the batch metadata
7. Test harness loads WAL records from S3 and forms `BuildIndexRequest`
8. `LocalIndexBuilder` materializes the latest snapshot and writes versioned local artifacts
9. `IndexPublisher` uploads versioned artifacts to S3 and atomically updates `_head`
10. Test validates manifest object, artifact objects, and `_head` contents in S3

## Error Handling

Failures should identify the stage clearly so the integration test is diagnosable:

- WAL stage: failure writing or reading the S3 WAL object
- queue stage: failure sending or receiving the SQS message
- decode stage: failure parsing batch metadata or WAL records
- build stage: failure materializing the snapshot or generating artifacts
- publish stage: failure uploading artifacts or updating `_head`

The integration test should surface stage-specific assertions rather than a single opaque end-to-end failure.

## Test Design

### Primary passing test

Add one main end-to-end test that proves:

- documents are accepted through `WriteApi`
- WAL lands in Moto S3 before queue-driven processing continues
- batch metadata reaches Moto SQS
- a single queue message can drive build and publish successfully
- published version artifacts exist in S3
- `_head` points at the newly published manifest

### Failure visibility

The test harness should keep its assertions segmented so failures reveal the broken stage immediately. For example:

- missing WAL object after ingest implies write path breakage
- missing queue message implies enqueue breakage
- missing manifest or `_head` after processing implies build or publish breakage

This satisfies the issue requirement that failures clearly identify WAL, queue, build, or publish stage.

## Tooling and Environment

### Moto compose file

Add `docker-compose.moto.yml` configured only for the services needed here:

- `s3`
- `sqs`

### Resource bootstrap

Bootstrap bucket and queue inside Rust test setup via AWS SDK clients. Do not require `aws` CLI commands, because the current environment does not provide that tool.

### Credentials and endpointing

Use the standard Moto defaults in test configuration:

- endpoint: `http://localhost:5000`
- region: fixed test region
- static test credentials
- path-style S3 access if required by SDK configuration

## Files

Expected new files:

- `tests/write_build_publish_test.rs`
- `docker-compose.moto.yml`

Expected updated areas:

- adapter code under `src/` for minimal S3/SQS-backed trait implementations
- async trait and caller updates in the write/build/publish path
- module exports only where the integration test needs to construct those adapters directly

## Verification Plan

Primary verification for this issue:

1. `docker compose -f docker-compose.moto.yml up -d`
2. `cargo test --test write_build_publish_test -- --nocapture`

Regression verification after implementation:

1. `cargo test --test wal_test --test write_api_test --test index_builder_test --test publisher_test`
2. optionally `cargo test` if runtime allows

## Future Follow-up

This design intentionally prepares for a future SQS-triggered Lambda without implementing it now. Once Lambda work begins, the future handler should reuse the same infrastructure boundaries proven here:

- SQS message carries batch metadata
- batch processor reads WAL from S3
- processor performs build and publish

That future work can choose whether to lift parts of the test harness into a production `process_batch` entry point.
