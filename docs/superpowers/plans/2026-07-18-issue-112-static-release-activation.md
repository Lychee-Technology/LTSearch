# Issue #112 — 静态 release 显式激活与查询侧双版本 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务执行。步骤用 `- [ ]` 追踪。
>
> 执行时把本文件存为 `docs/superpowers/plans/2026-07-18-issue-112-static-release-activation.md`（epic 惯例）。

## Context

Epic #106 / 路线图 #132 的 Wave B。#110（PR #136，已合并 `9247b8b`）交付了本地 `static-build`：从 pin 版本 Lance 快照确定性产出自描述、内容寻址的 TurboQuant **v3** release（9 个 `.bin` + `release_manifest.json`），`MmapIndex` 已能加载 v3 并暴露 `version()`/`original_doc_id()`/`metadata_json()`。但 #110 不触碰指针与查询侧：release 建好后无从"激活"；查询侧 `try_load_static_searcher`（`src/query_lambda.rs:252`）仍按目录约定 `<artifact_root>/static` 隐式 mmap 一个 v2 索引——v2 的 `doc_id` 是 FNV u64 哈希、`metadata: None`，导致带 filter 的查询静默丢弃全部静态结果（`src/query/filter.rs:40`），原始 ID/citation 无从谈起。

#112 补齐三件事：① **激活**（与 build 分离的独立命令，验证后 CAS 指针切换）；② **本地 SQLite 与 AWS S3 指针等价 lost-CAS 语义**；③ **查询侧按 `(dynamic_version, static_release_id)` 对解析、缓存、上报，单请求内不混 release，v3 元数据/filter/原始 doc_id/citation 端到端暴露**。

**用户裁决（本次会话确认，勿重议）**：
- **Pointer-only**：彻底删除隐式 `<artifact_root>/static` 目录约定（含 `LTSEARCH_QUERY_STATIC_DIR` env、镜像 `IMAGE_STATIC_DIR=/app/static`、dynamic publish 顺带上传 `static/` 的行为）。查询侧只经"激活后指针解析出的 release"服务静态结果。依赖该约定的 e2e/scripts/镜像位一并修正。
- **完整 AWS 激活路径**：S3 release 存储布局 + AWS 激活入口 + query lambda 解析静态指针并从 S3 拉取 v3 release。AWS v2 孪生 `src/bin/turbo_index_builder.rs` **不动**——可被 AWS 激活的 v3 release 由本地 `static-build` 产出、激活时上传。
- **受管 store**：本地激活把制品装入 `<root>/static/releases/<release_id>/`（同盘 rename，跨盘回退 copy），指针只存 `release_id`。release 不可变；回滚 = CAS 指针指回仍在盘的旧 release。

**Epic #132/#106 既有裁决（binding）**：
- 静态指针用**全新 key/表**——不复用 `active_head` 行 / `INDEX_HEAD_KEY`。CAS 机制（etag + `BEGIN IMMEDIATE` + 恰一 winner）沿用 `src/local/sqlite/head.rs` 模式。
- lost-CAS 语义与 S3 条件写等价（precondition 失败 → `Ok(false)`）。
- local profile 保持 AWS-free（CI feature-matrix 强制）。
- 每任务 TDD；e2e 走 `scripts/e2e/*.sh` + `tests/test_*.py` pytest 结构守卫；PR 不自动合并。
- **MUST-DO 前置（#110 review 记录在案）**：`MmapIndex::load` 只校验 meta_ext 条数，未对 `docid`/`meta_json` blob 的 `offset+len` 做越界检查，损坏 sidecar 会让 v3 访问器 panic——接查询路径前必须硬化。

**Goal:** `static-build` 产出的 v3 release 经独立 `static-activate` 验证（manifest + 输出 hash + Lance provenance + embedding profile）后 CAS 切换静态指针（SQLite/S3 等价 lost-CAS）；查询按 `(dynamic_version, static_release_id)` 对解析/缓存/上报，v3 端到端暴露。

**Architecture:** 复用现成 `PublishStorage` trait（`compare_and_swap`/`upload_directory` 已泛化到任意 key，`AwsPublishStorage` 零改动；`LocalPublishStorage` 增一条新 key→新 SQLite 表的路由）。新增：指针类型 `StaticReleaseHead`（镜像 `ManifestHead` 形态、独立 key/表）、激活编排 `static_publisher`（verify + install + CAS）、读侧 `StaticReleaseStore`（SQLite/文件双实现，镜像 `manifest_store_for` 分发）。查询缓存键从 `u64` 扩为 `(u64, Option<String>)`；`SearchResponse`/`HealthBody` 增 `static_release_id`。AWS 查询侧下载 `static/_head` 指针 + 按 `release_id` 惰性拉 `static/releases/<id>/*`（miss 才下载，镜像 #111 的 S3→/tmp 冷启思路）。

**Tech Stack:** Rust；rusqlite（新表 + IMMEDIATE 事务 CAS）；aws-sdk-s3（条件写 CAS + 前缀下载）；sha2/hex（重算 hash、re-derive release_id，已是直接依赖）；memmap2。

## Global Constraints

- **两个 PR**（对齐 #109 的节奏），每个都让 main 绿且可发布：
  - **PR-1**：`MmapIndex::load` 越界硬化 + 静态指针契约 + 激活命令（本地 CLI + AWS bin）。纯增量，不改查询路径、不删目录约定。分支 `feat/112-static-release-activation`，worktree `../LTSearch-issue-112`。PR 正文引用 #112 但**不** Close。
  - **PR-2**（前置 PR-1 合并）：查询双版本 + v3 端到端暴露 + 目录约定移除 + AWS 查询拉取 + e2e/镜像修正。新分支 `feat/112-dual-version-query`（PR-1 合并、ff main 后新建 worktree）。PR 正文 `Closes #112`。
- 每个 PR 开工前：清理已合并分支 → `git pull --ff-only` → 新 worktree。
- local profile AWS-free：`cargo tree --no-default-features --features local` 不得出现 aws-config/aws-sdk-s3/aws-sdk-sqs/lambda_runtime；每 PR 本地复现核验。
- 不动 AWS v2 孪生：`src/bin/turbo_index_builder.rs`、`load_static_chunks_from_s3`、`StaticIndexBuilder`（v2 writer）及其测试。
- 每任务 TDD：失败测试 → 实现 → 绿 → conventional commit。
- 决定性铁律延续：`StaticReleaseHead` 序列化无 UUID/HashMap；`updated_at` 显式入参（激活时刻）；release_id 是 #110 定死的内容寻址值，激活侧只读不改。
- **协调**：#111（PR #137，S3→/tmp）在途，若先合并需 rebase；两者都碰 lambda 冷启拉取但文件面不同（#111 是模型资产，本 PR 是 `S3ArtifactSync`）。

---

## 设计要点（已定，执行时勿改）

### 1. 静态指针 schema 与 key/表命名（全新，不复用 dynamic）

`src/storage/s3_paths.rs` 新增：

```rust
pub const STATIC_HEAD_KEY: &str = "static/_head";
pub fn static_release_dir_key(release_id: &str) -> String {
    format!("static/releases/{release_id}")
}
pub fn static_release_manifest_key(release_id: &str) -> String {
    format!("static/releases/{release_id}/release_manifest.json")
}
```

`src/storage/static_head.rs`（新）——镜像 `ManifestHead`（`src/storage/head.rs:14-70`）：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticReleaseHead {
    pub release_id: String,     // 64 位 sha256 hex（#110 内容寻址）
    pub manifest_path: String,  // 永远派生自 release_id，不接受调用方传入
    pub updated_at: i64,
}
```

`new(release_id, updated_at)` 派生 `manifest_path`；`validate()` 校验 64 位 hex、path 一致性、`updated_at >= MIN_PLAUSIBLE_EPOCH_MILLIS`；错误类型 `StaticHeadError` 复刻 `HeadError` 风格。

SQLite 新表（`src/local/sqlite/schema.rs::init` 幂等追加）：

```sql
CREATE TABLE IF NOT EXISTS static_release_head (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    head_bytes BLOB NOT NULL
);
```

S3 布局：指针 `static/_head`（小 JSON）；制品 `static/releases/<release_id>/<file>`（10 文件，CreateOnly 不可变）。

`LocalPublishStorage` 路由（`src/local/sqlite/head.rs`）：把 head-row 的 read/CAS 逻辑抽成按表名参数化的私有 helper，`match key { INDEX_HEAD_KEY => "active_head", STATIC_HEAD_KEY => "static_release_head", _ => fs }`。`AwsPublishStorage` 零改动（`compare_and_swap` 已对任意 key 用 If-Match/If-None-Match，`src/adapters/s3_publish.rs:142-169`）。

### 2. 激活命令契约

**本地**（`ltsearch` 子命令，手写旗标解析，对齐 `parse_static_build_args` `src/app.rs:190`）：

```
ltsearch static-activate --release <built_release_dir> --root <localroot> \
                         [--expect-model-id <id>] [--expect-dim <n>]
```

安装到 `<root>/static/releases/<release_id>/`（同盘 `fs::rename`；rename 失败回退"递归 copy 到 `<root>/static/releases/.<id>-staging` 再 rename 就位"，**不依赖** `ErrorKind::CrossesDevices` 判定）；指针写 SQLite `static_release_head`。

**AWS**（新 bin `src/bin/static_activate.rs`，`required-features=["aws"]`）：

```
static_activate --release <built_release_dir> [--expect-model-id <id>] [--expect-dim <n>]
env: LTSEARCH_STATIC_S3_BUCKET（必填，须 == query lambda 的 artifact bucket）
     AWS_ENDPOINT_URL_S3（Moto/LocalStack 覆盖，经 s3_client_from_env）
```

上传 `<release_dir>/*` → `s3://<bucket>/static/releases/<release_id>/`（`upload_directory(..., UploadMode::CreateOnly)`，已存在 → 幂等视为已装），再 CAS `static/_head`。

**验证清单（AC2，两侧共享 `verify_release_dir`）**：
1. 解析 `release_manifest.json` → `ReleaseManifest`；
2. `manifest_schema_version==1 && turbo_version==3`；
3. 逐个重算 `outputs[]`（9 个 `.bin`，name 升序）的 `sha256_hex` + `size_bytes`，与 manifest 比对（**输出 hash**）；
4. `derive_release_id`（`src/index/release_manifest.rs:125`）重算 == `manifest.release_id`（**manifest 完整性**，绑死 profile/codec/content_digest/outputs）；
5. **Lance provenance**：`source.kind=="lance"`、`dataset_path` 非空、`table_version>0`、`table_row_count == input_fingerprint.doc_count`；
6. **embedding profile**：`embedding_profile.dim == codec.dim == 512`、`model_id` 非空；给了 `--expect-*` 则须相等（部署级钉住，缺省仅自洽）；
7. `MmapIndex::load(<dir>)` 成功且 `version()==3` 且 `record_count()==table_row_count`（顺带过 Task 1 硬化后的越界校验）；
8. 通过后返回 `ReleaseManifest`。

**CAS 编排（两侧共享）**：`read(STATIC_HEAD_KEY)` 取现值+etag → `StaticReleaseHead::new` → `compare_and_swap(STATIC_HEAD_KEY, expected_etag, bytes)`；`Ok(false)` → `StaticActivateError::LostCas`。**安装先于 CAS**：输者的 release 已幂等装好、无副作用；回滚 = 用旧 release_id 重跑 activate。

### 3. 查询侧 pair 语义

- 读侧新 trait（`src/storage/static_release_store.rs`）：

  ```rust
  pub trait StaticReleaseStore {
      /// None = 无静态指针（从未激活）；MissingHead 折叠为 Ok(None)。
      fn load_active_release(&self) -> Result<Option<StaticReleaseHead>, StaticReleaseStoreError>;
  }
  ```

  实现：`SqliteStaticReleaseStore`（读 `static_release_head` 行）、`LocalStaticReleaseStore`（读 `<root>/static/_head` 文件——AWS 路径由 sync 把指针落成此文件）。分发器 `static_release_store_for(artifact_root)` 镜像 `manifest_store_for`（`src/query_lambda.rs:73`）。
- 缓存键（`src/query_service.rs`）：`CachedQueryHandler { version_id: u64, static_release_id: Option<String>, handler }`；load 函数扩为 `Fn() -> Result<(u64, Option<String>)>`，bootstrap 扩为 `Fn((u64, Option<String>))`；任一分量变则重建 handler；retriable-on-change 同样覆盖静态指针中途变更。
- **单请求不混 release**：bootstrap（`src/query_lambda.rs:143`）冻结 dynamic manifest（现有 `FixedManifestStore`）**并**冻结 static_release_id 与其 `MmapIndex`；指针中途变化只影响下一次 `resolve_handler`。
- 响应 schema：`SearchResponse`（`src/models/search.rs:182`）增 `pub static_release_id: Option<String>`（`#[serde(default, skip_serializing_if = "Option::is_none")]`）；`QueryRouter::with_static_release_id(Option<String>)`；`HealthBody` 与 `/health` 同步上报。

### 4. v3 暴露（TurboQuantSearcher）与 v2 归宿

`src/query/turbo_searcher.rs` top-K materialization 按 `index.version()` 分支：
- v3：`doc_id = original_doc_id(i)`（原始串）；`metadata = Some(parse(metadata_json(i)))`（→ filter 生效）；`citation = Citation::from_metadata(&metadata)`（`src/models/search.rs:132`，真实 resource_id/source_ref/url）；解析失败保守回退 `metadata: None` + warn，不 panic。
- v2：现行为不变（护栏测试保留）。

**Pointer-only 下 v2 静态事实退役**：v2 无 manifest → `verify_release_dir` 第 1 步即失败 → 无法激活 → 指针永不指向 v2。`MmapIndex` v2 加载与 searcher v2 分支保留（不回归），但经指针路径永不触达。

### 5. AWS 查询拉取

`S3ArtifactSync::sync`（`src/adapters/s3_artifact_sync.rs`）：① 批量同步 `index/`、`lance/`（**去掉** `static/` 批量前缀）；② 下载 `static/_head`（存在才下）到 `<root>/static/_head`；③ 读本地指针得 release_id，若 `<root>/static/releases/<id>/release_manifest.json` 不存在才下载 `static/releases/<id>/*`（按 release_id 缓存，指针不变即命中跳过）。`NoopArtifactSync`（local）不变。

---

## PR-1：MmapIndex 硬化 + 静态指针契约 + 激活命令（本地 + AWS）

> 纯增量：不改查询路径、不删目录约定。合并后可 build→activate，查询行为不变，main 绿。

### Task 0: 开工前置

- [ ] 清理已合并分支；`git -C /Users/ruoshi/code/Lychee/LTBase/LTSearch pull --ff-only`
- [ ] `git worktree add ../LTSearch-issue-112 -b feat/112-static-release-activation`
- [ ] 本计划存入 `docs/superpowers/plans/2026-07-18-issue-112-static-release-activation.md`，首个 commit

### Task 1: MmapIndex::load 越界硬化（MUST-DO 前置）

**Files:**
- Modify: `src/index/mmap_index.rs`（`load` 的 v3 分支 + 新错误变体）
- Test: `tests/mmap_index_test.rs`（追加）

**Interfaces — Produces:**

```rust
MmapIndexError::MetaExtBlobOutOfBounds { index: u64, blob: &'static str },  // blob ∈ {"docid","meta_json"}
MmapIndexError::MetaExtBlobInvalidUtf8 { index: u64, blob: &'static str },
```

- [ ] **Step 1: 失败测试**（fixture 复用 `tests/static_release_builder_test.rs` 的 v3 写盘方式：先 `StaticReleaseBuilder::build_release` 产真 release，再手工篡改 `turbo_static_meta_ext.bin` 注入越界）

```rust
#[test]
fn mmap_index_rejects_v3_with_docid_blob_out_of_bounds() {
    let dir = build_valid_v3_dir("docid-oob");
    patch_meta_ext_record(&dir, 0, |ext| ext.docid_len = 9999); // 越过 docid blob 末尾
    let err = MmapIndex::load(&dir).unwrap_err();
    assert!(matches!(err,
        MmapIndexError::MetaExtBlobOutOfBounds { index: 0, blob: "docid" }));
}

#[test]
fn mmap_index_rejects_v3_with_meta_json_invalid_utf8() {
    let dir = build_valid_v3_dir("json-utf8");
    corrupt_meta_json_blob_to_invalid_utf8(&dir, 0); // len 不变，字节改为 0xFF 0xFE…
    let err = MmapIndex::load(&dir).unwrap_err();
    assert!(matches!(err,
        MmapIndexError::MetaExtBlobInvalidUtf8 { index: 0, blob: "meta_json" }));
}

#[test]
fn mmap_index_v3_accessors_never_panic_on_valid_release() {
    let dir = build_valid_v3_dir("valid");
    let index = MmapIndex::load(&dir).unwrap();
    for i in 0..index.record_count() {
        assert!(index.original_doc_id(i).is_some());
        assert!(index.metadata_json(i).is_some());
    }
}
```

- [ ] **Step 2:** `cargo test --test mmap_index_test` → 失败（当前 load 不校验，访问器才 panic）
- [ ] **Step 3: 实现** — `load` v3 分支在 meta_ext 条数校验后遍历每条 `MetaExtRecord`：`checked_add` 校验 `docid_offset+docid_len <= docid_mmap.len()`、`meta_json_offset+meta_json_len <= meta_json_mmap.len()`，且对应切片 `str::from_utf8` 成功；失败返回新错误变体。访问器保持现签名（load 已保证安全）。
- [ ] **Step 4:** `cargo test --test mmap_index_test` PASS + 全量 `cargo test` 绿（v2/v3 零回归）
- [ ] **Step 5: Commit** `fix(index): MmapIndex::load 硬化 v3 sidecar blob 越界与 UTF-8 校验`

### Task 2: 静态指针 key/路径 + StaticReleaseHead

**Files:**
- Modify: `src/storage/s3_paths.rs`、`src/storage/mod.rs`（导出）
- Create: `src/storage/static_head.rs`
- Test: `static_head.rs` 内联 `#[cfg(test)]`

**Interfaces — Produces:** 「设计要点 1」的 `STATIC_HEAD_KEY`/`static_release_dir_key`/`static_release_manifest_key`/`StaticReleaseHead`/`StaticHeadError`。

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn static_head_roundtrips_and_derives_manifest_path() {
    let head = StaticReleaseHead::new("a".repeat(64), 1_700_000_000_000);
    assert_eq!(head.manifest_path,
        format!("static/releases/{}/release_manifest.json", "a".repeat(64)));
    let parsed = StaticReleaseHead::from_json(head.to_json_pretty().as_bytes()).unwrap();
    assert_eq!(parsed, head);
}

#[test]
fn static_head_rejects_non_hex_release_id() {
    let bad = StaticReleaseHead { release_id: "not-hex".into(),
        manifest_path: static_release_manifest_key("not-hex"),
        updated_at: 1_700_000_000_000 };
    assert!(bad.validate().is_err());
}

#[test]
fn static_head_rejects_manifest_path_mismatch() {
    let head = StaticReleaseHead { release_id: "a".repeat(64),
        manifest_path: "static/releases/wrong/release_manifest.json".into(),
        updated_at: 1_700_000_000_000 };
    assert!(head.validate().is_err());
}
```

- [ ] **Step 2:** 失败 → **Step 3:** 实现（复刻 `ManifestHead`/`HeadError` 风格）→ **Step 4:** PASS → **Step 5: Commit** `feat(storage): StaticReleaseHead 指针类型与静态 S3 key 布局`

### Task 3: SQLite 新表 + LocalPublishStorage 路由静态指针

**Files:**
- Modify: `src/local/sqlite/schema.rs`（`static_release_head` 表）、`src/local/sqlite/head.rs`（read/CAS 抽表名参数化 helper + 新 key 分支）
- Test: `src/local/sqlite/head.rs` 内联（追加）

**Interfaces:**
- Consumes: Task 2 `STATIC_HEAD_KEY`。
- Produces: `LocalPublishStorage` 对 `STATIC_HEAD_KEY` 的 `read`/`compare_and_swap` 路由到 `static_release_head` 行（IMMEDIATE 事务 + FNV-1a etag，与 `active_head` 同机制、不同表）。

- [ ] **Step 1: 失败测试**（追加；另复刻 `src/local/sqlite/head.rs:203` 的跨连接测试为 `concurrent_cross_connection_static_cas_yields_conflict_not_busy_error`，用 `open_two_temp`）

```rust
#[tokio::test]
async fn static_head_cas_is_independent_from_index_head() {
    let (store, _dir) = store();
    assert!(store.compare_and_swap(INDEX_HEAD_KEY, None, b"idx").await.unwrap());
    assert!(store.compare_and_swap(STATIC_HEAD_KEY, None, b"stat").await.unwrap());
    assert_eq!(store.read(STATIC_HEAD_KEY).await.unwrap().unwrap().bytes, b"stat");
    assert_eq!(store.read(INDEX_HEAD_KEY).await.unwrap().unwrap().bytes, b"idx");
    // 陈旧 expectation（None 但已存在）→ lost CAS
    assert!(!store.compare_and_swap(STATIC_HEAD_KEY, None, b"stat2").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_static_head_cas_lets_exactly_one_win() {
    let (store, _dir) = store();
    assert!(store.compare_and_swap(STATIC_HEAD_KEY, None, b"seed").await.unwrap());
    let etag = store.read(STATIC_HEAD_KEY).await.unwrap().unwrap().etag;
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let store = store.clone();
        let etag = etag.clone();
        handles.push(tokio::spawn(async move {
            store.compare_and_swap(STATIC_HEAD_KEY, Some(&etag),
                format!("v{i}").as_bytes()).await.unwrap()
        }));
    }
    let winners: usize = futures_join_count(handles).await; // 逐个 await 计 true 数
    assert_eq!(winners, 1);
}
```

- [ ] **Step 2:** 失败 → **Step 3:** 实现（抽 `head_row_read(conn, table)` / `head_row_cas(conn, table, expected, new)`；key match 路由）→ **Step 4:** PASS + 全量绿 → **Step 5: Commit** `feat(local): SQLite static_release_head 表与静态指针 CAS 路由`

### Task 4: 激活编排 static_publisher（verify + install + CAS）

**Files:**
- Create: `src/indexing/static_publisher.rs`
- Modify: `src/indexing/mod.rs`（导出）
- Test: `tests/static_activation_test.rs`（新；CAS 用 `tests/publisher_test.rs:23` 的 `RecordingPublishStorage` fake 模式——**注意其冲突注入 API 是 `conflict_on_compare_and_swap(bytes: Vec<u8>)`**（预植一个"抢先写入的现值"），把该 fake 提取/复制到本测试文件）

**Interfaces:**
- Consumes: `ReleaseManifest`/`derive_release_id`/`sha256_hex`（`src/index/release_manifest.rs`）、`MmapIndex::load`、`PublishStorage`（含 `upload_directory`，`src/indexing/publisher.rs:65-89`）、Task 2/3 产物。
- Produces:

```rust
#[derive(Debug)]
pub enum StaticActivateError {
    Verify { message: String },
    LostCas { release_id: String },
    Storage(PublishError),
    Io { message: String },
}
pub struct StaticActivationResult {
    pub release_id: String,
    pub previous_release_id: Option<String>,
}

/// AC2 八步自洽验证；可选期望 profile。返回通过校验的 manifest。
pub fn verify_release_dir(dir: &Path, expect_model_id: Option<&str>, expect_dim: Option<u32>)
    -> Result<ReleaseManifest, StaticActivateError>;

/// 本地受管 store 安装：先试 fs::rename，失败回退递归 copy 到 .<id>-staging 再 rename；已存在则幂等跳过。
pub fn install_into_managed_store(root: &Path, release_id: &str, src_dir: &Path)
    -> Result<(), StaticActivateError>;

/// 读现指针 etag → StaticReleaseHead → CAS static/_head；Ok(false) → LostCas。
pub async fn activate_static_pointer<S: PublishStorage>(storage: &S, release_id: &str, updated_at: i64)
    -> Result<StaticActivationResult, StaticActivateError>;
```

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn verify_rejects_tampered_output_hash() {
    let dir = build_v3_release_fixture(); // 调 StaticReleaseBuilder 产真 release
    corrupt_one_byte(&dir.join("turbo_static_text.bin"));
    assert!(matches!(verify_release_dir(&dir, None, None).unwrap_err(),
        StaticActivateError::Verify { .. }));
}

#[test]
fn verify_rejects_unexpected_model_id() {
    let dir = build_v3_release_fixture(); // model_id = fixture 值
    assert!(matches!(verify_release_dir(&dir, Some("wrong-model"), None).unwrap_err(),
        StaticActivateError::Verify { .. }));
}

#[test]
fn verify_accepts_valid_release() {
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, Some(512)).unwrap();
    assert_eq!(manifest.turbo_version, 3);
    assert_eq!(manifest.release_id.len(), 64);
}

#[tokio::test]
async fn activate_writes_pointer_when_none_present() {
    let storage = RecordingPublishStorage::default();
    let res = activate_static_pointer(&storage, &"a".repeat(64), 1_700_000_000_000).await.unwrap();
    assert_eq!(res.previous_release_id, None);
    let obj = storage.read(STATIC_HEAD_KEY).await.unwrap().unwrap();
    let head = StaticReleaseHead::from_json(&obj.bytes).unwrap();
    assert_eq!(head.release_id, "a".repeat(64));
}

#[tokio::test]
async fn activate_reports_lost_cas_on_conflict() {
    let storage = RecordingPublishStorage::default();
    // 预植抢先写入的现值 → 我方 expected(None) 过期 → lost CAS
    storage.conflict_on_compare_and_swap(
        StaticReleaseHead::new("f".repeat(64), 1_700_000_000_000).to_json_pretty().into_bytes());
    let err = activate_static_pointer(&storage, &"b".repeat(64), 1_700_000_000_001).await.unwrap_err();
    assert!(matches!(err, StaticActivateError::LostCas { .. }));
}

#[test]
fn install_into_managed_store_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let src = build_v3_release_fixture();
    let rid = "c".repeat(64);
    install_into_managed_store(root.path(), &rid, &src).unwrap();
    install_into_managed_store(root.path(), &rid, &src).unwrap(); // 二次不报错
    assert!(root.path()
        .join(format!("static/releases/{rid}/release_manifest.json")).exists());
}
```

- [ ] **Step 2:** 失败 → **Step 3:** 实现（verify 按「设计要点 2」八步；install/CAS 按设计要点）→ **Step 4:** PASS + 全量绿 → **Step 5: Commit** `feat(indexing): 静态 release 激活编排——verify+install+CAS`

### Task 5: 本地 CLI `static-activate` 接线

**Files:**
- Modify: `src/app.rs`（`run_static_activate` + `parse_static_activate_args`）、`src/bin/ltsearch.rs`（分派 + USAGE 行）
- Test: `src/app.rs` 内联（arg 解析：缺 `--release`/`--root`、未知旗标、`--expect-dim` 非数字均报错）+ `tests/static_activate_cli_test.rs`（新，端到端）

**Interfaces:**
- Consumes: Task 4 三函数、`LocalPublishStorage`、`SqliteDb`。
- Produces: `pub async fn run_static_activate<I: IntoIterator<Item = S>, S: AsRef<str>>(args: I) -> Result<String, AppError>`（摘要含 release_id + previous）。

- [ ] **Step 1: 失败测试**（`tests/static_activate_cli_test.rs`；fixture 复用 #110 的 Lance fixture helper + `run_static_build`）

```rust
#[tokio::test]
async fn run_static_activate_installs_and_flips_pointer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let release = tmp.path().join("release");
    build_v3_release_via_cli(&release).await; // Lance fixture → run_static_build
    let summary = ltsearch::app::run_static_activate([
        "--release", release.to_str().unwrap(),
        "--root", root.to_str().unwrap(),
    ]).await.unwrap();
    assert!(summary.contains("activated"));
    // managed store 落位 + SQLite 指针行存在
    assert!(root.join("static/releases").read_dir().unwrap().next().is_some());
    let db = SqliteDb::open(&root.join("ltsearch.db")).unwrap();
    let head_bytes: Vec<u8> = read_static_head_row(&db); // SELECT head_bytes FROM static_release_head
    assert!(StaticReleaseHead::from_json(&head_bytes).is_ok());
}
```

  （PR-1 直接断言表行 + 目录；PR-2 Task 8 后收紧为经 `SqliteStaticReleaseStore` 读。）
- [ ] **Step 2:** 失败 → **Step 3:** 实现（`SqliteDb::open(root/ltsearch.db)` → `verify_release_dir` → `install_into_managed_store` → `activate_static_pointer(LocalPublishStorage, …, now_ms)`；USAGE 增 `static-activate` 行）→ **Step 4:** PASS + `cargo build --no-default-features --features local` → **Step 5: Commit** `feat(local): static-activate 命令——验证并 CAS 切换静态指针`

### Task 6: AWS 激活 bin `static_activate`

**Files:**
- Create: `src/bin/static_activate.rs`（`required-features=["aws"]`）
- Modify: `Cargo.toml`（`[[bin]]`）
- Test: bin 内联 arg 解析 + AWS 集成测试——**放测试基建所在处**：`tests/write_build_publish_test.rs` 的 `MockS3Server`（`:747`，已支持 If-Match/If-None-Match 条件写，见 `:231/:241`）是该文件私有的；将其提取到共享测试模块（如 `tests/support/mock_s3.rs` 以 `#[path]` 引用）或把新测试追加进该文件，执行时按最小改动取舍。

**Interfaces:**
- Consumes: Task 4 编排、`AwsPublishStorage`（`upload_directory(CreateOnly)` + `compare_and_swap`）、`s3_client_from_env`（`src/bootstrap.rs`）。

- [ ] **Step 1: 失败测试**（`#[cfg(feature = "aws")]`）

```rust
#[tokio::test]
async fn aws_static_activate_uploads_release_and_flips_pointer() {
    let server = mock_s3_with_bucket("artifacts").await;
    let storage = AwsPublishStorage::new("artifacts", server.client());
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, None).unwrap();
    storage.upload_directory(&static_release_dir_key(&manifest.release_id), &dir,
        UploadMode::CreateOnly).await.unwrap();
    let res = activate_static_pointer(&storage, &manifest.release_id, 1_700_000_000_000)
        .await.unwrap();
    assert_eq!(res.previous_release_id, None);
    assert!(storage.read(STATIC_HEAD_KEY).await.unwrap().is_some());
    assert!(storage.read(&static_release_manifest_key(&manifest.release_id))
        .await.unwrap().is_some());
}
```

- [ ] **Step 2:** 失败 → **Step 3:** 实现 bin（解析 `--release`/`--expect-*`；`LTSEARCH_STATIC_S3_BUCKET` 必填；`s3_client_from_env` → `AwsPublishStorage` → verify → upload_directory → activate → 摘要）→ **Step 4:** `cargo build --no-default-features --features aws` + 对应测试绿 → **Step 5: Commit** `feat(aws): static_activate bin——上传 v3 release 至 S3 并 CAS 切换指针`

### Task 7: PR-1 验收 + 开 PR

- [ ] 核验（见「验证」节 1-3 条）；**关键**：`static_publisher.rs` 只依赖 `PublishStorage` trait，不得 `use crate::adapters::*`（否则 AWS 泄入 local 图）。
- [ ] `gh pr create` — 标题 `feat(index): 静态 release 激活命令与指针 CAS（本地+AWS）(#112 PR-1)`，正文引用 #112（不 Close）+ 覆盖的 AC 分量（AC1 分离、AC2 verify、AC3 lost-CAS 等价）+ 验证证据；不自动合并；review 按 superpowers:receiving-code-review 处理。

---

## PR-2：查询双版本 + v3 端到端 + 目录约定移除 + AWS 拉取 + e2e

> 前置 PR-1 合并。开工：清理分支 → ff main → `git worktree add ../LTSearch-issue-112-pr2 -b feat/112-dual-version-query`。

### Task 8: 读侧 StaticReleaseStore（SQLite + 文件 + 分发器）

**Files:**
- Create: `src/storage/static_release_store.rs`（trait + `LocalStaticReleaseStore`）、`src/local/sqlite/static_release.rs`（`SqliteStaticReleaseStore`）
- Modify: `src/storage/mod.rs`、`src/local/sqlite/mod.rs`（导出）、`src/query_lambda.rs`（`static_release_store_for` 分发器）
- Test: 各内联 `#[cfg(test)]`

**Interfaces — Produces:** 「设计要点 3」的 trait 与两实现；`static_release_store_for(artifact_root)`（`<root>/ltsearch.db` 存在 → SQLite，否则文件）。

- [ ] **Step 1: 失败测试**

```rust
#[tokio::test]
async fn sqlite_static_store_reads_active_release_or_none() {
    let (db, dir) = SqliteDb::open_temp();
    let store = SqliteStaticReleaseStore::new(db.clone());
    assert!(store.load_active_release().unwrap().is_none());
    let publish = LocalPublishStorage::new(db, dir.path());
    let head = StaticReleaseHead::new("a".repeat(64), 1_700_000_000_000);
    publish.compare_and_swap(STATIC_HEAD_KEY, None,
        head.to_json_pretty().as_bytes()).await.unwrap();
    assert_eq!(store.load_active_release().unwrap().unwrap().release_id, "a".repeat(64));
}

#[test]
fn local_file_static_store_reads_pointer_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStaticReleaseStore::new(dir.path());
    assert!(store.load_active_release().unwrap().is_none());
    let head = StaticReleaseHead::new("b".repeat(64), 1_700_000_000_000);
    let p = dir.path().join("static/_head");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, head.to_json_pretty()).unwrap();
    assert_eq!(store.load_active_release().unwrap().unwrap().release_id, "b".repeat(64));
}
```

- [ ] **Step 2:** 失败 → **Step 3:** 实现（NotFound/空行 → `Ok(None)`）→ **Step 4:** PASS → **Step 5: Commit** `feat(storage): StaticReleaseStore 指针读侧（SQLite+文件）`

### Task 9: TurboQuantSearcher v3 暴露（原始 doc_id / metadata / citation / filter）

**Files:**
- Modify: `src/query/turbo_searcher.rs`（materialization 按 `version()` 分支）
- Test: `tests/turbo_searcher_v3_test.rs`（新；用 `StaticReleaseBuilder` 建含 citation 字段 metadata 的真 v3 release，`Box::leak` 加载）

**Interfaces:**
- Consumes: `MmapIndex::version()`/`original_doc_id`/`metadata_json`、`Citation::from_metadata`（`src/models/search.rs:132`）。

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn v3_searcher_exposes_original_doc_id_metadata_and_citation() {
    let index = leak_v3_index(&[("doc-α",
        r#"{"resource_id":"r1","source_type":"law","source_ref":"ref1",
           "title":"民法典","url":"https://x","lang":"zh"}"#)]);
    let searcher = TurboQuantSearcher::new(index);
    let results = searcher.search(&active_manifest(), &query_vec(), 5).unwrap();
    let top = &results[0];
    assert_eq!(top.doc_id, "doc-α"); // 原始字符串，非 u64 哈希
    assert_eq!(top.metadata.as_ref().unwrap().get("lang").unwrap(), "zh");
    let c = top.citation.as_ref().unwrap();
    assert_eq!(c.resource_id, "r1");
    assert_eq!(c.url.as_deref(), Some("https://x"));
    assert_eq!(c.title.as_deref(), Some("民法典"));
}

#[test]
fn v3_searcher_results_survive_metadata_filter() {
    let index = leak_v3_index(&[("doc-α", r#"{"lang":"zh"}"#),
                                ("doc-β", r#"{"lang":"en"}"#)]);
    let searcher = TurboQuantSearcher::new(index);
    let raw = searcher.search(&active_manifest(), &query_vec(), 5).unwrap();
    let filtered = apply_filters(raw, Some(&filters_lang_eq("zh")));
    assert!(filtered.iter().any(|r| r.doc_id == "doc-α"));
    assert!(!filtered.iter().any(|r| r.doc_id == "doc-β"));
}
```

  另保留一条 v2 回归护栏：v2 index 仍 `metadata: None`、title-only citation。
- [ ] **Step 2:** 失败 → **Step 3:** 实现（v3 分支取 `original_doc_id`/解析 `metadata_json`/`Citation::from_metadata`；解析失败回退 `None` + warn）→ **Step 4:** PASS + 全量绿 → **Step 5: Commit** `feat(query): TurboQuantSearcher 暴露 v3 原始 doc_id/metadata/citation 并支持 filter`

### Task 10: 响应 & 健康 schema 增 static_release_id

**Files:**
- Modify: `src/models/search.rs`（`SearchResponse` +字段）、`src/query/router.rs`（`with_static_release_id` + `search()` 写入）、`src/http/mod.rs`（`HealthBody` +字段）、`src/http/query.rs`（`/health` 上报）
- Test: `src/query/router.rs` 内联 + `tests/http_query_test.rs` 追加

- [ ] **Step 1: 失败测试** — router 内联：`with_static_release_id(Some("abc"))` → 响应 `static_release_id == Some("abc")`；未设置 → `None`。http 测试：seed 指针文件后 `/health` body 含 `static_release_id`；无指针为 null。
- [ ] **Step 2:** 失败 → **Step 3:** 实现（serde `default` + `skip_serializing_if`）→ **Step 4:** PASS → **Step 5: Commit** `feat(query): 响应与健康检查上报 static_release_id`

### Task 11: 查询缓存键扩为 (dynamic_version, static_release_id) + 指针解析加载

**Files:**
- Modify: `src/query_service.rs`（`CachedQueryHandler` + resolve 函数签名）、`src/query_lambda.rs`（load 返回 pair；bootstrap 冻结静态 release；`try_load_static_searcher` 改指针解析，**删除** `LTSEARCH_QUERY_STATIC_DIR` 与 `<root>/static` 目录逻辑）
- Test: `tests/query_service_test.rs` 追加

**Interfaces — Produces:**

```rust
struct CachedQueryHandler {
    version_id: u64,
    static_release_id: Option<String>,
    handler: SharedQueryRequestHandler,
}
// load:      Fn() -> Result<(u64, Option<String>), QueryLambdaError>
// bootstrap: Fn((u64, Option<String>)) -> Result<SharedQueryRequestHandler, QueryLambdaError>
fn try_load_static_searcher(artifact_root: &Path, release_id: Option<&str>)
    -> Result<Option<TurboQuantSearcher>, QueryLambdaError>;
// None → NoopStaticRetriever；Some(id) → MmapIndex::load(<root>/static/releases/<id>/) → Box::leak
```

- [ ] **Step 1: 失败测试**（`tests/query_service_test.rs`，对称现有 `active_versions=[7,7,8]` 模式）

```rust
#[test]
fn cache_rebuilds_when_static_release_changes_even_if_dynamic_version_stable() {
    // dynamic 恒 7；static: [Some("r1"), Some("r1"), Some("r2")]
    // 期望 bootstrap 恰被调 2 次：(7,r1) 与 (7,r2)
}

#[test]
fn cache_rebuilds_when_dynamic_version_changes_static_stable() {
    // dynamic [7,7,8]；static 恒 Some("r1") → bootstrap 2 次
}

#[test]
fn cache_hits_when_pair_unchanged() {
    // (7, None) 三连 → bootstrap 1 次
}
```

- [ ] **Step 2:** 失败 → **Step 3:** 实现（resolve 比对 pair；load 组合 `manifest_store.load_active_version()` + `static_release_store_for(...).load_active_release()`；bootstrap 冻结 dynamic manifest 后按传入 release_id 装 static searcher 并 `with_static_release_id`；retriable-on-change 覆盖静态指针变更）→ **Step 4:** PASS + 全量绿 → **Step 5: Commit** `feat(query): 查询按 (dynamic_version, static_release_id) 对解析与缓存，单请求不混 release`

### Task 12: AWS 查询侧拉取（指针文件 + 按 release_id 惰性拉 release）

**Files:**
- Modify: `src/adapters/s3_artifact_sync.rs`（去 `static/` 批量前缀；加指针下载 + release-dir 惰性拉取）
- Test: 内联前缀守卫更新（批量前缀**不含** `static/`）+ AWS 集成测试（Task 6 所在测试基建：activate 后 sync，断言 `<root>/static/_head` 与 `<root>/static/releases/<id>/*` 落盘；二次 sync 不重下）

- [ ] **Step 1: 失败测试**

```rust
#[tokio::test]
async fn s3_sync_pulls_pointer_and_active_release_once() {
    let server = mock_s3_with_bucket("artifacts").await;
    let storage = AwsPublishStorage::new("artifacts", server.client());
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, None).unwrap();
    storage.upload_directory(&static_release_dir_key(&manifest.release_id), &dir,
        UploadMode::CreateOnly).await.unwrap();
    activate_static_pointer(&storage, &manifest.release_id, 1_700_000_000_000).await.unwrap();

    let root = tempfile::tempdir().unwrap();
    let sync = s3_artifact_sync_against(&server, "artifacts");
    sync.sync(root.path()).await.unwrap();
    assert!(root.path().join("static/_head").exists());
    assert!(root.path().join(format!("static/releases/{}/release_manifest.json",
        manifest.release_id)).exists());
    sync.sync(root.path()).await.unwrap(); // 命中：release 已在盘，不重下（mock 计数断言）
}
```

- [ ] **Step 2:** 失败 → **Step 3:** 实现（「设计要点 5」三步）→ **Step 4:** aws 测试绿 → **Step 5: Commit** `feat(aws): 查询侧按指针 release_id 惰性拉取 v3 release`

### Task 13: 移除目录约定（publisher / mmap_index / Dockerfile / 测试残留）

**Files:**
- Modify: `src/indexing/publisher.rs`（删 `static_artifact_source` + `upload_directory("static", …, Overwrite)` 块，`:174-178` 与 `:412-428` 附近）、`src/index/mmap_index.rs`（删 `IMAGE_STATIC_DIR`/`load_from_image`/`global_from_image`/关联 static；删前 `grep -rn` 全仓确认无其他调用方）、`Dockerfile`（删 `:18` `COPY static/ /app/static/` 与 `:20` `ENV LTSEARCH_QUERY_STATIC_DIR=/app`）、`tests/http_query_test.rs`（删 `:75`/`:167` 两处 `remove_var("LTSEARCH_QUERY_STATIC_DIR")`）、`tests/publisher_test.rs`（改写任何"dynamic publish 上传 static"断言为"不产生 static key"）
- Create: `tests/test_image_no_static_convention.py`（pytest 结构守卫）

- [ ] **Step 1: 守卫测试**

```python
import pathlib

def test_dockerfile_has_no_static_convention():
    text = pathlib.Path("Dockerfile").read_text()
    assert "/app/static" not in text
    assert "LTSEARCH_QUERY_STATIC_DIR" not in text

def test_source_has_no_image_static_dir():
    assert "IMAGE_STATIC_DIR" not in pathlib.Path("src/index/mmap_index.rs").read_text()
```

- [ ] **Step 2:** 守卫红 → **Step 3:** 执行删除 → **Step 4:** 守卫绿 + `cargo test` 全量绿 + `grep -rn 'LTSEARCH_QUERY_STATIC_DIR\|IMAGE_STATIC_DIR\|/app/static' src/ tests/ scripts/ Dockerfile` 无残留 → **Step 5: Commit** `refactor: 移除隐式 static 目录约定，静态服务只经指针解析`

### Task 14: e2e——build→activate→query 的 v3 端到端

**Files:**
- Create: `scripts/e2e/run-static-release-flow.sh`（local profile：`static-build` → `static-activate` → 起 query 服务 → `POST /query` 带 filter → 断言原始 doc_id + citation + `static_release_id`；再激活第二个 release → `/health` 翻转断言）
- Create: `tests/test_static_release_e2e.py`（pytest 结构守卫：脚本存在、含 build→activate→query 顺序、含 `static_release_id`/`citation` 断言）
- Modify: `.github/workflows/ci.yml`（e2e 脚本纳入现有 local-e2e 类 job，按现有模式）

- [ ] **Step 1: 守卫红** → **Step 2: 写脚本**（fixture 用 #110 冒烟方式造小 Lance 数据集；`LTSEARCH_LOCAL_ROOT` = query 的 artifact_root；`curl POST /query '{"query":…,"top_k":3,"filters":{…},"include_metadata":true}'` 断言 `static_chunks[0].doc_id` 为原始串、`.citation` 非空、响应 `static_release_id` == 激活 id）→ **Step 3:** `bash scripts/e2e/run-static-release-flow.sh` 绿 + 守卫绿 → **Step 4: Commit** `test(e2e): static-build→activate→query v3 端到端流`

### Task 15: PR-2 验收 + 开 PR + 收尾

- [ ] 全量核验（「验证」节四条）+ 手动冒烟。
- [ ] `gh pr create` — 标题 `feat(query): 双版本查询与 v3 静态检索端到端 (#112 PR-2)`，正文 `Closes #112` + AC 映射表 + 验证证据；不自动合并。
- [ ] 合并后：清 worktree/分支、ff main、确认 #112 关闭、勾选 epic #132/#106 对应项、更新记忆。

---

## 验证（端到端）

每个 PR 收尾都跑：

```bash
# 1. local profile 全量 + AWS-free 核验（feature-matrix 本地复现）
cargo build --no-default-features --features local
for pkg in aws-config aws-sdk-s3 aws-sdk-sqs lambda_runtime; do
  cargo tree --no-default-features --features local -i "$pkg" >/dev/null 2>&1 && echo "LEAK $pkg"
done   # 期望无输出
cargo test --no-default-features --features local

# 2. aws / lambda profile（孪生零回归；static_activate bin 纳入 aws 构建面）
cargo build --no-default-features --features aws
cargo test  --no-default-features --features aws
cargo build --no-default-features --features lambda
cargo build --no-default-features --features aws --bin static_activate   # PR-1 起

# 3. lint / fmt / python 守卫
cargo clippy --all-targets -- -D warnings && cargo fmt --check
python3 -m pytest tests/test_*.py -q

# 4. e2e（PR-2）
bash scripts/e2e/run-static-release-flow.sh
```

手动冒烟（PR-2）：`static-build` 产 release → `static-activate` → `ltsearch query` 起服务 → `curl POST /query`（带 filter）确认 `static_chunks` 有原始 doc_id + citation、`static_release_id` 与激活 id 一致 → 激活另一 release → `/health` 的 `static_release_id` 翻转、下一请求命中新 release。

CI 期望：`fast` / `feature-matrix` / `integration` / local-e2e 类 job 全绿。

## AC 映射表

| AC | 需求 | 覆盖任务 |
|---|---|---|
| 1 | build 与 activate 分离命令，built release 绝不隐式激活 | PR-1 T4/T5/T6（独立命令；`static-build` 不碰指针）+ PR-2 T13（拔掉隐式目录入口） |
| 2 | 激活前验证 manifest/输出 hash/Lance provenance/embedding profile，再 CAS | PR-1 T4 `verify_release_dir` 八步 + T1 硬化保证结构校验不 panic |
| 3 | SQLite 与 AWS 指针等价 lost-CAS | PR-1 T2/T3（新表 + IMMEDIATE CAS）+ T4/T6（`Ok(false)`→`LostCas`；S3 412/409 复用）；并发/跨连接测试证等价 |
| 4 | 查询缓存并上报 `(dynamic_version, static_release_id)` 对，单请求不混 | PR-2 T8/T10/T11 |
| 5 | v3 元数据/filter/原始 ID/citation 端到端 | PR-2 T9 + T10 + T13 + T14（e2e 证端到端） |

## 风险 / 执行时需就地确认的点

1. **`RecordingPublishStorage`**（`tests/publisher_test.rs:23`）是该文件私有 struct，冲突注入为 `conflict_on_compare_and_swap(bytes: Vec<u8>)`（预植抢先写入的现值）——T4 需把 fake 提取或复制到新测试文件，按其真实 API 适配。
2. **`MockS3Server`**（`tests/write_build_publish_test.rs:747`）已验证支持 If-Match/If-None-Match 条件写（`:231`/`:241`），但同为文件私有——T6/T12 的 AWS 测试要么提取成共享测试模块，要么追加进该文件；执行时按最小改动取舍。
3. **跨盘 rename 回退**不依赖 `ErrorKind::CrossesDevices`（稳定性存疑）：任何 rename 失败即走"copy 到 `.<id>-staging` 再 rename"路径。
4. **#111（PR #137）在途**：都碰 lambda 冷启拉取语境但文件面不同；后合者 rebase。
5. **`SqliteManifestStore` 与 `SqliteStaticReleaseStore` 同库双连接**：读路径单行只读，WAL + busy_timeout 已覆盖，风险低；query 进程 wiring 时优先复用同一 `SqliteDb` 句柄。
6. **多语料/多 profile** 超出 #112 范围（`ReleaseSource.corpus_type` 单值，#110 已裁决单数据集）；`--expect-*` 仅单 profile 钉住。
7. SAM/运维 runbook（AWS 激活 bin 的 IAM/操作文档）属 #113 文档校准范围，本计划不含。
