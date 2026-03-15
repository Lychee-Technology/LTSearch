# Toolchain And Real LanceDB Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade the Rust toolchain to a version compatible with real Rust LanceDB/Lance crates, then replace the local vector `rows.json` shim with true LanceDB-backed build and read paths.

**Architecture:** First raise the repo-wide compiler/dependency baseline and prove the existing project still builds cleanly. Then switch the indexing builder and vector searcher together so the artifact writer and reader stay in lockstep, while preserving the current manifest contract and Tantivy path.

**Tech Stack:** Rust, Cargo, `lancedb`, `arrow-array`, `arrow-schema`, `tokio`, `tantivy`, `serde`, `thiserror`

---

## Chunk 1: Toolchain upgrade

### Task 1: Upgrade toolchain and dependency baseline

**Files:**
- Modify: `rust-toolchain.toml`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Test: `cargo test`

- [ ] **Step 1: Write a failing compile-feasibility test or dependency probe**
- [ ] **Step 2: Run it on the old toolchain to verify failure**
- [ ] **Step 3: Upgrade `rust-toolchain.toml` to a Lance-compatible toolchain and add minimal real Lance dependencies**
- [ ] **Step 4: Run `cargo test` and `cargo clippy --all-targets --all-features -- -D warnings`**
- [ ] **Step 5: Commit**

## Chunk 2: Real Lance artifacts

### Task 2: Build true local Lance artifacts in the index builder

**Files:**
- Modify: `src/indexing/builder.rs`
- Modify: `src/indexing/mod.rs`
- Modify: `tests/index_builder_test.rs`

- [ ] **Step 1: Write failing builder tests for real Lance artifact creation**
- [ ] **Step 2: Run focused builder tests to verify failure**
- [ ] **Step 3: Implement minimal real LanceDB/Lance artifact build path**
- [ ] **Step 4: Re-run focused builder tests**
- [ ] **Step 5: Commit**

### Task 3: Read real Lance artifacts in the vector searcher

**Files:**
- Modify: `src/query/vector_searcher.rs`
- Modify: `src/query/mod.rs`
- Modify: `tests/vector_searcher_test.rs`

- [ ] **Step 1: Write failing vector searcher tests for real Lance-backed retrieval**
- [ ] **Step 2: Run focused vector tests to verify failure**
- [ ] **Step 3: Implement minimal Lance-backed read/query path**
- [ ] **Step 4: Re-run focused vector tests**
- [ ] **Step 5: Commit**

## Chunk 3: Integration verification

### Task 4: Re-verify issue #16 end-to-end with real Lance artifacts

**Files:**
- Modify: `tests/index_builder_test.rs`
- Modify: `tests/vector_searcher_test.rs`

- [ ] **Step 1: Add or update integration-style tests proving builder output is consumable by the real vector searcher**
- [ ] **Step 2: Run `cargo test --test index_builder_test --test vector_searcher_test`**
- [ ] **Step 3: Run `cargo test` and `cargo clippy --all-targets --all-features -- -D warnings`**
- [ ] **Step 4: Commit**

Plan complete and saved to `docs/superpowers/plans/2026-03-15-toolchain-and-real-lancedb.md`. Ready to execute?
