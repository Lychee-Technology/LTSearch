# AWS-Optional Runtime Profiles Implementation Plan (Issue #107)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the crate into explicit `local` / `aws` / `lambda` (+ existing `ltembed`) cargo build profiles so the domain core compiles with zero AWS SDK / AWS config / Lambda runtime dependencies, while the existing AWS adapter path keeps compiling and deploying unchanged.

**Architecture:** Make the four AWS/Lambda crates optional. Remove the four places where AWS types leak into library (non-`bin`) code by (a) reusing the four provider-neutral contracts that already exist — `WalStorage` (document events), `BuildQueue` (build jobs), `PublishStorage` (artifact access), `ManifestStore` (active-release coordination) — and (b) adding two small consumer/sync contracts to cover the last leaks. Provide filesystem/in-memory local implementations of every contract so a `local`-profile runtime can be constructed with no AWS. Prove both profiles construct their runtime via two feature-gated integration tests. Flip the default profile to `local` and update all CI/scripts/Docker to name their profile explicitly.

**Tech Stack:** Rust 1.94, cargo features, `async-trait`, `tokio`, `axum` (server), `aws-config`/`aws-sdk-s3`/`aws-sdk-sqs` (aws), `lambda_runtime` (lambda), `ltembed` (embedding).

## Global Constraints

- **Toolchain:** Rust `1.94.0` (`rust-toolchain.toml`, do not change).
- **Default profile after this issue:** `default = ["local"]`. Bare `cargo build` / `cargo test` must compile the AWS-free path only.
- **Feature set (exact):**
  - `default = ["local"]`
  - `local = ["server"]`
  - `server = ["dep:axum"]`
  - `aws = ["server", "dep:aws-config", "dep:aws-sdk-s3", "dep:aws-sdk-sqs"]`
  - `lambda = ["aws", "dep:lambda_runtime"]`
  - `ltembed = ["dep:ltembed"]` (unchanged)
- **Local build graph invariant (acceptance criterion 1):** `cargo tree --no-default-features --features local -i aws-config` (and `-i aws-sdk-s3`, `-i aws-sdk-sqs`, `-i lambda_runtime`) must each report the package is **not** in the graph.
- **AWS/Lambda semantics unchanged (acceptance criterion 2):** the six `src/bin/*` binaries and `turbo_index_builder` produce byte-for-byte equivalent runtime behavior; only their `required-features` gate and their `#[cfg]`-gated wiring change. No env var names, no S3/SQS call shapes change.
- **No infrastructure types in the core (acceptance criterion 3):** no `aws_config::`, `aws_sdk_s3::`, `aws_sdk_sqs::`, or `lambda_runtime::` path may appear outside `#[cfg(feature = "aws")]` / `#[cfg(feature = "lambda")]` blocks or `src/bin/` binaries gated by those features.
- **Feature-matrix CI (acceptance criterion 4):** CI must build+test `local`, build+test `aws`, and build `lambda`, and fail if any regresses.
- **Migration ordering rule:** every intermediate commit must leave `cargo build --features aws,lambda` green. The default flip to `local` happens only in the final tooling task, so early commits keep bare `cargo build` building everything via a temporary permissive default.
- **Chinese doc-comment style:** this repo writes module/function doc-comments in Chinese for domain logic; match the surrounding file's language and density when editing.

---

## File Structure

New files:
- `src/contracts.rs` — provider-neutral contract facade: re-exports the four existing traits under one documented surface and defines the two new contracts (`BuildJobSource`, `ArtifactSync`).
- `src/local/mod.rs` — local (AWS-free) contract implementations module root.
- `src/local/fs_wal.rs` — `LocalFsWalStorage: WalStorage` (filesystem WAL).
- `src/local/fs_build_queue.rs` — `LocalFsBuildQueue: BuildQueue + BuildJobSource` (filesystem/JSONL queue).
- `src/local/fs_publish.rs` — `LocalFsPublishStorage: PublishStorage` (filesystem artifact store with mtime/hash etag).
- `src/local/noop_sync.rs` — `NoopArtifactSync: ArtifactSync` (local artifacts already on disk).
- `tests/runtime_local_test.rs` — `#![cfg(feature = "local")]` construction proof.
- `tests/runtime_aws_test.rs` — `#![cfg(feature = "aws")]` construction proof.
- `docs/adr/0001-aws-optional-runtime-profiles.md` — the decision record.

Modified files:
- `Cargo.toml` — optional deps, `[features]`, explicit `[[bin]]` with `required-features`.
- `src/lib.rs` — gate modules, add `contracts`, `local`.
- `src/adapters/mod.rs` + `src/adapters/{s3_publish,s3_wal,sqs_build_queue}.rs` — gate behind `aws`.
- `src/bootstrap.rs` — split neutral config from AWS client builders.
- `src/build_worker.rs` — extract neutral job loop; gate SQS loop behind `aws`.
- `src/query_service.rs` — extract `ArtifactSync`-driven sync; gate S3 impl behind `aws`.
- `src/index/static_source.rs` + `src/index/mod.rs` — split neutral parser from gated S3 fetch.
- `.github/workflows/ci.yml`, `.github/workflows/publish-images.yml` — feature-matrix + explicit `--features`.
- `scripts/verify-fast.sh`, `scripts/verify-moto.sh` — explicit `--features`.
- `sam/builder.Dockerfile`, root `Dockerfile` — explicit `--features`.
- `tests/test_ci_workflow.py`, `tests/test_readme_workflow.py` — update structural guards to the new CI shape.
- `CONTEXT.md`, `docs/deployment.md`, `README.md` — document the profiles.

---

### Task 1: Feature scaffolding + optional deps + explicit binaries

Introduce all features and make the four crates optional, but keep a **temporary permissive default** (`["aws", "lambda"]`) so bare `cargo build` keeps compiling everything while later tasks fix leaks. Declare every binary explicitly so `required-features` can gate them.

**Files:**
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: the feature names `local`, `server`, `aws`, `lambda` (used by every later `#[cfg]`), and per-binary `required-features` gates.

- [ ] **Step 1: Rewrite the `[features]` and dependency optionality**

Replace the `[features]` block and mark the four crates optional in `[dependencies]`:

```toml
[features]
# Temporary permissive default during the refactor; Task 13 flips this to ["local"].
default = ["aws", "lambda"]
local = ["server"]
server = ["dep:axum"]
aws = ["server", "dep:aws-config", "dep:aws-sdk-s3", "dep:aws-sdk-sqs"]
lambda = ["aws", "dep:lambda_runtime"]
ltembed = ["dep:ltembed"]
```

In `[dependencies]`, change these four lines to optional (leave every other dependency untouched):

```toml
axum = { version = "0.8", optional = true }
aws-config = { version = "1", optional = true }
aws-sdk-s3 = { version = "1", optional = true }
aws-sdk-sqs = { version = "1", optional = true }
lambda_runtime = { version = "0.13", optional = true }
```

- [ ] **Step 2: Declare every binary with `required-features`**

Keep the existing `[[bin]] turbo_index_builder`, and add the six auto-discovered binaries explicitly so they can be gated. Append to `Cargo.toml`:

```toml
[[bin]]
name = "turbo_index_builder"
path = "src/bin/turbo_index_builder.rs"
required-features = ["aws"]

[[bin]]
name = "query_lambda"
path = "src/bin/query_lambda.rs"
required-features = ["lambda"]

[[bin]]
name = "write_lambda"
path = "src/bin/write_lambda.rs"
required-features = ["lambda"]

[[bin]]
name = "index_builder_lambda"
path = "src/bin/index_builder_lambda.rs"
required-features = ["lambda"]

[[bin]]
name = "query_server"
path = "src/bin/query_server.rs"
required-features = ["aws"]

[[bin]]
name = "write_server"
path = "src/bin/write_server.rs"
required-features = ["aws"]

[[bin]]
name = "index_builder_server"
path = "src/bin/index_builder_server.rs"
required-features = ["aws"]
```

> Note: the three `*_server` binaries stay `aws`-gated in this issue. The AWS-free local server binaries (SQLite-backed) are #108's deliverable. #107 proves the local runtime constructs via a test, not a shipped local server binary.

- [ ] **Step 3: Verify the AWS+Lambda graph still builds**

Run: `cargo build --features aws,lambda`
Expected: compiles (bare `cargo build` also works via the temporary default). No source changed yet, so this is a pure Cargo-config smoke.

- [ ] **Step 4: Verify the local graph currently fails (documents remaining work)**

Run: `cargo build --no-default-features --features local 2>&1 | grep -c "aws_\|lambda_runtime"`
Expected: a non-zero count — the library still references AWS. Tasks 6–11 drive this to a clean build.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml
git commit -m "build: introduce local/aws/lambda feature profiles (permissive default)"
```

---

### Task 2: Provider-neutral contract facade + two new contracts

Create `src/contracts.rs`: a single documented surface that re-exports the four existing AWS-free traits and defines the two contracts needed to remove the remaining library leaks (`BuildJobSource` for the build-jobs consumer side, `ArtifactSync` for query-side artifact access).

**Files:**
- Create: `src/contracts.rs`
- Modify: `src/lib.rs`
- Test: `tests/contracts_facade_test.rs`

**Interfaces:**
- Consumes: `crate::write::WalStorage`, `crate::write::BuildQueue`, `crate::indexing::PublishStorage`, `crate::storage::ManifestStore` (all already defined, all AWS-free).
- Produces:
  - `contracts::{WalStorage, BuildQueue, PublishStorage, ManifestStore}` (re-exports)
  - `trait BuildJobSource` with `async fn receive(&self) -> Result<Vec<BuildJob>, String>` and `async fn ack(&self, job: &BuildJob) -> Result<(), String>`
  - `struct BuildJob { pub receipt: String, pub body: String }`
  - `trait ArtifactSync` with `async fn sync(&self, artifact_root: &std::path::Path) -> Result<(), String>`

- [ ] **Step 1: Write the failing facade test**

Create `tests/contracts_facade_test.rs`:

```rust
//! The neutral contract facade must name all six contracts without pulling AWS.

use ltsearch::contracts::{ArtifactSync, BuildJob, BuildJobSource};

#[test]
fn build_job_carries_receipt_and_body() {
    let job = BuildJob {
        receipt: "r-1".to_string(),
        body: "{}".to_string(),
    };
    assert_eq!(job.receipt, "r-1");
    assert_eq!(job.body, "{}");
}

// Compile-only: the facade re-exports the storage contracts under one path.
#[allow(dead_code)]
fn contract_paths_exist() {
    fn assert_impl<T: ?Sized>() {}
    assert_impl::<dyn BuildJobSource>();
    assert_impl::<dyn ArtifactSync>();
    let _ = std::any::type_name::<ltsearch::contracts::PublishStorage>;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test contracts_facade_test`
Expected: FAIL — `unresolved import ltsearch::contracts`.

- [ ] **Step 3: Create the contracts module**

Create `src/contracts.rs`:

```rust
//! 供应商中立契约门面（provider-neutral contracts）。
//!
//! #107 的核心不引入新抽象，而是把已经存在、且不含任何基础设施类型的四个契约
//! 收敛到一个入口，并补齐两个尚缺的消费侧契约，使 domain core 在没有 AWS 的前提
//! 下也能被完整构造。四类契约对应 issue 的四个语义：
//!
//! - 文档事件（document events）→ [`WalStorage`]
//! - 构建作业（build jobs）→ [`BuildQueue`]（生产侧）+ [`BuildJobSource`]（消费侧）
//! - 制品访问（artifact access）→ [`PublishStorage`]（读写）+ [`ArtifactSync`]（查询侧下载）
//! - 活跃版本协调（active-release coordination）→ [`ManifestStore`]

use async_trait::async_trait;
use std::path::Path;

pub use crate::indexing::PublishStorage;
pub use crate::storage::ManifestStore;
pub use crate::write::{BuildQueue, WalStorage};

/// 构建队列上的一条待处理作业：`receipt` 是删除/确认所需的句柄（SQS receipt
/// handle 或本地文件名），`body` 是原始 JSON（`QueueBatch` 的序列化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildJob {
    pub receipt: String,
    pub body: String,
}

/// 构建作业消费侧契约：worker 轮询循环只依赖它，不再直接触碰 SQS。AWS 实现见
/// `#[cfg(feature = "aws")]` 的 `SqsBuildJobSource`，本地实现见 `LocalFsBuildQueue`。
#[async_trait]
pub trait BuildJobSource: Send + Sync {
    /// 拉取零个或多个待处理作业（长轮询实现可阻塞至超时）。
    async fn receive(&self) -> Result<Vec<BuildJob>, String>;
    /// 处理完成后确认（删除）一条作业，无论处理成功与否都应调用。
    async fn ack(&self, job: &BuildJob) -> Result<(), String>;
}

/// 查询侧制品访问契约：把活跃版本所需的 index/lance/static 制品同步到本地
/// `artifact_root`。AWS 实现从 S3 下载前缀；本地实现（制品已在盘上）是 no-op。
#[async_trait]
pub trait ArtifactSync: Send + Sync {
    async fn sync(&self, artifact_root: &Path) -> Result<(), String>;
}
```

- [ ] **Step 4: Register the module**

In `src/lib.rs`, add `pub mod contracts;` (keep alphabetical order — insert between `pub mod build_worker;` and `pub mod embedding;`):

```rust
pub mod contracts;
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --test contracts_facade_test --features aws,lambda`
Expected: PASS (2 tests / compile OK).

- [ ] **Step 6: Commit**

```bash
git add src/contracts.rs src/lib.rs tests/contracts_facade_test.rs
git commit -m "feat(contracts): add neutral contract facade with BuildJobSource + ArtifactSync"
```

---

### Task 3: Local filesystem WAL storage

Implement `WalStorage` on a filesystem-backed store so the local profile has a document-event sink with no AWS.

**Files:**
- Create: `src/local/mod.rs`, `src/local/fs_wal.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)]` in `src/local/fs_wal.rs`

**Interfaces:**
- Consumes: `crate::write::WalStorage` (`async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError>`, `async fn read(&self, key: &str) -> Result<Vec<u8>, IngestError>`), `crate::error::IngestError`.
- Produces: `local::LocalFsWalStorage` with `pub fn new(root: impl Into<PathBuf>) -> Self`.

- [ ] **Step 1: Write the failing test**

Add to a new file `src/local/fs_wal.rs` (module + test together):

```rust
//! 文件系统 WAL：把 `key`（形如 `wal/2026/07/14/batch-<uuid>.jsonl`）当作
//! `root` 下的相对路径落盘。本地单进程场景不需要 S3 的条件写；append 直接创建
//! 父目录并写文件，read 读回。

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::IngestError;
use crate::write::WalStorage;

#[derive(Debug, Clone)]
pub struct LocalFsWalStorage {
    root: PathBuf,
}

impl LocalFsWalStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl WalStorage for LocalFsWalStorage {
    async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| IngestError::Storage {
                    message: format!("failed to create WAL dir for {key}: {error}"),
                })?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|error| IngestError::Storage {
                message: format!("failed to write WAL {key}: {error}"),
            })
    }

    async fn read(&self, key: &str) -> Result<Vec<u8>, IngestError> {
        tokio::fs::read(self.path_for(key))
            .await
            .map_err(|error| IngestError::Storage {
                message: format!("failed to read WAL {key}: {error}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_then_read_round_trips_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let wal = LocalFsWalStorage::new(dir.path());
        let key = "wal/2026/07/14/batch-abc.jsonl";

        wal.append(key, b"{\"doc\":1}\n").await.unwrap();
        let bytes = wal.read(key).await.unwrap();

        assert_eq!(bytes, b"{\"doc\":1}\n");
    }
}
```

> Before running: confirm `IngestError` has a `Storage { message: String }` variant. Check with `grep -n "enum IngestError" -A 20 src/error.rs`. If the variant name differs (e.g. `Backend`/`Io`), use that exact variant here and in Tasks 4 and 12 — do not invent a new one.

- [ ] **Step 2: Create the module root and register it**

Create `src/local/mod.rs`:

```rust
//! 本地（AWS-free）契约实现。这些类型只依赖 std / tokio / 文件系统，可在任何
//! profile 下编译；`local` profile 用它们构造 runtime，#108 将以 SQLite 版本
//! 替换耐久事件与版本协调部分。

pub mod fs_wal;

pub use fs_wal::LocalFsWalStorage;
```

In `src/lib.rs` add `pub mod local;` (after `pub mod indexing;`).

- [ ] **Step 3: Run test to verify it fails first, then passes**

Run: `cargo test -p ltsearch fs_wal`
Expected: PASS after the code above compiles. (If you scaffolded the test before the impl, the first run FAILs to compile — that is the red step.)

- [ ] **Step 4: Commit**

```bash
git add src/local/mod.rs src/local/fs_wal.rs src/lib.rs
git commit -m "feat(local): filesystem WalStorage for the local profile"
```

---

### Task 4: Local build queue + job source

Implement both `BuildQueue` (producer) and `BuildJobSource` (consumer) on one filesystem-backed queue: `enqueue` writes a JSON file into a `queue/` dir; `receive` lists+reads them; `ack` deletes the file.

**Files:**
- Create: `src/local/fs_build_queue.rs`
- Modify: `src/local/mod.rs`
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::write::BuildQueue` (`async fn enqueue(&self, batch: QueueBatch) -> Result<(), IngestError>`), `crate::write::QueueBatch`, `crate::contracts::{BuildJob, BuildJobSource}`.
- Produces: `local::LocalFsBuildQueue` with `pub fn new(root: impl Into<PathBuf>) -> Self`.

- [ ] **Step 1: Write the failing test**

Create `src/local/fs_build_queue.rs`:

```rust
//! 文件系统构建队列：`enqueue` 把 `QueueBatch` 序列化成 `queue/<batch_id>.json`；
//! `receive` 读回全部待处理文件，`ack` 删除。同时实现生产侧 `BuildQueue` 与
//! 消费侧 `BuildJobSource`，本地单进程即可闭环 write→build 触发。

use std::path::PathBuf;

use async_trait::async_trait;

use crate::contracts::{BuildJob, BuildJobSource};
use crate::error::IngestError;
use crate::write::{BuildQueue, QueueBatch};

#[derive(Debug, Clone)]
pub struct LocalFsBuildQueue {
    dir: PathBuf,
}

impl LocalFsBuildQueue {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            dir: root.into().join("queue"),
        }
    }
}

#[async_trait]
impl BuildQueue for LocalFsBuildQueue {
    async fn enqueue(&self, batch: QueueBatch) -> Result<(), IngestError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|error| IngestError::Storage {
                message: format!("failed to create local queue dir: {error}"),
            })?;
        let body = serde_json::to_vec(&batch).map_err(|error| IngestError::Storage {
            message: format!("failed to encode queue batch: {error}"),
        })?;
        let path = self.dir.join(format!("{}.json", batch.batch_id));
        tokio::fs::write(&path, body)
            .await
            .map_err(|error| IngestError::Storage {
                message: format!("failed to write queue file: {error}"),
            })
    }
}

#[async_trait]
impl BuildJobSource for LocalFsBuildQueue {
    async fn receive(&self) -> Result<Vec<BuildJob>, String> {
        let mut jobs = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(_) => return Ok(jobs), // empty/absent queue dir → no jobs
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| format!("failed to scan queue dir: {error}"))?
        {
            let path = entry.path();
            let body = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("failed to read queue file: {error}"))?;
            jobs.push(BuildJob {
                receipt: path.to_string_lossy().into_owned(),
                body,
            });
        }
        Ok(jobs)
    }

    async fn ack(&self, job: &BuildJob) -> Result<(), String> {
        tokio::fs::remove_file(&job.receipt)
            .await
            .map_err(|error| format!("failed to ack queue job: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enqueue_then_receive_then_ack() {
        let dir = tempfile::tempdir().unwrap();
        let queue = LocalFsBuildQueue::new(dir.path());
        let batch = QueueBatch {
            batch_id: "batch-1".to_string(),
            wal_key: "wal/2026/07/14/batch-1.jsonl".to_string(),
            ..QueueBatch::default()
        };

        queue.enqueue(batch).await.unwrap();
        let jobs = queue.receive().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].body.contains("batch-1"));

        queue.ack(&jobs[0]).await.unwrap();
        assert!(queue.receive().await.unwrap().is_empty());
    }
}
```

> Before running: confirm `QueueBatch`'s fields and whether it derives `Default` and `Serialize`. Check with `grep -n "struct QueueBatch" -A 12 src/write/api.rs`. If it lacks `Default`, construct all fields explicitly in the test instead of `..QueueBatch::default()`; if it lacks `Serialize`, add `#[derive(Serialize)]` to it (it already derives `Deserialize` per the worker path) in the same commit.

- [ ] **Step 2: Register and test**

Add to `src/local/mod.rs`:

```rust
pub mod fs_build_queue;

pub use fs_build_queue::LocalFsBuildQueue;
```

Run: `cargo test -p ltsearch fs_build_queue`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/local/fs_build_queue.rs src/local/mod.rs
git commit -m "feat(local): filesystem BuildQueue + BuildJobSource"
```

---

### Task 5: Local filesystem publish storage

Implement `PublishStorage` on the filesystem: uploads copy dirs/files under `root`, `read` returns bytes with an etag derived from content hash, `compare_and_swap` guards the pointer file by comparing the stored etag.

**Files:**
- Create: `src/local/fs_publish.rs`
- Modify: `src/local/mod.rs`
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::indexing::{PublishStorage, UploadMode, VersionedObject}`, `crate::error::PublishError`.
- Produces: `local::LocalFsPublishStorage` with `pub fn new(root: impl Into<PathBuf>) -> Self`. Etag = lowercase hex of a stable content hash (use `std::hash`-free approach: hash bytes via `blake3`? Avoid new deps — use a simple FNV-1a over bytes, sufficient for CAS identity within one process/disk).

- [ ] **Step 1: Write the failing test**

Create `src/local/fs_publish.rs`:

```rust
//! 文件系统制品存储：把 `key` 当作 `root` 下相对路径。etag 用内容 FNV-1a 哈希
//! 的十六进制，`compare_and_swap` 比较目标文件当前 etag 与 `expected_etag`
//! 决定是否写入——本地单进程无并发，但保持与 S3 ETag CAS 同构的语义，让
//! `next_version_id` / `IndexPublisher` 的发布路径无需改动即可跑在本地。

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::PublishError;
use crate::indexing::{PublishStorage, UploadMode, VersionedObject};

#[derive(Debug, Clone)]
pub struct LocalFsPublishStorage {
    root: PathBuf,
}

impl LocalFsPublishStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

fn etag_of(bytes: &[u8]) -> String {
    // FNV-1a 64-bit: 稳定、无依赖，仅作本地 CAS 身份用途，非加密强度。
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn copy_tree(source: &Path, dest: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, dest).map(|_| ())
    }
}

#[async_trait]
impl PublishStorage for LocalFsPublishStorage {
    async fn upload_directory(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
        let dest = self.path_for(key);
        if mode == UploadMode::CreateOnly && dest.exists() {
            return Err(PublishError::Conflict {
                message: format!("directory {key} already exists"),
            });
        }
        copy_tree(source, &dest).map_err(|error| PublishError::Storage {
            message: format!("failed to upload dir {key}: {error}"),
        })
    }

    async fn upload_file(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
        let dest = self.path_for(key);
        if mode == UploadMode::CreateOnly && dest.exists() {
            return Err(PublishError::Conflict {
                message: format!("file {key} already exists"),
            });
        }
        copy_tree(source, &dest).map_err(|error| PublishError::Storage {
            message: format!("failed to upload file {key}: {error}"),
        })
    }

    async fn read(&self, key: &str) -> Result<Option<VersionedObject>, PublishError> {
        match std::fs::read(self.path_for(key)) {
            Ok(bytes) => {
                let etag = etag_of(&bytes);
                Ok(Some(VersionedObject { bytes, etag }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(PublishError::Storage {
                message: format!("failed to read {key}: {error}"),
            }),
        }
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_etag: Option<&str>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        let path = self.path_for(key);
        let current = match std::fs::read(&path) {
            Ok(bytes) => Some(etag_of(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(PublishError::Storage {
                    message: format!("failed to read {key} for CAS: {error}"),
                })
            }
        };
        if current.as_deref() != expected_etag {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| PublishError::Storage {
                message: format!("failed to create dir for {key}: {error}"),
            })?;
        }
        std::fs::write(&path, new_value).map_err(|error| PublishError::Storage {
            message: format!("failed to CAS-write {key}: {error}"),
        })?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cas_creates_then_rejects_stale_then_updates() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsPublishStorage::new(dir.path());

        // create (expected None)
        assert!(store.compare_and_swap("index/_head", None, b"v1").await.unwrap());
        // stale expectation is rejected
        assert!(!store.compare_and_swap("index/_head", None, b"v2").await.unwrap());
        // read back etag and CAS with it
        let object = store.read("index/_head").await.unwrap().unwrap();
        assert_eq!(object.bytes, b"v1");
        assert!(store
            .compare_and_swap("index/_head", Some(&object.etag), b"v2")
            .await
            .unwrap());
        assert_eq!(store.read("index/_head").await.unwrap().unwrap().bytes, b"v2");
    }
}
```

> Before running: confirm `PublishError` variant names. Check `grep -n "enum PublishError" -A 20 src/error.rs`. Map "already exists" to whatever variant the S3 adapter uses for `If-None-Match` failures (the worker's `is_publish_cas_conflict` looks for the string `"publish conflict"` on `error_type == "publish_error"`; the CAS-conflict path here returns `Ok(false)`, not an error, so it is fine — but the `CreateOnly` collision must use the same variant the AWS adapter uses). Use the exact existing variants; do not add new ones.

- [ ] **Step 2: Register and test**

Add to `src/local/mod.rs`:

```rust
pub mod fs_publish;

pub use fs_publish::LocalFsPublishStorage;
```

Run: `cargo test -p ltsearch fs_publish`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/local/fs_publish.rs src/local/mod.rs
git commit -m "feat(local): filesystem PublishStorage with FNV etag CAS"
```

---

### Task 6: Decouple the build worker loop from SQS

Extract a neutral `run_build_job_loop<C: BuildJobSource, S: PublishStorage>` that drives `receive → process_queue_message → ack`. Keep `run_sqs_worker_loop` as an `aws`-gated thin wrapper that constructs an SQS-backed `BuildJobSource` and calls the neutral loop. This removes the `aws_sdk_sqs` leak from library code.

**Files:**
- Modify: `src/build_worker.rs`
- Create: `src/adapters/sqs_job_source.rs` (aws-gated `SqsBuildJobSource: BuildJobSource`)
- Modify: `src/adapters/mod.rs`
- Test: inline `#[cfg(test)]` in `src/build_worker.rs` (neutral loop with a fake source)

**Interfaces:**
- Consumes: `contracts::{BuildJob, BuildJobSource}`, existing `process_queue_message`, `BuildServerState`, `ListWalKeysFn`, `PublishStorage`.
- Produces: `pub async fn run_build_job_loop<C: BuildJobSource, S: PublishStorage>(source: C, state: BuildServerState, storage: S, list_wal_keys: ListWalKeysFn)`; `#[cfg(feature = "aws")] pub async fn run_sqs_worker_loop(sqs: aws_sdk_sqs::Client, queue_url: String, state: BuildServerState, storage: S, list_wal_keys: ListWalKeysFn)` (unchanged public signature).

- [ ] **Step 1: Write the failing test for the neutral loop**

Add to the `#[cfg(test)]` module in `src/build_worker.rs` a fake source that yields one job then empties, and assert the loop processes and acks it. Because the real loop is infinite, expose a bounded variant for testing: `run_build_job_loop_once` that drains one `receive()` batch and returns how many jobs it acked.

```rust
#[cfg(test)]
mod job_loop_tests {
    use super::*;
    use crate::contracts::{BuildJob, BuildJobSource};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct OnceSource {
        jobs: Mutex<Vec<BuildJob>>,
        acked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl BuildJobSource for OnceSource {
        async fn receive(&self) -> Result<Vec<BuildJob>, String> {
            Ok(std::mem::take(&mut *self.jobs.lock().unwrap()))
        }
        async fn ack(&self, job: &BuildJob) -> Result<(), String> {
            self.acked.lock().unwrap().push(job.receipt.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn loop_once_acks_every_received_job_even_on_processing_error() {
        let source = OnceSource {
            jobs: Mutex::new(vec![BuildJob {
                receipt: "r-1".to_string(),
                body: "not-json".to_string(), // forces process error; must still ack
            }]),
            acked: Mutex::new(Vec::new()),
        };
        // A build state + storage that are never actually reached because parsing
        // fails first. Use the crate's existing test helpers to construct them.
        let state = crate::http::build::test_support::empty_build_server_state();
        let storage = crate::local::LocalFsPublishStorage::new(std::env::temp_dir());
        let list: ListWalKeysFn = std::sync::Arc::new(|| Box::pin(async { Ok(vec![]) }));

        let acked = run_build_job_loop_once(&source, &state, &storage, &list).await;

        assert_eq!(acked, 1);
        assert_eq!(&*source.acked.lock().unwrap(), &["r-1".to_string()]);
    }
}
```

> Note: `crate::http::build::test_support::empty_build_server_state()` may not exist. If constructing `BuildServerState` in a test is awkward, instead make `run_build_job_loop_once` generic over a processing closure `FnMut(&str) -> Fut` in the loop's testable core, and test that core; keep the concrete `run_build_job_loop` calling `process_queue_message`. Pick whichever keeps the test AWS-free and the production path calling `process_queue_message`. Do not skip the "acks even on error" assertion — it is the invariant the SQS loop guarantees today.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ltsearch job_loop_tests`
Expected: FAIL — `run_build_job_loop_once` not defined.

- [ ] **Step 3: Implement the neutral loop and bounded helper**

In `src/build_worker.rs`, add (keeping `process_queue_message` unchanged):

```rust
use crate::contracts::{BuildJob, BuildJobSource};

/// 中立构建作业循环：`receive → process → ack`。无论处理成败都 ack（本地单用户
/// 不做毒消息隔离，与既有 SQS 行为一致）。receive 出错退避 5s 再试。
pub async fn run_build_job_loop<C: BuildJobSource, S: PublishStorage>(
    source: C,
    state: BuildServerState,
    storage: S,
    list_wal_keys: ListWalKeysFn,
) {
    loop {
        match run_build_job_loop_once(&source, &state, &storage, &list_wal_keys).await {
            _acked => {}
        }
    }
}

/// 处理一批 receive 结果，返回 ack 的作业数（供单测断言，避免无限循环）。
pub async fn run_build_job_loop_once<C: BuildJobSource, S: PublishStorage>(
    source: &C,
    state: &BuildServerState,
    storage: &S,
    list_wal_keys: &ListWalKeysFn,
) -> usize {
    let jobs = match source.receive().await {
        Ok(jobs) => jobs,
        Err(error) => {
            eprintln!("build worker: receive failed: {error}");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            return 0;
        }
    };
    let mut acked = 0;
    for job in &jobs {
        match process_queue_message(state, storage, list_wal_keys, &job.body).await {
            Ok(version_id) => eprintln!("build worker: published index version {version_id}"),
            Err(error) => eprintln!(
                "build worker: message processing failed (job acked after logging): {error}"
            ),
        }
        if let Err(error) = source.ack(job).await {
            eprintln!("build worker: ack failed: {error}");
        } else {
            acked += 1;
        }
    }
    acked
}
```

- [ ] **Step 4: Move the SQS specifics into an aws-gated adapter**

Create `src/adapters/sqs_job_source.rs`:

```rust
//! `BuildJobSource` 的 SQS 实现：`receive` 长轮询取至多 1 条，`ack` 用
//! receipt handle 删除。原 `run_sqs_worker_loop` 的 SQS 细节收敛到这里，worker
//! 循环本身改走中立契约。

use async_trait::async_trait;
use aws_sdk_sqs::Client as SqsClient;

use crate::contracts::{BuildJob, BuildJobSource};

#[derive(Clone)]
pub struct SqsBuildJobSource {
    client: SqsClient,
    queue_url: String,
}

impl SqsBuildJobSource {
    pub fn new(client: SqsClient, queue_url: impl Into<String>) -> Self {
        Self {
            client,
            queue_url: queue_url.into(),
        }
    }
}

#[async_trait]
impl BuildJobSource for SqsBuildJobSource {
    async fn receive(&self) -> Result<Vec<BuildJob>, String> {
        let output = self
            .client
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(10)
            .send()
            .await
            .map_err(|error| format!("receive_message failed: {error}"))?;
        Ok(output
            .messages
            .unwrap_or_default()
            .into_iter()
            .filter_map(|message| {
                let receipt = message.receipt_handle()?.to_string();
                let body = message.body().unwrap_or_default().to_string();
                Some(BuildJob { receipt, body })
            })
            .collect())
    }

    async fn ack(&self, job: &BuildJob) -> Result<(), String> {
        self.client
            .delete_message()
            .queue_url(&self.queue_url)
            .receipt_handle(&job.receipt)
            .send()
            .await
            .map(|_| ())
            .map_err(|error| format!("delete_message failed: {error}"))
    }
}
```

Add to `src/adapters/mod.rs` (this whole module becomes aws-gated in Task 10; for now add the line):

```rust
pub mod sqs_job_source;
```

Replace the old `run_sqs_worker_loop` body in `src/build_worker.rs` with an `aws`-gated thin wrapper (keep the same public signature so `index_builder_server.rs` needs no change):

```rust
#[cfg(feature = "aws")]
pub async fn run_sqs_worker_loop<S: PublishStorage>(
    sqs: aws_sdk_sqs::Client,
    queue_url: String,
    state: BuildServerState,
    storage: S,
    list_wal_keys: ListWalKeysFn,
) {
    let source = crate::adapters::sqs_job_source::SqsBuildJobSource::new(sqs, queue_url);
    run_build_job_loop(source, state, storage, list_wal_keys).await;
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p ltsearch job_loop_tests` then `cargo build --features aws,lambda`
Expected: neutral loop test PASSes; AWS build still green (wrapper compiles).

- [ ] **Step 6: Commit**

```bash
git add src/build_worker.rs src/adapters/sqs_job_source.rs src/adapters/mod.rs
git commit -m "refactor(build-worker): drive worker via neutral BuildJobSource, gate SQS behind aws"
```

---

### Task 7: Decouple query-side artifact sync from S3

Introduce `ArtifactSync`-driven syncing in `query_service.rs`. The neutral path takes an `&dyn ArtifactSync` (or a generic) and calls `.sync(artifact_root)`. Move the S3 implementation into an `aws`-gated `S3ArtifactSync`; add the local `NoopArtifactSync`.

**Files:**
- Modify: `src/query_service.rs`
- Create: `src/adapters/s3_artifact_sync.rs` (aws-gated `S3ArtifactSync: ArtifactSync`)
- Create: `src/local/noop_sync.rs` (`NoopArtifactSync: ArtifactSync`)
- Modify: `src/adapters/mod.rs`, `src/local/mod.rs`
- Test: inline test that `NoopArtifactSync` is a no-op; keep existing `synced_artifact_prefixes` test.

**Interfaces:**
- Consumes: `contracts::ArtifactSync`.
- Produces: `local::NoopArtifactSync` (`pub fn new() -> Self`), `#[cfg(feature = "aws")] adapters::s3_artifact_sync::S3ArtifactSync` (`pub fn new(bucket: String) -> Self`, syncs prefixes `["index/", "lance/", "static/"]`). `query_service` exposes `pub async fn sync_artifacts_with<A: ArtifactSync>(sync: &A, artifact_root: &Path) -> Result<(), String>` and keeps `synced_artifact_prefixes()` as a shared `pub(crate)` list used by the S3 impl.

- [ ] **Step 1: Write the failing test**

Create `src/local/noop_sync.rs`:

```rust
//! 本地制品同步：制品已在盘上（挂载卷或本地构建产出），无需下载，`sync` 为
//! no-op。保留契约以便 query 侧代码在 local / aws 之间只换实现不换调用点。

use std::path::Path;

use async_trait::async_trait;

use crate::contracts::ArtifactSync;

#[derive(Debug, Clone, Default)]
pub struct NoopArtifactSync;

impl NoopArtifactSync {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ArtifactSync for NoopArtifactSync {
    async fn sync(&self, _artifact_root: &Path) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_sync_returns_ok_without_touching_disk() {
        let sync = NoopArtifactSync::new();
        sync.sync(Path::new("/definitely/not/created")).await.unwrap();
    }
}
```

- [ ] **Step 2: Run to verify it fails then passes**

Add `pub mod noop_sync; pub use noop_sync::NoopArtifactSync;` to `src/local/mod.rs`.
Run: `cargo test -p ltsearch noop_sync`
Expected: PASS.

- [ ] **Step 3: Extract the neutral sync entrypoint and gate the S3 impl**

In `src/query_service.rs`:
- Change `synced_artifact_prefixes()` to `pub(crate)` so the adapter can share it.
- Replace `sync_query_artifacts_from_s3_if_configured` and `sync_prefix` with an `aws`-gated implementation living in the adapter, and add the neutral entrypoint:

```rust
use crate::contracts::ArtifactSync;

/// 中立同步入口：由调用方注入 profile 对应的 `ArtifactSync`（本地 no-op，AWS
/// 从 S3 下载）。query bin 在 profile 边界处选择实现。
pub async fn sync_artifacts_with<A: ArtifactSync>(
    sync: &A,
    artifact_root: &std::path::Path,
) -> Result<(), String> {
    sync.sync(artifact_root).await
}

pub(crate) fn synced_artifact_prefixes() -> Vec<&'static str> {
    vec!["index/", "lance/", "static/"]
}
```

Create `src/adapters/s3_artifact_sync.rs` and move the existing `sync_prefix` loop (lines 133–192 of `query_service.rs`) into `S3ArtifactSync::sync`, reading `LTSEARCH_QUERY_S3_BUCKET` at construction and calling `aws_config::load_defaults` + `s3_client_from_env` inside `sync`:

```rust
//! `ArtifactSync` 的 S3 实现：把活跃版本所需的 index/lance/static 前缀
//! 下载到本地 `artifact_root`。逻辑原样搬自 query_service 的 sync_prefix。

use std::path::Path;

use async_trait::async_trait;

use crate::bootstrap::s3_client_from_env;
use crate::contracts::ArtifactSync;
use crate::query_service::synced_artifact_prefixes;

pub struct S3ArtifactSync {
    bucket: String,
}

impl S3ArtifactSync {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
        }
    }
}

#[async_trait]
impl ArtifactSync for S3ArtifactSync {
    async fn sync(&self, artifact_root: &Path) -> Result<(), String> {
        std::fs::create_dir_all(artifact_root)
            .map_err(|error| format!("failed to create query artifact root: {error}"))?;
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = s3_client_from_env(&config);
        for prefix in synced_artifact_prefixes() {
            sync_prefix(&client, &self.bucket, prefix, artifact_root).await?;
        }
        Ok(())
    }
}

// Move the existing `sync_prefix(client, bucket, prefix, artifact_root)` fn here verbatim.
```

Update the callers: the query lambda/server binaries currently call `sync_query_artifacts_from_s3_if_configured()`. Change those `aws`-gated bins to construct `S3ArtifactSync` (only if `LTSEARCH_QUERY_S3_BUCKET` is set) and call `sync_artifacts_with`. Keep the "unset bucket → skip" behavior at the bin boundary.

- [ ] **Step 4: Register the adapter and verify AWS build**

Add `pub mod s3_artifact_sync;` to `src/adapters/mod.rs`.
Run: `cargo build --features aws,lambda` then `cargo test -p ltsearch --features aws query_service`
Expected: green; the `synced_artifact_prefixes_include_static_artifacts` test still passes.

- [ ] **Step 5: Commit**

```bash
git add src/query_service.rs src/adapters/s3_artifact_sync.rs src/adapters/mod.rs src/local/noop_sync.rs src/local/mod.rs src/bin/query_lambda.rs src/bin/query_server.rs
git commit -m "refactor(query): sync artifacts via neutral ArtifactSync, gate S3 behind aws"
```

---

### Task 8: Split static-source parsing from S3 fetch

`load_static_chunks_from_s3` is the last library-level `aws_sdk_s3` reference. Split the JSONL parsing (neutral) from the S3 GET (aws-gated).

**Files:**
- Modify: `src/index/static_source.rs`, `src/index/mod.rs`
- Test: inline test for the neutral parser.

**Interfaces:**
- Produces: `pub fn parse_static_source_lines(bytes: &[u8], corpus_type: &CorpusType, origin: &str) -> Result<Vec<StaticChunk>, IndexError>` (neutral); `#[cfg(feature = "aws")] pub async fn load_static_chunks_from_s3(client: &aws_sdk_s3::Client, sources: &[StaticSourceConfig]) -> Result<Vec<StaticChunk>, IndexError>` (unchanged signature, now delegates to the parser).

- [ ] **Step 1: Write the failing parser test**

Add to `src/index/static_source.rs` `#[cfg(test)]`:

```rust
#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parse_skips_blank_lines_and_applies_corpus_type() {
        let jsonl = b"{\"doc_id\":\"d1\",\"text\":\"hello\"}\n\n{\"doc_id\":\"d2\",\"text\":\"world\"}\n";
        let chunks =
            parse_static_source_lines(jsonl, &CorpusType::Legal, "s3://bucket/key").unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].doc_id, "d1");
        assert_eq!(chunks[1].corpus_type, CorpusType::Legal);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ltsearch parse_tests`
Expected: FAIL — `parse_static_source_lines` not defined.

- [ ] **Step 3: Extract the parser; gate the S3 fetch**

In `src/index/static_source.rs`, add the neutral parser and make the (now aws-gated) S3 fetch call it:

```rust
/// 解析 JSONL 静态源字节为 `StaticChunk`；`origin` 仅用于错误信息。无 AWS 依赖，
/// 供本地构建与 S3 构建共用。
pub fn parse_static_source_lines(
    bytes: &[u8],
    corpus_type: &CorpusType,
    origin: &str,
) -> Result<Vec<StaticChunk>, IndexError> {
    let text = std::str::from_utf8(bytes).map_err(|error| IndexError::Operation {
        message: format!("static source {origin} was not valid utf-8: {error}"),
    })?;
    let mut chunks = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let chunk: StaticSourceLine =
            serde_json::from_str(line).map_err(|error| IndexError::Operation {
                message: format!(
                    "failed to parse static source line {} from {origin}: {error}",
                    line_number + 1
                ),
            })?;
        chunks.push(StaticChunk {
            doc_id: chunk.doc_id,
            text: chunk.text,
            metadata: chunk.metadata,
            corpus_type: corpus_type.clone(),
        });
    }
    Ok(chunks)
}
```

Wrap the S3 loader in `#[cfg(feature = "aws")]` and replace its parsing tail with a call to `parse_static_source_lines(body.as_ref(), &source.corpus_type, &format!("s3://{}/{}", source.bucket, source.key))`. Move `use aws_sdk_s3::Client as S3Client;` under `#[cfg(feature = "aws")]`.

In `src/index/mod.rs`, gate the re-export: `#[cfg(feature = "aws")] pub use static_source::load_static_chunks_from_s3;` and keep `pub use static_source::{parse_static_source_lines, StaticSourceConfig, TurboBuildConfig};` neutral.

- [ ] **Step 4: Run tests**

Run: `cargo test -p ltsearch parse_tests` then `cargo build --features aws,lambda`
Expected: parser test PASSes; AWS build green.

- [ ] **Step 5: Commit**

```bash
git add src/index/static_source.rs src/index/mod.rs
git commit -m "refactor(static-source): split neutral parser from aws-gated S3 fetch"
```

---

### Task 9: Split bootstrap into neutral config vs AWS clients

`bootstrap.rs` holds both neutral config parsing (`WriteConfig`, `BuildConfig`, env helpers, embedding provider selection) and AWS client builders (`s3_client_from_env`, `sqs_client_from_env`). Gate the AWS client builders behind `aws`; keep the config + embedding helpers neutral.

**Files:**
- Modify: `src/bootstrap.rs`
- Test: existing neutral tests (`write_config_requires_bucket_and_queue_url`, `build_config_requires_bucket_and_defaults_artifact_root`) must run under `local`.

**Interfaces:**
- Produces (neutral, unchanged signatures): `WriteConfig`, `BuildConfig`, `required_env`, `build_embedding_provider_from_env`, `build_embedding_generator_from_env`, `probe_build_embedding_from_env`.
- Produces (`aws`-gated, unchanged signatures): `s3_client_from_env`, `sqs_client_from_env`.

- [ ] **Step 1: Gate the AWS client builders**

In `src/bootstrap.rs`, wrap `s3_client_from_env` and `sqs_client_from_env` (and their `use` of `aws_config`/`aws_sdk_s3`/`aws_sdk_sqs`) in `#[cfg(feature = "aws")]`. Wrap the `endpoint_overrides_are_applied_without_panicking` test in `#[cfg(feature = "aws")]` too. Leave `WriteConfig`/`BuildConfig` and the embedding helpers untouched.

- [ ] **Step 2: Verify neutral config tests run without AWS**

Run: `cargo test --no-default-features --features local bootstrap 2>&1 | tail -20`
Expected: `write_config_requires_bucket_and_queue_url` and `build_config_requires_bucket_and_defaults_artifact_root` compile and PASS; no `aws_config` symbol referenced.

- [ ] **Step 3: Verify AWS build still green**

Run: `cargo build --features aws,lambda`
Expected: green (client builders + endpoint test compile under `aws`).

- [ ] **Step 4: Commit**

```bash
git add src/bootstrap.rs
git commit -m "refactor(bootstrap): gate AWS client builders behind the aws feature"
```

---

### Task 10: Gate the AWS adapters and lambda/aws-only modules

Gate `src/adapters/*` behind `aws`, and gate any remaining module declarations so the `local` library compiles cleanly. This is the task that flips `cargo build --no-default-features --features local` from red to green for the library.

**Files:**
- Modify: `src/adapters/mod.rs`, `src/lib.rs`
- Test: `cargo build --no-default-features --features local` (library) must succeed.

**Interfaces:**
- Produces: `adapters` module compiled only under `aws`.

- [ ] **Step 1: Gate the adapters module**

In `src/lib.rs`, change `pub mod adapters;` to:

```rust
#[cfg(feature = "aws")]
pub mod adapters;
```

`src/adapters/mod.rs` keeps declaring `s3_publish`, `s3_wal`, `sqs_build_queue`, `sqs_job_source`, `s3_artifact_sync` (all only compiled when the parent is compiled, i.e. under `aws`).

- [ ] **Step 2: Resolve any remaining local-profile compile errors**

Run: `cargo build --no-default-features --features local 2>&1 | tee /tmp/local_build.txt`
For each error, apply the minimal `#[cfg(feature = "aws")]` / `#[cfg(feature = "lambda")]` gate at the exact reference. Expected remaining offenders (verify none survive):
- any `use crate::adapters::...` in non-bin library code (there should be none after Tasks 6–9);
- `build_lambda.rs` / `query_lambda.rs` / `write_lambda.rs` — these are library modules (per `src/lib.rs`) but contain no `lambda_runtime`/AWS types (the report confirms they are pure); they should compile under `local`. If any does reference an AWS type, gate that line.

Iterate until the command exits 0.

- [ ] **Step 3: Verify the local build graph is AWS-free (acceptance criterion 1)**

Run:
```bash
for pkg in aws-config aws-sdk-s3 aws-sdk-sqs lambda_runtime; do
  echo "== $pkg =="
  cargo tree --no-default-features --features local -i "$pkg" 2>&1 | head -3
done
```
Expected: each prints `package ID specification ... did not match any packages` (i.e. absent from the local graph).

- [ ] **Step 4: Verify AWS + Lambda still build**

Run: `cargo build --features aws,lambda`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/adapters/mod.rs
git commit -m "refactor: gate AWS adapters behind aws feature; local library builds AWS-free"
```

---

### Task 11: Verify per-profile binary gating

Confirm each binary compiles only under its declared feature and that the profiles select the intended binary sets.

**Files:**
- None (verification only); fix `required-features` in `Cargo.toml` if a binary fails.

- [ ] **Step 1: Lambda binaries build under lambda**

Run: `cargo build --no-default-features --features lambda --bin query_lambda --bin write_lambda --bin index_builder_lambda`
Expected: green.

- [ ] **Step 2: Server + turbo binaries build under aws**

Run: `cargo build --no-default-features --features aws --bin query_server --bin write_server --bin index_builder_server --bin turbo_index_builder`
Expected: green.

- [ ] **Step 3: Local profile builds the library and its tests, no bins required**

Run: `cargo build --no-default-features --features local`
Expected: green, and produces no AWS/Lambda binary (all are `required-features`-gated). This is expected for #107; local server binaries arrive in #108.

- [ ] **Step 4: Commit (only if Cargo.toml changed)**

```bash
git add Cargo.toml
git commit -m "build: confirm per-profile binary required-features"
```

---

### Task 12: Runtime construction proof tests (both profiles)

Add the two feature-gated integration tests that prove each profile can construct its full runtime wiring. This is the "small executable path" the acceptance criteria require.

**Files:**
- Create: `tests/runtime_local_test.rs`, `tests/runtime_aws_test.rs`

**Interfaces:**
- Consumes: `local::{LocalFsWalStorage, LocalFsBuildQueue, LocalFsPublishStorage, NoopArtifactSync}`, `storage::LocalManifestStore`, `write::{WriteApi, WriteAheadLog}`, `indexing::IndexPublisher`, and (aws) `adapters::{AwsS3WalStorage, AwsPublishStorage, AwsSqsBuildQueue}`.

- [ ] **Step 1: Write the local construction proof**

Create `tests/runtime_local_test.rs`:

```rust
//! local profile 构造证明：用文件系统/内存契约实现组装出 write / build / query
//! 三条链路所需的核心类型，断言构造成功且不触及任何 AWS。
#![cfg(feature = "local")]

use ltsearch::contracts::{ArtifactSync, BuildJobSource, WalStorage};
use ltsearch::local::{
    LocalFsBuildQueue, LocalFsPublishStorage, LocalFsWalStorage, NoopArtifactSync,
};
use ltsearch::storage::LocalManifestStore;

#[tokio::test]
async fn local_profile_constructs_all_four_contract_families() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // document events
    let wal = LocalFsWalStorage::new(root);
    wal.append("wal/2026/07/14/batch-1.jsonl", b"{}\n")
        .await
        .unwrap();

    // build jobs (producer + consumer are the same local type)
    let queue = LocalFsBuildQueue::new(root);
    let jobs = queue.receive().await.unwrap();
    assert!(jobs.is_empty());

    // artifact access
    let publish = LocalFsPublishStorage::new(root);
    assert!(publish
        .compare_and_swap("index/_head", None, b"seed")
        .await
        .unwrap());
    let sync = NoopArtifactSync::new();
    sync.sync(root).await.unwrap();

    // active-release coordination
    let _manifest_store = LocalManifestStore::new(root);
}
```

- [ ] **Step 2: Write the AWS construction proof**

Create `tests/runtime_aws_test.rs`. Construct the AWS adapters against a dummy client (no network — construction only), asserting the types wire together. Reuse the existing Moto harness pattern from `tests/write_build_publish_test.rs` for client construction, but do **not** perform I/O:

```rust
//! aws profile 构造证明：用 AWS 适配器组装出与 local 对应的四类契约实现，断言
//! 构造成功（仅构造，不做网络 I/O）。
#![cfg(feature = "aws")]

use aws_config::BehaviorVersion;
use ltsearch::adapters::{AwsPublishStorage, AwsS3WalStorage, AwsSqsBuildQueue};

#[tokio::test]
async fn aws_profile_constructs_all_adapter_types() {
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region("us-east-1")
        .load()
        .await;
    let s3 = aws_sdk_s3::Client::new(&config);
    let sqs = aws_sdk_sqs::Client::new(&config);

    let _wal = AwsS3WalStorage::new("bucket", s3.clone());
    let _publish = AwsPublishStorage::new("bucket", s3);
    let _queue = AwsSqsBuildQueue::new("http://queue", sqs);
}
```

> Confirm the exact exported names in `src/adapters/mod.rs` (`AwsS3WalStorage`, `AwsPublishStorage`, `AwsSqsBuildQueue`) and their constructor arg order from the dependency map (bucket/queue_url first, client second). Match verbatim.

- [ ] **Step 3: Run both proofs under their profiles**

Run:
```bash
cargo test --no-default-features --features local --test runtime_local_test
cargo test --no-default-features --features aws --test runtime_aws_test
```
Expected: both PASS.

- [ ] **Step 4: Commit**

```bash
git add tests/runtime_local_test.rs tests/runtime_aws_test.rs
git commit -m "test: prove local and aws profiles each construct their runtime"
```

---

### Task 13: Flip default to local + feature-matrix CI + tooling

Flip `default = ["local"]`, add the CI feature-matrix job, and update every build/test/Docker/script command to name its profile explicitly. Update the Python structural guards that assert on `ci.yml`.

**Files:**
- Modify: `Cargo.toml`, `.github/workflows/ci.yml`, `scripts/verify-fast.sh`, `scripts/verify-moto.sh`, `sam/builder.Dockerfile`, root `Dockerfile`, `tests/test_ci_workflow.py`, `tests/test_readme_workflow.py`

- [ ] **Step 1: Flip the default**

In `Cargo.toml`:

```toml
default = ["local"]
```

- [ ] **Step 2: Add a feature-matrix job to `ci.yml`**

Add a `feature-matrix` job (runs alongside `fast`, before `integration`) that proves all three profiles:

```yaml
  feature-matrix:
    runs-on: ubuntu-24.04-arm
    timeout-minutes: 45
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/setup-rust-toolchain@v1
      - name: local profile builds AWS-free
        run: |
          cargo build --no-default-features --features local
          for pkg in aws-config aws-sdk-s3 aws-sdk-sqs lambda_runtime; do
            if cargo tree --no-default-features --features local -i "$pkg" >/dev/null 2>&1; then
              echo "::error::$pkg leaked into the local build graph"; exit 1
            fi
          done
      - name: local profile tests
        run: cargo test --no-default-features --features local --lib --tests
      - name: aws profile builds + tests
        run: |
          cargo build --no-default-features --features aws
          cargo test --no-default-features --features aws --lib
      - name: lambda profile builds
        run: cargo build --no-default-features --features lambda --bin query_lambda --bin write_lambda --bin index_builder_lambda
```

- [ ] **Step 3: Update `verify-fast.sh` to name profiles**

Change the build/test lines in `scripts/verify-fast.sh`:
- lambda-bin build → `cargo build --no-default-features --features lambda --bin query_lambda --bin write_lambda --bin index_builder_lambda`
- unit/integration tests → keep the AWS-free set on `--no-default-features --features local` where they are pure-local, and run the AWS-touching integration tests under `--no-default-features --features aws`. Concretely: split the existing 21-target loop so pure-local targets run under `local` and any target that constructs AWS clients runs under `aws`. (Per the CI survey, all 21 fast targets are pure-local, so they run under `local`; the Moto target stays in `verify-moto.sh`.)
- `cargo fmt --check` unchanged.
- clippy → run per profile: `cargo clippy --no-default-features --features local --all-targets -- -D warnings` and `cargo clippy --no-default-features --features aws,lambda,ltembed --all-targets -- -D warnings` (replaces the single `--all-features` line, which is invalid now that `local`/`aws` are mutually exclusive at link time for some bins).

- [ ] **Step 4: Update `verify-moto.sh` and Dockerfiles**

- `scripts/verify-moto.sh`: `cargo test --no-default-features --features aws --test write_build_publish_test -- --nocapture`.
- `sam/builder.Dockerfile`: the six-binary build lines gain profiles. Stub mode: `cargo build --release --no-default-features --features lambda --bin write_lambda --bin index_builder_lambda --bin query_lambda` and `--no-default-features --features aws --bin write_server --bin index_builder_server --bin query_server`. Real mode: add `,ltembed` to the feature list on both. (Confirm the builder needs both bin groups; keep exactly the six binaries it copies to `/`.)
- root `Dockerfile`: `cargo build --release --no-default-features --features lambda --bin query_lambda`.

- [ ] **Step 5: Update the Python structural guards**

Run the guards first to see what they assert:
Run: `python3 -B tests/test_ci_workflow.py; python3 -B tests/test_readme_workflow.py`
Expected: FAIL (they encode the old `ci.yml`/README shape). Update `tests/test_ci_workflow.py` to expect the new `feature-matrix` job and the profiled cargo lines, and `tests/test_readme_workflow.py` for any README profile documentation added in Task 14. Re-run until PASS.

- [ ] **Step 6: Full local + aws verification**

Run:
```bash
bash scripts/verify-fast.sh
```
Expected: PASS end-to-end under the new default.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml .github/workflows/ci.yml scripts/verify-fast.sh scripts/verify-moto.sh sam/builder.Dockerfile Dockerfile tests/test_ci_workflow.py tests/test_readme_workflow.py
git commit -m "build: default to local profile; add feature-matrix CI; profile-scope all tooling"
```

---

### Task 14: Documentation + ADR

Record the profile split so future work (and #108) reads from a single source of truth.

**Files:**
- Create: `docs/adr/0001-aws-optional-runtime-profiles.md`
- Modify: `CONTEXT.md`, `docs/deployment.md`, `README.md`

- [ ] **Step 1: Write the ADR**

Create `docs/adr/0001-aws-optional-runtime-profiles.md` with: Context (AWS was unconditionally compiled), Decision (four features `local`/`server`/`aws`/`lambda` + `ltembed`; `default = ["local"]`; four neutral contract families with local + AWS impls; two new contracts `BuildJobSource`/`ArtifactSync`), Consequences (bare `cargo build` is AWS-free; every AWS/Lambda command must name its profile; local server binaries deferred to #108), and the exact `cargo tree` invariant that guards the local graph.

- [ ] **Step 2: Update CONTEXT.md**

Add a short "Build profiles" paragraph naming the four features, the `default = ["local"]` rule, and the contract→feature mapping. Keep it to the file's existing concise style.

- [ ] **Step 3: Update deployment.md and README**

- `docs/deployment.md`: note that the AWS SDK and Lambda runtime are now optional features; the server images build under `--features aws`, lambda images under `--features lambda`.
- `README.md`: update any documented `cargo build`/`cargo test` commands to show the profile flags, so `test_readme_workflow.py` and newcomers stay aligned.

- [ ] **Step 4: Verify doc guards**

Run: `python3 -B tests/test_readme_workflow.py`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add docs/adr/0001-aws-optional-runtime-profiles.md CONTEXT.md docs/deployment.md README.md
git commit -m "docs: record AWS-optional runtime profiles (ADR-0001) and update guides"
```

---

## Self-Review

**Spec coverage vs the four acceptance criteria:**

1. *Local build graph contains no AWS SDK/config/Lambda* → Tasks 1 (optional deps), 6–10 (remove the four leaks + gate adapters), 10 Step 3 + 13 Step 2 (`cargo tree` invariant enforced in CI). ✅
2. *AWS and Lambda profiles compile the existing adapter path unchanged* → Tasks 6–9 keep public signatures (`run_sqs_worker_loop`, `load_static_chunks_from_s3`, `s3_client_from_env`) identical; Task 11 verifies each bin builds; migration rule keeps `--features aws,lambda` green at every commit. ✅
3. *Event/job/artifact/release contracts allow local + AWS without infra types in core* → Task 2 (facade + two new contracts), Tasks 3–5, 7 (local impls), Tasks 6–9 (leaks removed). The four families map explicitly: WalStorage / BuildQueue+BuildJobSource / PublishStorage+ArtifactSync / ManifestStore. ✅
4. *Automated feature-matrix checks prevent local/lambda regressing* → Task 13 Step 2 `feature-matrix` CI job (local build+graph-check+test, aws build+test, lambda build). ✅

**Placeholder scan:** every code step carries complete code. The three "before running, confirm variant/field name" notes are deliberate verification guards against unknown exact enum-variant spellings in `IngestError` / `PublishError` / `QueueBatch` — the implementer must read the file and use the exact existing name, not invent one. These are not placeholders for logic.

**Type consistency:** `BuildJob { receipt, body }` used identically in Task 2 (def), Task 6 (SQS source + neutral loop), Task 4 (local queue). `ArtifactSync::sync(&self, &Path) -> Result<(), String>` consistent across Task 2/7. `PublishStorage`/`UploadMode`/`VersionedObject` used per the verbatim trait in the dependency map. Adapter constructor arg order (bucket/queue_url first, client second) matches the map and is reused in Task 12.

**Known verification points for the implementer** (read the file before coding, per the notes): `IngestError` variant used by local WAL/queue; `PublishError` variant for CreateOnly-collision; `QueueBatch` `Default`/`Serialize` derives; `BuildServerState` constructibility in the Task 6 unit test (fall back to testing the loop core over a closure if direct construction is awkward).
