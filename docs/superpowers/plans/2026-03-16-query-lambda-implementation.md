# Query Lambda Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a thin query Lambda binary that accepts plain `SearchRequest`, returns plain `SearchResponse` on success, and emits a typed error envelope on failure while delegating query behavior to library modules.

**Architecture:** Keep Lambda-specific concerns at the binary boundary only. Introduce the smallest possible query-Lambda adapter surface: serializable error/response types, a testable handler function, and a binary that wires the existing query stack into `lambda_runtime`. If a concrete query-time dependency is missing, add only the narrowest bootstrap seam required to construct the router.

**Tech Stack:** Rust, `lambda_runtime`, `tokio`, `serde`, `serde_json`, existing LTSearch query/storage modules

---

## File Structure

- Create: `src/bin/query_lambda.rs` - Lambda runtime entrypoint and thin adapter code
- Create: `tests/query_lambda_test.rs` - handler-focused tests for success and error mapping
- Modify: `Cargo.toml` - add Lambda runtime dependencies required by the new binary
- Modify: `src/lib.rs` - export any minimal shared modules if tests need them through the crate boundary
- Possible Create or Modify: a small query-lambda adapter module under `src/` only if binary-only organization becomes too hard to test cleanly

## Chunk 1: Define the handler contract with TDD

### Task 1: Add failing tests for the Lambda success and error shapes

**Files:**
- Create: `tests/query_lambda_test.rs`
- Create or Modify: `src/bin/query_lambda.rs`

- [ ] **Step 1: Write the failing success-path test**

Create `tests/query_lambda_test.rs` with a test that exercises a small handler function directly:

```rust
#[tokio::test]
async fn query_lambda_returns_plain_search_response_on_success() {
    let response = handle_search_request(test_handler_deps_success(), valid_search_request())
        .await
        .unwrap();

    let body = serde_json::to_value(&response).unwrap();
    assert_eq!(body["index_version"], 7);
    assert!(body.get("error_type").is_none());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test query_lambda_test query_lambda_returns_plain_search_response_on_success -- --exact --nocapture`
Expected: FAIL because the handler function and/or query Lambda binary surface does not exist.

- [ ] **Step 3: Write the failing validation-error mapping test**

Add a second test:

```rust
#[tokio::test]
async fn query_lambda_maps_validation_errors_to_error_envelope() {
    let error = handle_search_request(test_handler_deps_validation_error(), valid_search_request())
        .await
        .unwrap_err();

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "validation_error");
}
```

- [ ] **Step 4: Run the validation test to verify it fails**

Run: `cargo test --test query_lambda_test query_lambda_maps_validation_errors_to_error_envelope -- --exact --nocapture`
Expected: FAIL because the error envelope type and mapping do not exist.

- [ ] **Step 5: Write the failing execution-error mapping test**

Add a third test:

```rust
#[tokio::test]
async fn query_lambda_maps_execution_errors_to_error_envelope() {
    let error = handle_search_request(test_handler_deps_execution_error(), valid_search_request())
        .await
        .unwrap_err();

    let body = serde_json::to_value(&error).unwrap();
    assert_eq!(body["error_type"], "execution_error");
}
```

- [ ] **Step 6: Run the execution-error test to verify it fails**

Run: `cargo test --test query_lambda_test query_lambda_maps_execution_errors_to_error_envelope -- --exact --nocapture`
Expected: FAIL because the error envelope type and execution mapping do not exist.

## Chunk 2: Implement the smallest testable handler layer

### Task 2: Add minimal shared/query-Lambda adapter types and pass the new tests

**Files:**
- Create or Modify: `src/bin/query_lambda.rs`
- Create: `tests/query_lambda_test.rs`
- Modify if needed: `src/lib.rs`

- [ ] **Step 1: Add minimal serializable error type**

Define a small typed error envelope with exactly:

- `error_type: String`
- `message: String`

- [ ] **Step 2: Add a minimal testable handler function**

Implement a function that accepts:

- request data (`SearchRequest`)
- injectable dependencies or a small callable abstraction so tests can force success / validation / execution outcomes

Keep this function transport-agnostic.

- [ ] **Step 3: Map `SearchError::Validation` to `validation_error`**

- [ ] **Step 4: Map `SearchError::Execution` to `execution_error`**

- [ ] **Step 5: Keep success as plain `SearchResponse`**

Do not wrap successful responses in a success envelope.

- [ ] **Step 6: Run the full handler test file**

Run: `cargo test --test query_lambda_test -- --nocapture`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add tests/query_lambda_test.rs src/bin/query_lambda.rs src/lib.rs
git commit -m "feat: add query lambda handler contract"
```

## Chunk 3: Wire real query dependencies into the binary

### Task 3: Add failing tests for dependency bootstrap and real router delegation

**Files:**
- Modify: `tests/query_lambda_test.rs`
- Modify: `src/bin/query_lambda.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Write a failing test that the production bootstrap path can construct handler dependencies**

Prefer a narrow unit that verifies a real builder/bootstrap function returns a usable handler context or an explicit service error.

- [ ] **Step 2: Run the bootstrap test to verify it fails**

Run: `cargo test --test query_lambda_test <bootstrap_test_name> -- --exact --nocapture`
Expected: FAIL because the production dependency bootstrap function does not exist yet.

- [ ] **Step 3: Add Lambda runtime dependencies to `Cargo.toml`**

Add only what the binary needs, for example:

```toml
lambda_runtime = "0.13"
```

Add `tokio` features only if the runtime wiring requires them.

- [ ] **Step 4: Implement the narrow production bootstrap path**

Build the query stack from existing concrete pieces:

- `LocalManifestStore`
- concrete query-time embedding dependency, or the smallest adapter necessary if none exists yet
- `KeywordSearcher`
- `VectorSearcher`
- `QueryRouter`

If a true production query embedding provider does not exist, keep the seam minimal and return a service-style bootstrap error rather than expanding into a large provider framework.

- [ ] **Step 5: Re-run the bootstrap test**

Run: `cargo test --test query_lambda_test <bootstrap_test_name> -- --exact --nocapture`
Expected: PASS.

## Chunk 4: Add the actual Lambda entrypoint

### Task 4: Implement the binary runtime wrapper around the tested handler

**Files:**
- Modify: `src/bin/query_lambda.rs`

- [ ] **Step 1: Write the failing compile-level/runtime wiring test if practical**

If there is a clean way to test the runtime wrapper directly, add it. If not, rely on `cargo test` and `cargo build --bin query_lambda` as the verification boundary.

- [ ] **Step 2: Run the chosen verification command to verify failure**

Run: `cargo build --bin query_lambda`
Expected: FAIL because the binary target or runtime wrapper is incomplete.

- [ ] **Step 3: Implement the Lambda `main` function**

The binary should:

- start `lambda_runtime`
- deserialize the plain event into `SearchRequest`
- call the already-tested handler path
- return `SearchResponse` on success
- return the typed error envelope on expected failure

- [ ] **Step 4: Run binary build verification**

Run: `cargo build --bin query_lambda`
Expected: PASS.

- [ ] **Step 5: Re-run the handler test file**

Run: `cargo test --test query_lambda_test -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/bin/query_lambda.rs tests/query_lambda_test.rs src/lib.rs
git commit -m "feat: add query lambda binary"
```

## Chunk 5: Full verification

### Task 5: Run repository verification and catch fallout

**Files:**
- Modify only if verification exposes real fallout from the query Lambda addition

- [ ] **Step 1: Run focused query Lambda tests**

Run: `cargo test --test query_lambda_test -- --nocapture`
Expected: PASS.

- [ ] **Step 2: Build the binary explicitly**

Run: `cargo build --bin query_lambda`
Expected: PASS.

- [ ] **Step 3: Run the full Rust test suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 4: Run lint-equivalent checks if the branch is nearing PR state**

Run: `cargo fmt --check`
Expected: PASS.

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit any verification fallout fix if needed**

```bash
git add <exact files>
git commit -m "fix: align query lambda verification"
```

Plan complete and saved to `docs/superpowers/plans/2026-03-16-query-lambda-implementation.md`. Ready to execute?
