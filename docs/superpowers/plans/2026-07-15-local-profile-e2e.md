# Local-profile e2e (file-based) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship AWS-free `local`-feature composition roots for the write/index-builder/query HTTP servers and a moto-free native-process CI e2e that drives the write→build→query loop over HTTP.

**Architecture:** Reuse the three existing `*_server` bins; relax their `required-features` from `["aws"]` to `["server"]` and cfg-split only their composition roots so `--features local` wires the existing `LocalFs*` contract impls (plain files) instead of AWS clients. The build worker's provider-neutral `run_build_job_loop` is spawned directly with a `LocalFsBuildQueue` as its `BuildJobSource`; no new worker-loop function is added. A curl-driven flow script launches the three servers as native processes sharing one root dir; a standalone `local-e2e` GHA job builds them under `--features local` and runs the script.

**Tech Stack:** Rust (axum HTTP, tokio), existing `LocalFs*` impls (`src/local/`), GitHub Actions (`ubuntu-24.04-arm`), bash + curl for the flow.

## Global Constraints

- Toolchain: Rust `1.94.0` (pinned in `rust-toolchain.toml`); do not change it.
- CI must need **no** Docker, **no** moto, **no** model download. Embeddings use `EmbeddingProvider::Fixed` (no `ltembed` feature).
- Local branches must construct **zero** AWS clients and must **not** reference `crate::adapters::*` (that module is `#[cfg(feature = "aws")]` and absent under `local`). All `use ltsearch::adapters::…` lines in the touched bins must be `#[cfg(feature = "aws")]`-gated.
- The `--features aws` build of all three bins must remain byte-for-behavior unchanged (covered by existing `http-e2e`/`sam-e2e`).
- Any new lib item used only by the local branch must be gated `#[cfg(not(feature = "aws"))]` so the `--features aws,lambda,ltembed` clippy pass (run with `-D warnings` in `scripts/verify-fast.sh`) sees no dead code.
- Env-var contract (exact names):
  - `LTSEARCH_LOCAL_ROOT` — **new**; shared root for the write + index-builder local branches. Required in those branches.
  - `LTSEARCH_QUERY_ARTIFACT_ROOT` — existing; query artifact root. The e2e sets it equal to `LTSEARCH_LOCAL_ROOT`.
  - `LTSEARCH_HTTP_PORT` — existing; per-process listen port (default 8080).
  - `LTSEARCH_BUILD_EMBEDDING_DIM` — existing; required by the worker path. Set in the e2e.
  - `LTSEARCH_BUILD_EMBEDDING_PROVIDER` (default `fixed`) / `LTSEARCH_BUILD_FIXED_EMBEDDING` — build-side fixed vector.
  - `LTSEARCH_QUERY_EMBEDDING_PROVIDER` (set `fixed`) / `LTSEARCH_QUERY_FIXED_EMBEDDING` — query-side fixed vector. Must be the **same dimension** as the build vector.
- Related issue: this plan is a partial delivery of **#108** (file-based durability now; SQLite substrate remains out of scope).

---

## File Structure

- `src/local/wal_keys.rs` — **new**. `list_local_wal_keys(root) -> io::Result<Vec<String>>`: recursively walks `<root>/wal/`, returning keys relative to `root` with `/` separators. Unit-tested. Gated `#[cfg(not(feature = "aws"))]`.
- `src/local/mod.rs` — add `mod wal_keys; pub use wal_keys::list_local_wal_keys;` (gated).
- `src/bootstrap.rs` — add `LocalConfig { root: String }` + `from_env()` reading `LTSEARCH_LOCAL_ROOT`. Gated `#[cfg(not(feature = "aws"))]`. Unit-tested.
- `Cargo.toml` — change `required-features` of `query_server`, `write_server`, `index_builder_server` from `["aws"]` to `["server"]`.
- `src/bin/query_server.rs` — no code change (already AWS-free); only the Cargo.toml gate relaxes.
- `src/bin/write_server.rs` — cfg-split composition root; add local branch.
- `src/bin/index_builder_server.rs` — cfg-split composition root; add local branch that spawns `run_build_job_loop`.
- `scripts/e2e/run-local-server-flow.sh` — **new**. Native-process write→build→query flow.
- `.github/workflows/ci.yml` — **new** `local-e2e` job.

---

## Task 1: Local WAL-key lister (`list_local_wal_keys`)

**Files:**
- Create: `src/local/wal_keys.rs`
- Modify: `src/local/mod.rs`
- Test: `src/local/wal_keys.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `#[cfg(not(feature = "aws"))] pub fn list_local_wal_keys(root: &std::path::Path) -> std::io::Result<Vec<String>>` — returns WAL segment keys (e.g. `wal/2026/07/15/seg.jsonl`) relative to `root`, sorted, `/`-separated; returns `Ok(vec![])` if `<root>/wal` is absent. Consumed by Task 5's local worker `ListWalKeysFn`.

Rationale: the AWS worker lists S3 objects under the `wal/` prefix (`src/bin/index_builder_server.rs:145-170`). The local worker needs the filesystem equivalent, and `LocalFsWalStorage::read(key)` reads `<root>/<key>`, so keys must be **relative to root**.

- [ ] **Step 1: Write the failing test**

Create `src/local/wal_keys.rs`:

```rust
//! 本地 profile 的 WAL 段枚举：递归遍历 `<root>/wal/` 返回相对 root 的 key，
//! 供 index-builder 本地 worker 在每次构建前取得完整快照输入（对齐 AWS 侧的
//! ListObjectsV2(prefix="wal/") 语义）。
#![cfg(not(feature = "aws"))]

use std::path::Path;

/// 递归列出 `<root>/wal/` 下所有文件，返回相对 `root` 的、以 `/` 分隔的 key，
/// 已排序。若 `wal/` 不存在则返回空列表（与空队列同构，不视为错误）。
pub fn list_local_wal_keys(root: &Path) -> std::io::Result<Vec<String>> {
    let wal_dir = root.join("wal");
    let mut keys = Vec::new();
    if !wal_dir.exists() {
        return Ok(keys);
    }
    let mut stack = vec![wal_dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                keys.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    keys.sort();
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_nested_wal_segments_relative_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let seg_dir = root.join("wal/2026/07/15");
        std::fs::create_dir_all(&seg_dir).unwrap();
        std::fs::write(seg_dir.join("b.jsonl"), b"{}\n").unwrap();
        std::fs::write(seg_dir.join("a.jsonl"), b"{}\n").unwrap();
        // Non-wal files must be ignored.
        std::fs::create_dir_all(root.join("queue")).unwrap();
        std::fs::write(root.join("queue/x.json"), b"{}").unwrap();

        let keys = list_local_wal_keys(root).unwrap();
        assert_eq!(
            keys,
            vec![
                "wal/2026/07/15/a.jsonl".to_string(),
                "wal/2026/07/15/b.jsonl".to_string(),
            ]
        );
    }

    #[test]
    fn missing_wal_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_local_wal_keys(dir.path()).unwrap().is_empty());
    }
}
```

Wire the module in `src/local/mod.rs` (add near the other `mod`/`pub use` lines):

```rust
#[cfg(not(feature = "aws"))]
mod wal_keys;
#[cfg(not(feature = "aws"))]
pub use wal_keys::list_local_wal_keys;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --no-default-features --features local --lib local::wal_keys`
Expected: FAIL to compile or `list_local_wal_keys` not found — before wiring `mod.rs` it won't resolve; after wiring, tests run. (If it compiles and passes immediately because you pasted the impl, that's fine — the impl and test were authored together; proceed.)

- [ ] **Step 3: (impl already written in Step 1)**

No additional code — the implementation lives beside the test.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --no-default-features --features local --lib local::wal_keys`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/local/wal_keys.rs src/local/mod.rs
git commit -m "feat(local): add list_local_wal_keys for file-based WAL enumeration"
```

---

## Task 2: `LocalConfig` env parsing

**Files:**
- Modify: `src/bootstrap.rs`
- Test: `src/bootstrap.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `#[cfg(not(feature = "aws"))] pub struct LocalConfig { pub root: String }` with `pub fn from_env() -> Result<Self, BootstrapError>` reading required env `LTSEARCH_LOCAL_ROOT`. `BootstrapError::MissingEnv { name }` already exists (`src/bootstrap.rs:17-23`). Consumed by Tasks 4 and 5.

- [ ] **Step 1: Write the failing test**

Add to `src/bootstrap.rs` (near `WriteConfig`/`BuildConfig`):

```rust
/// 本地 profile 的运行时配置：所有本地契约实现（WAL / 队列 / 发布 / manifest）
/// 共用一个根目录。write-server 与 index-builder-server 的本地分支从此读取。
#[cfg(not(feature = "aws"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    pub root: String,
}

#[cfg(not(feature = "aws"))]
impl LocalConfig {
    pub fn from_env() -> Result<Self, BootstrapError> {
        let root = std::env::var("LTSEARCH_LOCAL_ROOT").map_err(|_| BootstrapError::MissingEnv {
            name: "LTSEARCH_LOCAL_ROOT".to_string(),
        })?;
        if root.trim().is_empty() {
            return Err(BootstrapError::MissingEnv {
                name: "LTSEARCH_LOCAL_ROOT".to_string(),
            });
        }
        Ok(Self { root })
    }
}
```

Add the test (guard env mutation with a process-global mutex if the file already has one; otherwise this standalone test is fine since it sets then removes the var):

```rust
#[cfg(all(test, not(feature = "aws")))]
mod local_config_tests {
    use super::*;

    #[test]
    fn from_env_reads_local_root() {
        std::env::set_var("LTSEARCH_LOCAL_ROOT", "/tmp/ltsearch-e2e");
        let cfg = LocalConfig::from_env().unwrap();
        assert_eq!(cfg.root, "/tmp/ltsearch-e2e");
        std::env::remove_var("LTSEARCH_LOCAL_ROOT");
    }

    #[test]
    fn from_env_errors_when_missing() {
        std::env::remove_var("LTSEARCH_LOCAL_ROOT");
        match LocalConfig::from_env() {
            Err(BootstrapError::MissingEnv { name }) => {
                assert_eq!(name, "LTSEARCH_LOCAL_ROOT");
            }
            other => panic!("expected MissingEnv, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --no-default-features --features local --lib bootstrap::local_config_tests`
Expected: FAIL — `LocalConfig` not defined (before you paste the impl).

- [ ] **Step 3: (impl written in Step 1)** — no extra code.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --no-default-features --features local --lib bootstrap::local_config_tests`
Expected: PASS (2 tests). If they run in parallel and the env var races, add `-- --test-threads=1` for this module.

- [ ] **Step 5: Commit**

```bash
git add src/bootstrap.rs
git commit -m "feat(bootstrap): add LocalConfig reading LTSEARCH_LOCAL_ROOT"
```

---

## Task 3: Relax `query_server` to build under `local`

**Files:**
- Modify: `Cargo.toml` (the `query_server` `[[bin]]` entry)

**Interfaces:**
- Consumes: nothing new. `src/bin/query_server.rs` already builds `QueryServerState { service: Arc::new(QueryService::new()), embedding_probe }` with zero AWS references (verified). `QueryService::sync_artifacts_if_configured` already has the `#[cfg(not(feature = "aws"))] → NoopArtifactSync` branch (`src/query_service.rs:41-48`).

- [ ] **Step 1: Change the feature gate**

In `Cargo.toml`, the `query_server` bin block currently reads:

```toml
[[bin]]
name = "query_server"
path = "src/bin/query_server.rs"
required-features = ["aws"]
```

Change the last line to:

```toml
required-features = ["server"]
```

- [ ] **Step 2: Verify it builds under local**

Run: `cargo build --no-default-features --features local --bin query_server`
Expected: PASS (compiles; produces `target/debug/query_server`).

- [ ] **Step 3: Verify it still builds under aws (regression guard)**

Run: `cargo build --no-default-features --features aws --bin query_server`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml
git commit -m "feat(local): allow query_server to build under the local profile"
```

---

## Task 4: `write_server` local composition root

**Files:**
- Modify: `src/bin/write_server.rs`
- Modify: `Cargo.toml` (`write_server` `required-features` → `["server"]`)

**Interfaces:**
- Consumes: `LocalConfig::from_env()` (Task 2); `LocalFsWalStorage::new(root)` + `LocalFsBuildQueue::new(root)` (`src/local/`); `WriteAheadLog::new`, `WriteApi::new`, `WriteServerState`, `write_router`, `serve`, `port_from_env` (all unconditional).

- [ ] **Step 1: Rewrite the bin with a cfg-split composition root**

Replace the entire contents of `src/bin/write_server.rs` with:

```rust
use std::sync::Arc;

use futures::future::FutureExt;

use ltsearch::http::write::{write_router, WriteServerState};
use ltsearch::http::{port_from_env, serve};
use ltsearch::write::api::WriteApi;
use ltsearch::write::wal::WriteAheadLog;

#[cfg(feature = "aws")]
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
#[cfg(feature = "aws")]
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;
#[cfg(feature = "aws")]
use ltsearch::bootstrap::{s3_client_from_env, sqs_client_from_env, WriteConfig};

#[cfg(not(feature = "aws"))]
use ltsearch::bootstrap::LocalConfig;
#[cfg(not(feature = "aws"))]
use ltsearch::local::{LocalFsBuildQueue, LocalFsWalStorage};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Runtime::new()?.block_on(async {
        // AWS profile: S3 WAL + SQS build queue (unchanged; 照抄 write_lambda 接线).
        #[cfg(feature = "aws")]
        let write_api = {
            let write_config = WriteConfig::from_env()?;
            let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let wal_storage =
                AwsS3WalStorage::new(write_config.s3_bucket, s3_client_from_env(&sdk_config));
            let build_queue =
                AwsSqsBuildQueue::new(write_config.sqs_queue_url, sqs_client_from_env(&sdk_config));
            Arc::new(WriteApi::new(WriteAheadLog::new(wal_storage), build_queue))
        };

        // Local profile: filesystem WAL + filesystem build queue under one root.
        #[cfg(not(feature = "aws"))]
        let write_api = {
            let config = LocalConfig::from_env()?;
            let wal_storage = LocalFsWalStorage::new(&config.root);
            let build_queue = LocalFsBuildQueue::new(&config.root);
            Arc::new(WriteApi::new(WriteAheadLog::new(wal_storage), build_queue))
        };

        let ingest_api = write_api.clone();
        let delete_api = write_api.clone();
        let state = WriteServerState {
            ingest: Arc::new(move |documents| {
                let api = ingest_api.clone();
                async move { api.ingest(documents).await }.boxed()
            }),
            delete: Arc::new(move |doc_ids| {
                let api = delete_api.clone();
                async move { api.delete(doc_ids).await }.boxed()
            }),
        };

        let port = port_from_env();
        eprintln!("ltsearch-write-server listening on 0.0.0.0:{port}");
        serve(write_router(state), port).await?;
        Ok(())
    })
}
```

In `Cargo.toml`, change the `write_server` bin's `required-features = ["aws"]` → `required-features = ["server"]`.

- [ ] **Step 2: Verify local build**

Run: `cargo build --no-default-features --features local --bin write_server`
Expected: PASS.

- [ ] **Step 3: Verify aws build (regression guard)**

Run: `cargo build --no-default-features --features aws --bin write_server`
Expected: PASS.

- [ ] **Step 4: Verify clippy is clean on both profiles**

Run: `cargo clippy --no-default-features --features local --bin write_server -- -D warnings`
Then: `cargo clippy --no-default-features --features aws --bin write_server -- -D warnings`
Expected: both PASS (no dead-code or unused-import warnings; the cfg-gated `use` lines prevent unused imports in each profile).

- [ ] **Step 5: Commit**

```bash
git add src/bin/write_server.rs Cargo.toml
git commit -m "feat(local): wire write_server to filesystem WAL + build queue under local profile"
```

---

## Task 5: `index_builder_server` local composition root + local worker

**Files:**
- Modify: `src/bin/index_builder_server.rs`
- Modify: `Cargo.toml` (`index_builder_server` `required-features` → `["server"]`)

**Interfaces:**
- Consumes: `LocalConfig::from_env()` (Task 2); `list_local_wal_keys` (Task 1); `LocalFsWalStorage`, `LocalFsBuildQueue`, `LocalFsPublishStorage` (`src/local/`); `run_build_job_loop` (provider-neutral, `src/build_worker.rs:142`); `BuildServerState`/`BuildFn`/`PublishFn`/`SnapshotBuildRequest`/`build_router`; `LocalIndexBuilder`, `IndexPublisher`, `BuildIndexRequest`, `PublishRequest`; `build_embedding_generator_from_env`, `build_embedding_provider_from_env`, `probe_build_embedding_from_env` (unconditional); `ListWalKeysFn` type alias.

Design notes:
- `run_build_job_loop<C: BuildJobSource, S: PublishStorage>(source, state, storage, list_wal_keys)` is already provider-neutral — spawn it directly with `LocalFsBuildQueue` (which implements `BuildJobSource`) as `source` and `LocalFsPublishStorage` as `storage`. No new lib function.
- Unlike the AWS server (which only starts the worker when `LTSEARCH_BUILD_SQS_QUEUE_URL` is set), the **local worker always starts** — a single local process must close the write→build loop.
- `list_wal_keys` local closure wraps `list_local_wal_keys(root)` and maps `io::Error` → `String`.
- The build/publish closures mirror the AWS `build_closure`/`publish_closure`, swapping `AwsS3WalStorage`→`LocalFsWalStorage` and `AwsPublishStorage`→`LocalFsPublishStorage`; `LocalIndexBuilder`/`IndexPublisher`/embedding wiring are identical. Artifact root for `LocalIndexBuilder::new` and `IndexPublisher::new` is the shared `config.root` (the immutable LanceDB/Tantivy release trees live under it).

- [ ] **Step 1: Rewrite the bin with cfg-split composition root**

Replace the entire contents of `src/bin/index_builder_server.rs` with the following. The AWS branch is the existing code verbatim; the local branch is new.

```rust
//! index_builder 的 HTTP 服务进程：暴露 `POST /build` + `GET /health`。AWS profile
//! 下在设置 `LTSEARCH_BUILD_SQS_QUEUE_URL` 时后台轮询 SQS；local profile 下始终以
//! 文件系统构建队列（LocalFsBuildQueue）驱动 run_build_job_loop，单进程闭环
//! write→build。build/publish 接线在两 profile 间共享结构，仅替换存储后端。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::FutureExt;

use ltsearch::bootstrap::{
    build_embedding_generator_from_env, build_embedding_provider_from_env,
    probe_build_embedding_from_env,
};
use ltsearch::build_worker::ListWalKeysFn;
use ltsearch::error::IndexError;
use ltsearch::http::build::{build_router, BuildServerState, SnapshotBuildRequest};
use ltsearch::http::{port_from_env, serve};
use ltsearch::indexing::{BuildIndexRequest, IndexPublisher, LocalIndexBuilder, PublishRequest};
use ltsearch::write::WriteAheadLog;

#[cfg(feature = "aws")]
use ltsearch::adapters::s3_publish::AwsPublishStorage;
#[cfg(feature = "aws")]
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
#[cfg(feature = "aws")]
use ltsearch::bootstrap::{s3_client_from_env, sqs_client_from_env, BuildConfig};
#[cfg(feature = "aws")]
use ltsearch::build_worker::run_sqs_worker_loop;

#[cfg(not(feature = "aws"))]
use ltsearch::bootstrap::LocalConfig;
#[cfg(not(feature = "aws"))]
use ltsearch::build_worker::run_build_job_loop;
#[cfg(not(feature = "aws"))]
use ltsearch::local::{list_local_wal_keys, LocalFsBuildQueue, LocalFsPublishStorage, LocalFsWalStorage};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Runtime::new()?.block_on(async {
        #[cfg(feature = "aws")]
        {
            let config = BuildConfig::from_env()?;
            let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let s3_client = s3_client_from_env(&sdk_config);

            let state = BuildServerState {
                build: aws_build_closure(config.clone(), s3_client.clone()),
                publish: aws_publish_closure(config.clone(), s3_client.clone()),
                embedding_probe: Arc::new(build_embedding_probe()),
            };

            if let Ok(queue_url) = std::env::var("LTSEARCH_BUILD_SQS_QUEUE_URL") {
                if !queue_url.trim().is_empty() {
                    let sqs = sqs_client_from_env(&sdk_config);
                    let publish_storage =
                        AwsPublishStorage::new(config.s3_bucket.clone(), s3_client.clone());
                    let worker_state = state.clone();
                    eprintln!("ltsearch-index-builder-server: SQS worker enabled on {queue_url}");
                    tokio::spawn(run_sqs_worker_loop(
                        sqs,
                        queue_url,
                        worker_state,
                        publish_storage,
                        aws_list_wal_keys_closure(config.s3_bucket.clone(), s3_client.clone()),
                    ));
                }
            }

            let port = port_from_env();
            eprintln!("ltsearch-index-builder-server listening on 0.0.0.0:{port}");
            serve(build_router(state), port).await?;
        }

        #[cfg(not(feature = "aws"))]
        {
            let config = LocalConfig::from_env()?;
            let state = BuildServerState {
                build: local_build_closure(config.root.clone()),
                publish: local_publish_closure(config.root.clone()),
                embedding_probe: Arc::new(build_embedding_probe()),
            };

            // 本地单进程始终启用 worker，闭环 write→build。
            let queue = LocalFsBuildQueue::new(&config.root);
            let publish_storage = LocalFsPublishStorage::new(&config.root);
            eprintln!("ltsearch-index-builder-server: local worker enabled on {}", config.root);
            tokio::spawn(run_build_job_loop(
                queue,
                state.clone(),
                publish_storage,
                local_list_wal_keys_closure(config.root.clone()),
            ));

            let port = port_from_env();
            eprintln!("ltsearch-index-builder-server listening on 0.0.0.0:{port}");
            serve(build_router(state), port).await?;
        }

        Ok(())
    })
}

// ---- AWS closures (unchanged) ----

#[cfg(feature = "aws")]
fn aws_build_closure(
    config: BuildConfig,
    s3_client: aws_sdk_s3::Client,
) -> ltsearch::http::build::BuildFn {
    Arc::new(move |request: SnapshotBuildRequest| {
        let config = config.clone();
        let s3_client = s3_client.clone();
        async move {
            let wal_storage = AwsS3WalStorage::new(config.s3_bucket.clone(), s3_client.clone());
            let records = read_wal_records(&WriteAheadLog::new(wal_storage), &request.wal_keys).await?;
            let embedding_generator = embedding_generator()?;
            let builder = LocalIndexBuilder::new(&config.artifact_root, embedding_generator);
            run_build(builder, request).await
        }
        .boxed()
    })
}

#[cfg(feature = "aws")]
fn aws_publish_closure(
    config: BuildConfig,
    s3_client: aws_sdk_s3::Client,
) -> ltsearch::http::build::PublishFn {
    Arc::new(move |manifest, expected: Option<u64>| {
        let config = config.clone();
        let s3_client = s3_client.clone();
        async move {
            let publish_storage =
                AwsPublishStorage::new(config.s3_bucket.clone(), s3_client.clone());
            let publisher = IndexPublisher::new(&config.artifact_root, publish_storage);
            publisher
                .publish(&PublishRequest {
                    manifest,
                    expected_current_version: expected,
                    updated_at: current_time_millis(),
                })
                .await
        }
        .boxed()
    })
}

#[cfg(feature = "aws")]
fn aws_list_wal_keys_closure(bucket: String, s3_client: aws_sdk_s3::Client) -> ListWalKeysFn {
    Arc::new(move || {
        let bucket = bucket.clone();
        let s3_client = s3_client.clone();
        async move {
            let mut keys = Vec::new();
            let mut paginator = s3_client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(ltsearch::write::WAL_PREFIX)
                .into_paginator()
                .send();
            while let Some(page) = paginator.next().await {
                let page = page
                    .map_err(|error| format!("failed to list WAL objects in {bucket}: {error}"))?;
                for object in page.contents() {
                    if let Some(key) = object.key() {
                        keys.push(key.to_string());
                    }
                }
            }
            Ok(keys)
        }
        .boxed()
    })
}

// ---- Local closures ----

#[cfg(not(feature = "aws"))]
fn local_build_closure(root: String) -> ltsearch::http::build::BuildFn {
    Arc::new(move |request: SnapshotBuildRequest| {
        let root = root.clone();
        async move {
            let wal_storage = LocalFsWalStorage::new(&root);
            let records = read_wal_records(&WriteAheadLog::new(wal_storage), &request.wal_keys).await?;
            let embedding_generator = embedding_generator()?;
            let builder = LocalIndexBuilder::new(&root, embedding_generator);
            run_build(builder, request).await
        }
        .boxed()
    })
}

#[cfg(not(feature = "aws"))]
fn local_publish_closure(root: String) -> ltsearch::http::build::PublishFn {
    Arc::new(move |manifest, expected: Option<u64>| {
        let root = root.clone();
        async move {
            let publish_storage = LocalFsPublishStorage::new(&root);
            let publisher = IndexPublisher::new(&root, publish_storage);
            publisher
                .publish(&PublishRequest {
                    manifest,
                    expected_current_version: expected,
                    updated_at: current_time_millis(),
                })
                .await
        }
        .boxed()
    })
}

#[cfg(not(feature = "aws"))]
fn local_list_wal_keys_closure(root: String) -> ListWalKeysFn {
    Arc::new(move || {
        let root = root.clone();
        async move {
            list_local_wal_keys(std::path::Path::new(&root))
                .map_err(|error| format!("failed to list local WAL segments under {root}: {error}"))
        }
        .boxed()
    })
}

// ---- Shared helpers (unconditional) ----

/// 读取给定 wal_keys 的全部记录并按段拼接（快照重放）。
async fn read_wal_records<S>(
    wal: &WriteAheadLog<S>,
    wal_keys: &[String],
) -> Result<Vec<ltsearch::models::WalRecord>, IndexError>
where
    S: ltsearch::write::WalStorage,
{
    let mut records = Vec::new();
    for wal_key in wal_keys {
        let segment = wal.read(wal_key).await.map_err(|error| IndexError::Operation {
            message: format!("failed to read WAL records from {wal_key}: {error}"),
        })?;
        records.extend(segment);
    }
    Ok(records)
}

fn embedding_generator() -> Result<Box<dyn ltsearch::embedding::EmbeddingGenerator>, IndexError> {
    let provider = build_embedding_provider_from_env().map_err(|error| IndexError::Operation {
        message: error.to_string(),
    })?;
    build_embedding_generator_from_env(provider).map_err(|error| IndexError::Operation {
        message: error.to_string(),
    })
}

async fn run_build<E>(
    builder: LocalIndexBuilder<E>,
    request: SnapshotBuildRequest,
) -> Result<ltsearch::indexing::BuildIndexResult, IndexError>
where
    E: ltsearch::embedding::EmbeddingGenerator + Send + 'static,
{
    let build_request = BuildIndexRequest {
        version_id: request.version_id,
        created_at: current_time_millis(),
        embedding_dim: request.embedding_dim,
        records: request.records_placeholder(),
    };
    tokio::task::spawn_blocking(move || builder.build(&build_request))
        .await
        .map_err(|error| IndexError::Operation {
            message: format!("build task panicked: {error}"),
        })?
}

fn build_embedding_probe() -> impl Fn() -> Result<usize, String> + Send + Sync {
    use std::sync::OnceLock;
    static PROBE_RESULT: OnceLock<Result<usize, String>> = OnceLock::new();
    move || {
        PROBE_RESULT
            .get_or_init(probe_build_embedding_from_env)
            .clone()
    }
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
```

> **Implementer note — `records` threading:** `SnapshotBuildRequest` (from `src/http/build.rs`) does **not** carry `records`; the AWS bin reads them from WAL *inside* the build closure (`read_wal_records`) and then builds. The `run_build` helper above must take the already-read `records: Vec<WalRecord>` as a parameter rather than the placeholder `request.records_placeholder()`. Correct the helper to:
> ```rust
> async fn run_build<E>(builder: LocalIndexBuilder<E>, request: &SnapshotBuildRequest, records: Vec<ltsearch::models::WalRecord>) -> Result<ltsearch::indexing::BuildIndexResult, IndexError> where E: ltsearch::embedding::EmbeddingGenerator + Send + 'static { let build_request = BuildIndexRequest { version_id: request.version_id, created_at: current_time_millis(), embedding_dim: request.embedding_dim, records }; tokio::task::spawn_blocking(move || builder.build(&build_request)).await.map_err(|error| IndexError::Operation { message: format!("build task panicked: {error}") })? }
> ```
> and call it as `run_build(builder, &request, records).await` in both closures (the `records` are the `Vec` returned by `read_wal_records`). Remove `request.records_placeholder()` — it does not exist. This keeps both closures reading WAL then building, matching the original AWS bin exactly.

- [ ] **Step 2: Verify local build**

Run: `cargo build --no-default-features --features local --bin index_builder_server`
Expected: PASS.

- [ ] **Step 3: Verify aws build (regression guard)**

Run: `cargo build --no-default-features --features aws --bin index_builder_server`
Expected: PASS. (This proves the refactor of the AWS closures into shared helpers preserved behavior.)

- [ ] **Step 4: Clippy on both profiles**

Run: `cargo clippy --no-default-features --features local --bin index_builder_server -- -D warnings`
Then: `cargo clippy --no-default-features --features aws --bin index_builder_server -- -D warnings`
Expected: both PASS.

- [ ] **Step 5: Update Cargo.toml gate + commit**

Change `index_builder_server` `required-features = ["aws"]` → `["server"]`.

```bash
git add src/bin/index_builder_server.rs Cargo.toml
git commit -m "feat(local): drive index_builder_server via filesystem build queue under local profile"
```

---

## Task 6: e2e flow script

**Files:**
- Create: `scripts/e2e/run-local-server-flow.sh`

**Interfaces:**
- Consumes: the three `local`-profile binaries built at `target/debug/{write_server,index_builder_server,query_server}` (or a path passed via env). Drives HTTP: `POST /write`, `POST /query`, `GET /health` (routes from `write_router`/`query_router`/`build_router`).

Reference the existing `scripts/e2e/run-http-server-flow.sh` for the request/response JSON shapes (`/write` body `{"operation":"ingest","documents":[…]}`; `/query` body and hit assertion). The write payload doc shape must match what that script sends.

- [ ] **Step 1: Write the flow script**

Create `scripts/e2e/run-local-server-flow.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

# Moto-free local-profile e2e: launches write/index-builder/query servers as native
# processes sharing one root dir, then drives write→build→query over HTTP. No AWS,
# no Docker, no model download (fixed embeddings).

ROOT="$(mktemp -d)"
BIN_DIR="${BIN_DIR:-target/debug}"
PORT_W=18090
PORT_B=18091
PORT_Q=18092
EMBED_DIM=4
FIXED_EMBED="0.1,0.2,0.3,0.4"   # length must equal EMBED_DIM

pids=()
cleanup() {
  for pid in "${pids[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$ROOT"
}
trap cleanup EXIT

export LTSEARCH_LOCAL_ROOT="$ROOT"
export LTSEARCH_QUERY_ARTIFACT_ROOT="$ROOT"
export LTSEARCH_BUILD_EMBEDDING_DIM="$EMBED_DIM"
export LTSEARCH_BUILD_EMBEDDING_PROVIDER="fixed"
export LTSEARCH_BUILD_FIXED_EMBEDDING="$FIXED_EMBED"
export LTSEARCH_QUERY_EMBEDDING_PROVIDER="fixed"
export LTSEARCH_QUERY_FIXED_EMBEDDING="$FIXED_EMBED"

start() { # name port binary
  LTSEARCH_HTTP_PORT="$2" "$BIN_DIR/$3" &
  pids+=("$!")
}

wait_health() { # port
  for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$1/health" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  echo "server on :$1 never became healthy" >&2
  return 1
}

start write        "$PORT_W" write_server
start index_builder "$PORT_B" index_builder_server
start query        "$PORT_Q" query_server
wait_health "$PORT_W"
wait_health "$PORT_B"
wait_health "$PORT_Q"

# 1) Write a batch.
curl -sf -X POST "http://127.0.0.1:$PORT_W/write" \
  -H 'content-type: application/json' \
  -d '{"operation":"ingest","documents":[
        {"doc_id":"d1","text":"alpha bravo charlie","metadata":{}},
        {"doc_id":"d2","text":"delta echo foxtrot","metadata":{}}
      ]}' >/dev/null

# 2) Wait for the builder to publish v1, observed via a successful query hit.
query_hits() { # expected_doc_id
  curl -sf -X POST "http://127.0.0.1:$PORT_Q/query" \
    -H 'content-type: application/json' \
    -d '{"query":"alpha","top_k":5}' 2>/dev/null | grep -q "$1"
}

ok=0
for _ in $(seq 1 60); do
  if query_hits "d1"; then ok=1; break; fi
  sleep 0.5
done
[ "$ok" = 1 ] || { echo "v1 never became queryable" >&2; exit 1; }

# 3) Second write → wait for v2 → assert both batches queryable.
curl -sf -X POST "http://127.0.0.1:$PORT_W/write" \
  -H 'content-type: application/json' \
  -d '{"operation":"ingest","documents":[
        {"doc_id":"d3","text":"golf hotel india","metadata":{}}
      ]}' >/dev/null

ok=0
for _ in $(seq 1 60); do
  if query_hits "d3"; then ok=1; break; fi
  sleep 0.5
done
[ "$ok" = 1 ] || { echo "v2 never became queryable" >&2; exit 1; }

# d1 must still be queryable after v2 (snapshot replay keeps prior docs).
query_hits "d1" || { echo "d1 lost after v2 rebuild" >&2; exit 1; }

echo "local-profile e2e passed"
```

> **Implementer note:** confirm the exact `/write` and `/query` request/response JSON against `scripts/e2e/run-http-server-flow.sh` and the handlers in `src/http/write.rs`/`src/http/query.rs` before finalizing. Adjust field names (`documents`/`metadata`/`query`/`top_k`) and the hit-assertion (`grep` target) to match the real contract. The doc `text` values are chosen so the fixed-embedding retrieval returns them; if the query handler ranks purely by keyword/BM25 for the fixed provider, keep the query term present in the target doc's text.

- [ ] **Step 2: Make executable**

Run: `chmod +x scripts/e2e/run-local-server-flow.sh`

- [ ] **Step 3: Build the three local bins**

Run: `cargo build --no-default-features --features local --bin write_server --bin index_builder_server --bin query_server`
Expected: PASS.

- [ ] **Step 4: Run the flow locally**

Run: `bash scripts/e2e/run-local-server-flow.sh`
Expected: prints `local-profile e2e passed`, exit 0. If it hangs at health or query, inspect the server stderr (they log to the terminal) and correct the JSON contract per the implementer note.

- [ ] **Step 5: Commit**

```bash
git add scripts/e2e/run-local-server-flow.sh
git commit -m "test(e2e): moto-free native-process local-profile write→build→query flow"
```

---

## Task 7: `local-e2e` GHA job

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `scripts/e2e/run-local-server-flow.sh` (Task 6).

- [ ] **Step 1: Add the job**

In `.github/workflows/ci.yml`, add a new top-level job (sibling of `fast`/`feature-matrix`, no `needs:`):

```yaml
  local-e2e:
    runs-on: ubuntu-24.04-arm
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v6
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          cache: true
      - name: Build local-profile servers (AWS-free)
        run: cargo build --no-default-features --features local
          --bin write_server --bin index_builder_server --bin query_server
      - name: Run local-profile e2e
        run: bash scripts/e2e/run-local-server-flow.sh
```

- [ ] **Step 2: Validate workflow YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('yaml ok')"`
Expected: `yaml ok`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add standalone local-e2e job (AWS-free HTTP write→build→query)"
```

- [ ] **Step 4: Push and confirm the job is green on the PR**

Push the branch and open/refresh the PR; confirm the `local-e2e` check passes on the runner. This is the end-to-end verification that the local profile serves over HTTP with no AWS.

---

## Self-Review

**Spec coverage:**
- "cfg-select backend inside existing server bins; relax required-features" → Tasks 3, 4, 5. ✓
- "run_local_worker_loop mirroring run_sqs_worker_loop" → refined: spawn provider-neutral `run_build_job_loop` directly (Task 5). ✓ (documented deviation)
- "native processes, one shared root dir" → Task 6 script + `LTSEARCH_LOCAL_ROOT`/`LTSEARCH_QUERY_ARTIFACT_ROOT`. ✓
- "fixed embeddings, no ltembed / no model" → Task 6 env + Global Constraints. ✓
- "new flow script + standalone local-e2e job" → Tasks 6, 7. ✓
- Non-goals (SQLite, single-image, docker, restarts) → untouched. ✓

**Placeholder scan:** the `records_placeholder()` in Task 5 Step 1 is intentionally flagged and corrected in the adjacent implementer note (there is no such method; the note gives the real `run_build` signature threading `records: Vec<WalRecord>`). The two implementer notes (Task 5 records threading, Task 6 JSON contract) direct the engineer to verify concrete contracts against named source files — these are verification gates, not vague placeholders.

**Type consistency:** binary/local-impl signatures (`LocalFsWalStorage::new(impl Into<PathBuf>)`, `LocalFsBuildQueue::new` → appends `queue/`, `LocalFsPublishStorage::new`, `run_build_job_loop<C: BuildJobSource, S: PublishStorage>(source, state, storage, list_wal_keys)`, `BuildServerState`/`BuildFn`/`PublishFn`, `SnapshotBuildRequest { batch_id, wal_keys, version_id, embedding_dim }`) match the extracted source signatures. `LTSEARCH_LOCAL_ROOT` is introduced in Task 2 and consumed in Tasks 4–6 consistently.
