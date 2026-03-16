# Lambda Verification MVP Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the query, write, and build flows through Lambda binaries and lock down end-to-end verification plus developer runbooks.

**Architecture:** Keep each binary as a thin adapter over library code so testing stays centered in reusable modules. Add integration coverage only after the underlying library behaviors are already passing, then finish by documenting exact developer commands.

**Tech Stack:** Rust, `lambda_runtime`, `tokio`, Moto, Markdown docs

---

## Scope

Prerequisite: complete `docs/superpowers/plans/2026-03-14-query-core-mvp.md` and `docs/superpowers/plans/2026-03-14-write-build-publish-mvp.md` first.

- `#22` Implement query Lambda binary
- `#23` Implement write Lambda binary
- `#24` Implement index-builder Lambda binary
- `#25` Add end-to-end query integration coverage
- `#26` Document developer workflow and verification commands

## File Structure

- Create: `src/bin/query_lambda.rs`
- Create: `src/bin/write_lambda.rs`
- Create: `src/bin/index_builder_lambda.rs`
- Create: `tests/query_lambda_test.rs`
- Create: `tests/write_lambda_test.rs`
- Create: `tests/index_builder_lambda_test.rs`
- Create: `tests/query_flow_test.rs`
- Modify: `README.md`

## Chunk 1: Runtime adapters

### Task 1: Implement query Lambda binary (`#22`)

**Files:**
- Create: `src/bin/query_lambda.rs`
- Create: `tests/query_lambda_test.rs`

- [ ] **Step 1: Write the failing handler smoke test in `tests/query_lambda_test.rs`**
- [ ] **Step 2: Run the targeted test to verify failure**

Run: `cargo test query_lambda_handles_valid_request -- --exact`
Expected: FAIL with missing binary/handler.

- [ ] **Step 3: Implement minimal query handler wiring**
- [ ] **Step 4: Re-run the targeted test**

Run: `cargo test query_lambda_handles_valid_request -- --exact`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/bin/query_lambda.rs tests/query_lambda_test.rs
git commit -m "feat: add query lambda binary"
```

### Task 2: Implement write Lambda binary (`#23`)

**Files:**
- Create: `src/bin/write_lambda.rs`
- Create: `tests/write_lambda_test.rs`

- [ ] **Step 1: Write the failing handler test in `tests/write_lambda_test.rs` for ingest/delete request routing**
- [ ] **Step 2: Run the targeted test to verify failure**

Run: `cargo test write_lambda_routes_ingest_and_delete -- --exact`
Expected: FAIL with missing binary/handler.

- [ ] **Step 3: Implement minimal write handler wiring**
- [ ] **Step 4: Re-run the targeted test**

Run: `cargo test write_lambda_routes_ingest_and_delete -- --exact`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/bin/write_lambda.rs tests/write_lambda_test.rs
git commit -m "feat: add write lambda binary"
```

### Task 3: Implement index-builder Lambda binary (`#24`)

**Files:**
- Create: `src/bin/index_builder_lambda.rs`
- Create: `tests/index_builder_lambda_test.rs`

- [ ] **Step 1: Write the failing handler test in `tests/index_builder_lambda_test.rs` for batch processing**
- [ ] **Step 2: Run the targeted test to verify failure**

Run: `cargo test index_builder_lambda_processes_batch -- --exact`
Expected: FAIL with missing binary/handler.

- [ ] **Step 3: Implement minimal builder handler wiring**
- [ ] **Step 4: Re-run the targeted test**

Run: `cargo test index_builder_lambda_processes_batch -- --exact`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/bin/index_builder_lambda.rs tests/index_builder_lambda_test.rs
git commit -m "feat: add index-builder lambda binary"
```

## Chunk 2: End-to-end verification

### Task 4: Add query integration coverage (`#25`)

**Files:**
- Create: `tests/query_flow_test.rs`

- [ ] **Step 1: Write the failing integration test for manifest load, hybrid retrieval, and keyword-only fallback**
- [ ] **Step 2: Run the integration test to verify failure**

Run: `docker compose -f docker-compose.moto.yml up -d`
Expected: Moto starts.

Run: `cargo test --test query_flow_test -- --nocapture`
Expected: FAIL because end-to-end query wiring is incomplete.

- [ ] **Step 3: Add only the minimal harness needed to satisfy the integration expectations**
- [ ] **Step 4: Re-run the integration test**

Run: `cargo test --test query_flow_test -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add tests/query_flow_test.rs
git commit -m "test: add end-to-end query integration coverage"
```

### Task 5: Document developer workflow (`#26`)

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Write the failing documentation checklist**

Document the minimum commands that must be present:
- `cargo test`
- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `docker compose -f docker-compose.moto.yml up -d`

- [ ] **Step 2: Update `README.md` with exact setup and verification commands**
- [ ] **Step 3: Re-run every documented command verbatim and confirm the expected result**
- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: document developer workflow"
```

## Verification

- Run: `cargo build --bin query_lambda --bin write_lambda --bin index_builder_lambda`
- Expected: PASS
- Run: `cargo test`
- Expected: PASS
- Run: `cargo test --test query_flow_test -- --nocapture`
- Expected: PASS
- Run: `cargo fmt --check`
- Expected: PASS
- Run: `cargo clippy --all-targets --all-features -- -D warnings`
- Expected: PASS

Plan complete and saved to `docs/superpowers/plans/2026-03-14-lambda-verification-mvp.md`. Ready to execute?
