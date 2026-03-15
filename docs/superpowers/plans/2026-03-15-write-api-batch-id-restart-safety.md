# WriteApi Batch ID Restart Safety Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `WriteApi` batch IDs restart-safe so WAL segment keys cannot collide across process restarts on the same day.

**Architecture:** Replace the current process-local counter-driven batch ID with a restart-safe ID that combines the validated request timestamp and a generator-produced unique suffix that does not depend on process-local counters. Add an injectable batch ID generator seam so tests can verify restart behavior deterministically, keep WAL path generation and event ID structure unchanged apart from using the new batch ID value, and remove the dead counter state from `WriteApi`.

**Tech Stack:** Rust, `std::sync::atomic`, existing `WriteAheadLog`, existing Rust integration tests

---

## File Structure

- Modify: `src/write/api.rs` — replace batch ID generation, remove dead counter state, preserve enqueue/WAL ordering
- Modify: `tests/write_api_test.rs` — add regression coverage for restart-safe WAL key uniqueness and update assertions that depended on the old batch ID shape

## Chunk 1: Restart-safe batch IDs

### Task 1: Add regression coverage for same-day restart safety

**Files:**
- Modify: `tests/write_api_test.rs`

- [ ] **Step 1: Write a failing test around an extracted batch ID generator seam so two separately initialized generators on the same day can still produce distinct WAL keys even when given the same validated timestamp**
- [ ] **Step 2: Run the focused test to verify it fails**

Run: `cargo test restart_safe_batch_ids_produce_distinct_same_day_wal_keys --test write_api_test -- --exact`
Expected: FAIL because the current counter-based batch ID can repeat after a simulated restart.

### Task 2: Replace batch ID generation with restart-safe IDs

**Files:**
- Modify: `src/write/api.rs`

- [ ] **Step 1: Remove the unused per-instance batch counter field and the process-global `NEXT_BATCH_NUMBER` state**
- [ ] **Step 2: Introduce an injectable batch ID generator that builds `batch_id` from the validated request timestamp plus a restart-safe unique suffix**
- [ ] **Step 3: Use a default generator implementation backed by OS randomness or another non-counter unique source available in the codebase**
- [ ] **Step 4: Keep `event_id` generation and WAL append/enqueue flow unchanged except for using the new `batch_id` value**
- [ ] **Step 5: Re-run the focused regression test**

Run: `cargo test restart_safe_batch_ids_produce_distinct_same_day_wal_keys --test write_api_test -- --exact`
Expected: PASS.

### Task 3: Verify the full WriteApi surface still passes

**Files:**
- Modify: `tests/write_api_test.rs`
- Modify: `src/write/api.rs`

- [ ] **Step 1: Run the full WriteApi test target**

Run: `cargo test --test write_api_test`
Expected: PASS.

- [ ] **Step 2: Run the full repository test suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 3: Run lint verification**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS.

Plan complete and saved to `docs/superpowers/plans/2026-03-15-write-api-batch-id-restart-safety.md`. Ready to execute?
