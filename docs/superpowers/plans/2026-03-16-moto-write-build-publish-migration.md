# Moto Write-Build-Publish Migration Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace LocalStack with Moto for the write-build-publish integration path, CI setup, and current operational docs without changing the underlying AWS SDK adapter semantics.

**Architecture:** Keep the existing async S3/SQS adapter layer and end-to-end integration harness intact, but swap the backing mock AWS service from LocalStack to Moto. Update the compose file, CI workflow, test harness names/endpoints, and doc references so the repository consistently describes and runs against Moto.

**Tech Stack:** Rust, `tokio`, `aws-config`, `aws-sdk-s3`, `aws-sdk-sqs`, Python `unittest`, Docker Compose, `motoserver/moto`, Markdown docs

---

## File Structure

- Create: `docker-compose.moto.yml` - Moto S3/SQS service definition for local and CI integration runs
- Delete: `docker-compose.localstack.yml` - superseded LocalStack compose file
- Modify: `.github/workflows/ci.yml` - start/stop Moto instead of LocalStack in the test job
- Modify: `tests/test_ci_workflow.py` - enforce the Moto-based CI structure
- Modify: `tests/write_build_publish_test.rs` - rename LocalStack-specific harness/tests and switch endpointing to Moto
- Modify: `docs/design.md` - describe Moto as the current local AWS mock service
- Modify: `docs/superpowers/specs/2026-03-15-localstack-write-build-publish-design.md` - update body text to reflect Moto-based current implementation
- Modify: `docs/superpowers/plans/2026-03-15-localstack-write-build-publish.md` - update body text and commands to reflect Moto
- Modify: `docs/superpowers/plans/2026-03-14-write-build-publish-mvp.md` - update current command references if they still describe LocalStack as the active setup
- Modify: `docs/superpowers/plans/2026-03-14-lambda-verification-mvp.md` - update compose command references to Moto where describing current verification flow

## Chunk 1: Switch compose and CI with TDD

### Task 1: Update the CI guard test before changing the workflow

**Files:**
- Modify: `tests/test_ci_workflow.py`
- Modify: `.github/workflows/ci.yml`
- Create: `docker-compose.moto.yml`
- Delete: `docker-compose.localstack.yml`

- [ ] **Step 1: Write the failing CI guard update**

In `tests/test_ci_workflow.py`, replace the LocalStack compose expectations with Moto expectations:

```python
self.assertIn("run: docker compose -f docker-compose.moto.yml up -d", test)
self.assertIn(
    "if: always()\n        run: docker compose -f docker-compose.moto.yml down -v",
    test,
)
```

- [ ] **Step 2: Run the guard test to verify failure**

Run: `python3 -B tests/test_ci_workflow.py`
Expected: FAIL because `.github/workflows/ci.yml` still references `docker-compose.localstack.yml`.

- [ ] **Step 3: Add the Moto compose file**

Create `docker-compose.moto.yml` with the minimal Moto server definition:

```yaml
services:
  moto:
    image: motoserver/moto:latest
    ports:
      - "5000:5000"
    environment:
      MOTO_PORT: "5000"
```
```

- [ ] **Step 4: Update the CI workflow to use Moto**

In `.github/workflows/ci.yml`, replace:

- `docker compose -f docker-compose.localstack.yml up -d`
- `docker compose -f docker-compose.localstack.yml down -v`

with:

- `docker compose -f docker-compose.moto.yml up -d`
- `docker compose -f docker-compose.moto.yml down -v`

- [ ] **Step 5: Remove the old LocalStack compose file**

Delete `docker-compose.localstack.yml` once all references are updated.

- [ ] **Step 6: Re-run the CI guard test**

Run: `python3 -B tests/test_ci_workflow.py`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add tests/test_ci_workflow.py .github/workflows/ci.yml docker-compose.moto.yml docker-compose.localstack.yml
git commit -m "test: switch ci aws mock stack to moto"
```

## Chunk 2: Migrate the integration harness from LocalStack to Moto

### Task 2: Rename the harness and switch endpointing with TDD

**Files:**
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Write the failing first rename/endpoints change**

In `tests/write_build_publish_test.rs`, rename the first smoke test and harness usage to Moto-oriented naming:

```rust
#[tokio::test]
async fn moto_smoke_test_can_create_bucket_and_queue() {
    let harness = MotoHarness::new("bootstrap-smoke").await;
    assert!(harness.bucket_exists().await);
    assert!(harness.queue_exists().await);
}
```

- [ ] **Step 2: Run the targeted test to verify failure**

Run: `cargo test --test write_build_publish_test moto_smoke_test_can_create_bucket_and_queue -- --exact --nocapture`
Expected: FAIL because the harness/type/test names and endpoint are still LocalStack-based.

- [ ] **Step 3: Rename the harness and tests consistently**

Update all LocalStack-specific naming in `tests/write_build_publish_test.rs`:

- `LocalstackHarness` -> `MotoHarness`
- `localstack_*` -> `moto_*`
- `*_against_localstack` -> `*_against_moto`
- panic and error text mentioning LocalStack -> Moto or generic AWS mock wording

- [ ] **Step 4: Switch the AWS endpoint to Moto**

Replace the endpoint URL:

```rust
.endpoint_url("http://localhost:5000")
```

Keep:

- `localhost` host
- static credentials
- fixed region
- path-style S3 config

- [ ] **Step 5: Re-run the smoke test**

Run: `cargo test --test write_build_publish_test moto_smoke_test_can_create_bucket_and_queue -- --exact --nocapture`
Expected: PASS, assuming Moto is running locally.

### Task 3: Re-run the full integration harness and fix only Moto-specific fallout

**Files:**
- Modify: `tests/write_build_publish_test.rs`

- [ ] **Step 1: Run the full focused integration suite**

Run: `cargo test --test write_build_publish_test -- --nocapture`
Expected: Any remaining failures should be due to provider-specific assumptions, leftover names, or endpoint drift.

- [ ] **Step 2: Fix only the minimal Moto-specific issues**

Allowed fixes:

- readiness or create/get polling differences
- test assertions still using LocalStack wording
- endpoint/port assumptions

Not allowed:

- changing the adapter traits or public interfaces without a failing test demanding it
- redesigning the write/build/publish flow

- [ ] **Step 3: Re-run the focused integration suite**

Run: `cargo test --test write_build_publish_test -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add tests/write_build_publish_test.rs
git commit -m "test: run write-build-publish integration against moto"
```

## Chunk 3: Full verification

### Task 4: Verify the repository still passes after the provider switch

**Files:**
- Modify only if a verification failure exposes real Moto migration fallout

- [ ] **Step 1: Run the CI workflow guard**

Run: `python3 -B tests/test_ci_workflow.py`
Expected: PASS.

- [ ] **Step 2: Run the full Rust test suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 3: Manually verify compose startup/teardown commands**

Run: `docker compose -f docker-compose.moto.yml up -d`
Expected: Moto container starts.

Run: `docker compose -f docker-compose.moto.yml down -v`
Expected: container stops and volumes are removed.

- [ ] **Step 4: Commit any verification-only fallout fix if needed**

```bash
git add <exact files>
git commit -m "fix: align moto-backed integration verification"
```

## Chunk 4: Refresh docs and historical artifacts

### Task 5: Update current documentation to reflect Moto

**Files:**
- Modify: `docs/design.md`
- Modify: `docs/superpowers/specs/2026-03-15-localstack-write-build-publish-design.md`
- Modify: `docs/superpowers/plans/2026-03-15-localstack-write-build-publish.md`
- Modify: `docs/superpowers/plans/2026-03-14-write-build-publish-mvp.md`
- Modify: `docs/superpowers/plans/2026-03-14-lambda-verification-mvp.md`

- [ ] **Step 1: Replace current-operational LocalStack wording with Moto wording**

Update body text where it describes the current implementation or current commands. Replace:

- `LocalStack` -> `Moto` where describing the active local AWS mock provider
- `http://localhost:4566` -> `http://localhost:5000`
- `docker-compose.localstack.yml` -> `docker-compose.moto.yml`

- [ ] **Step 2: Preserve historical filenames but clarify current truth**

Do not rename the historical spec/plan files unless required. Instead, make the body text explicit that the current implementation now uses Moto.

- [ ] **Step 3: Re-read every documented command and test name**

Check that docs match the implemented repository state exactly, especially:

- compose filename
- endpoint port
- renamed Moto test identifiers

- [ ] **Step 4: Commit**

```bash
git add docs/design.md docs/superpowers/specs/2026-03-15-localstack-write-build-publish-design.md docs/superpowers/plans/2026-03-15-localstack-write-build-publish.md docs/superpowers/plans/2026-03-14-write-build-publish-mvp.md docs/superpowers/plans/2026-03-14-lambda-verification-mvp.md
git commit -m "docs: replace localstack references with moto"
```

## Verification

- Run: `python3 -B tests/test_ci_workflow.py`
- Expected: PASS
- Run: `docker compose -f docker-compose.moto.yml up -d`
- Expected: PASS
- Run: `cargo test --test write_build_publish_test -- --nocapture`
- Expected: PASS
- Run: `docker compose -f docker-compose.moto.yml down -v`
- Expected: PASS
- Run: `cargo test`
- Expected: PASS

Plan complete and saved to `docs/superpowers/plans/2026-03-16-moto-write-build-publish-migration.md`. Ready to execute?
