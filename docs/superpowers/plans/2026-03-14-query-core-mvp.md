# Query Core MVP Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the local, testable MVP query core: shared contracts, manifest loading, keyword/vector retrieval, hybrid fusion, and query orchestration.

**Architecture:** Implement the query path as library-first Rust modules with thin interfaces around embedding generation and retrieval engines. Finish all pure model, ranking, and routing behavior under fast unit tests before wiring concrete LanceDB and Tantivy adapters.

**Tech Stack:** Rust, `tokio`, `serde`, `serde_json`, `proptest`, `mockall`, `tantivy`, `lancedb`

---

## Scope

This sub-plan executes the first dependency chain for the project:
- `#9` Initialize Rust workspace and crate boundaries
- `#10` Implement core models, error types, and input validation
- `#11` Implement HybridRanker and RRF fusion logic
- `#12` Implement local IndexManifest and _head loading
- `#13` Implement Tantivy KeywordSearcher MVP
- `#14` Implement LanceDB VectorSearcher MVP
- `#15` Implement QueryRouter with keyword-only fallback

## File Structure

- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `src/lib.rs`
- Create: `src/error.rs`
- Create: `src/models/mod.rs`
- Create: `src/models/search.rs`
- Create: `src/models/index.rs`
- Create: `src/models/write.rs`
- Create: `src/storage/mod.rs`
- Create: `src/storage/manifest_store.rs`
- Create: `src/storage/s3_paths.rs`
- Create: `src/query/mod.rs`
- Create: `src/query/ranker.rs`
- Create: `src/query/router.rs`
- Create: `src/query/filter.rs`
- Create: `src/query/keyword_searcher.rs`
- Create: `src/query/vector_searcher.rs`
- Create: `src/embedding/mod.rs`
- Create: `src/embedding/generator.rs`
- Create: `tests/workspace_bootstrap_test.rs`
- Create: `tests/models_test.rs`
- Create: `tests/ranker_test.rs`
- Create: `tests/manifest_store_test.rs`
- Create: `tests/keyword_searcher_test.rs`
- Create: `tests/vector_searcher_test.rs`
- Create: `tests/router_test.rs`

## Chunk 1: Foundations

### Task 1: Bootstrap workspace (`#9`)

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `src/lib.rs`
- Create: `tests/workspace_bootstrap_test.rs`

- [ ] **Step 1: Write the failing smoke test**

```rust
// tests/workspace_bootstrap_test.rs
#[test]
fn crate_exposes_name_constant() {
    assert_eq!(ltsearch::CRATE_NAME, "ltsearch");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test crate_exposes_name_constant -- --exact`
Expected: FAIL with missing crate/module errors.

- [ ] **Step 3: Write minimal crate scaffolding**

```rust
// src/lib.rs
pub const CRATE_NAME: &str = "ltsearch";

pub mod config;
pub mod error;
pub mod models;
```

- [ ] **Step 4: Run the test again**

Run: `cargo test crate_exposes_name_constant -- --exact`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml rust-toolchain.toml src/lib.rs tests/workspace_bootstrap_test.rs
git commit -m "chore: bootstrap rust crate for search engine"
```

### Task 2: Add models and validation (`#10`)

**Files:**
- Create: `src/error.rs`
- Create: `src/models/mod.rs`
- Create: `src/models/search.rs`
- Create: `src/models/index.rs`
- Create: `src/models/write.rs`
- Create: `tests/models_test.rs`

- [ ] **Step 1: Write failing validation tests in `tests/models_test.rs` for `SearchRequest`, `IndexManifest`, and `WalRecord`**

- [ ] **Step 2: Run the focused tests and verify failure**

Run: `cargo test --test models_test`
Expected: FAIL with unresolved types or missing validation methods.

- [ ] **Step 3: Implement the minimal models and validation methods in `src/models/search.rs`, `src/models/index.rs`, and `src/models/write.rs`**
- [ ] **Step 4: Re-run model tests**

Run: `cargo test --test models_test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/error.rs src/models/mod.rs src/models/search.rs src/models/index.rs src/models/write.rs tests/models_test.rs
git commit -m "feat: add shared search and indexing models"
```

## Chunk 2: Retrieval primitives

### Task 3: Implement RRF fusion (`#11`)

**Files:**
- Create: `src/query/mod.rs`
- Create: `src/query/ranker.rs`
- Create: `tests/ranker_test.rs`

- [ ] **Step 1: Write failing tests in `tests/ranker_test.rs` for `compute_rrf_score` and `fuse`**
- [ ] **Step 2: Run ranker tests to verify failure**

Run: `cargo test --test ranker_test`
Expected: FAIL with missing `HybridRanker`.

- [ ] **Step 3: Implement minimal `HybridRanker` behavior**
- [ ] **Step 4: Re-run ranker tests**

Run: `cargo test --test ranker_test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/query/mod.rs src/query/ranker.rs tests/ranker_test.rs
git commit -m "feat: implement reciprocal rank fusion"
```

### Task 4: Implement local manifest and head loading (`#12`)

**Files:**
- Create: `src/storage/mod.rs`
- Create: `src/storage/manifest_store.rs`
- Create: `src/storage/s3_paths.rs`
- Create: `tests/manifest_store_test.rs`

- [ ] **Step 1: Write failing tests in `tests/manifest_store_test.rs` for `_head` parsing, manifest parsing, path generation, and active-version resolution**
- [ ] **Step 2: Run manifest store tests to verify failure**

Run: `cargo test --test manifest_store_test`
Expected: FAIL with missing storage helpers.

- [ ] **Step 3: Implement minimal store/path helpers**
- [ ] **Step 4: Re-run manifest store tests**

Run: `cargo test --test manifest_store_test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/storage/mod.rs src/storage/manifest_store.rs src/storage/s3_paths.rs tests/manifest_store_test.rs
git commit -m "feat: add local manifest loading"
```

### Task 5: Implement keyword retrieval (`#13`)

**Files:**
- Create: `src/query/keyword_searcher.rs`
- Create: `tests/keyword_searcher_test.rs`

- [ ] **Step 1: Write failing tests in `tests/keyword_searcher_test.rs` for top-k, sorting, invalid query handling, and manifest-driven `index_version` context**
- [ ] **Step 2: Run keyword searcher tests to verify failure**

Run: `cargo test --test keyword_searcher_test`
Expected: FAIL with missing `KeywordSearcher`.

- [ ] **Step 3: Implement the minimal Tantivy-backed searcher that satisfies the first test**
- [ ] **Step 4: Expand only enough to satisfy all keyword tests**
- [ ] **Step 5: Re-run keyword tests**

Run: `cargo test --test keyword_searcher_test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/query/keyword_searcher.rs tests/keyword_searcher_test.rs
git commit -m "feat: add tantivy keyword searcher"
```

### Task 6: Implement vector retrieval (`#14`)

**Files:**
- Create: `src/query/vector_searcher.rs`
- Create: `tests/vector_searcher_test.rs`

- [ ] **Step 1: Write failing tests in `tests/vector_searcher_test.rs` for embedding-dimension validation, top-k, result ordering, and manifest-driven embedding compatibility**
- [ ] **Step 2: Run vector searcher tests to verify failure**

Run: `cargo test --test vector_searcher_test`
Expected: FAIL with missing `VectorSearcher`.

- [ ] **Step 3: Implement the minimal LanceDB-backed searcher that satisfies the first test**
- [ ] **Step 4: Expand only enough to satisfy all vector tests**
- [ ] **Step 5: Re-run vector tests**

Run: `cargo test --test vector_searcher_test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/query/vector_searcher.rs tests/vector_searcher_test.rs
git commit -m "feat: add lancedb vector searcher"
```

## Chunk 3: Query orchestration

### Task 7: Implement router and filtering (`#15`)

**Files:**
- Create: `src/query/router.rs`
- Create: `src/query/filter.rs`
- Create: `src/embedding/mod.rs`
- Create: `src/embedding/generator.rs`
- Create: `tests/router_test.rs`

- [ ] **Step 1: Write failing tests in `tests/router_test.rs` for validation failure, hybrid path, keyword-only fallback, post-retrieval filtering, and returned `index_version`**
- [ ] **Step 2: Run router tests to verify failure**

Run: `cargo test --test router_test`
Expected: FAIL with missing `QueryRouter` and embedding interfaces.

- [ ] **Step 3: Implement minimal router behavior**
- [ ] **Step 4: Re-run router tests**

Run: `cargo test --test router_test`
Expected: PASS.

- [ ] **Step 5: Run the entire query-core suite**

Run: `cargo test --test workspace_bootstrap_test --test models_test --test ranker_test --test manifest_store_test --test keyword_searcher_test --test vector_searcher_test --test router_test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/query/mod.rs src/query/ranker.rs src/query/router.rs src/query/filter.rs src/query/keyword_searcher.rs src/query/vector_searcher.rs src/embedding/mod.rs src/embedding/generator.rs tests/router_test.rs
git commit -m "feat: add query router with hybrid fallback"
```

## Verification

- Run: `cargo check`
- Expected: PASS
- Run: `cargo test`
- Expected: all query-core tests pass
- Run: `cargo fmt --check`
- Expected: PASS
- Run: `cargo clippy --all-targets --all-features -- -D warnings`
- Expected: PASS

Plan complete and saved to `docs/superpowers/plans/2026-03-14-query-core-mvp.md`. Ready to execute?
