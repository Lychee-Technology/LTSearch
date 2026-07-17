# Issue #110 — 从 pinned static Lance release 构建 TurboQuant v3 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> 执行时将本文件保存为 `docs/superpowers/plans/2026-07-17-issue-110-static-lance-turboquant.md`（epic 惯例）。

## Context

Epic #106 / 路线图 #132 的 Wave A 任务。现状:本地 `static-build` 命令读本地 JSONL 并**全量重新嵌入**,产出 TQNT **v2** 静态索引——v2 把 doc_id 经 FNV-1a 哈希成 u64,**丢失原始 doc_id 字符串、完整 metadata 与 citation**;静态侧没有任何 manifest/provenance/输出 hash。#110 要求:从 pinned 的独立 static Lance 数据集做确定性全表扫描,**复用已存的 512 维 embeddings(不重嵌)**,产出自描述的不可变 TurboQuant **v3** release(v3 补齐原始 doc_id/metadata/citation 保留 + release manifest)。后续 #112(不在本范围)做激活与查询侧暴露,因此本次 `MmapIndex` 必须能加载 v3 并提供新访问器。

**已裁决(用户确认)**:① 本地 `static-build` 彻底替换为 Lance 模式(本地 JSONL 路径删除;AWS 孪生 `turbo_index_builder` 的 S3 JSONL 路径不动);② config 单数据集(单 `dataset_path` + `table_version` + `corpus_type` + `embedding_profile`)。

**Goal:** 本地 `static-build` 从 pinned Lance table version 确定性构建 TurboQuant v3 static release(9 个 bin + `release_manifest.json`),build-twice 逐字节相同。

**Architecture:** v3 = v2 的 6 文件不动 + 3 个附加 sidecar(meta_ext / docid blob / metadata JSON blob)+ manifest;新增独立 `StaticReleaseBuilder`(类型上不接受缺失 embedding,杜绝重嵌)与 `lance_source` 模块(lancedb `checkout(version)` pin + 全表扫描 + 按 doc_id 排序保证确定性);`release_id` 内容导出(无时间戳/UUID)。

**Tech Stack:** Rust、lancedb 0.31(`table.checkout(version)` / plain query 全表扫描,local profile 下 AWS-free)、arrow_array、sha2+hex(已在 Cargo.lock 传递依赖中,提升为直接依赖不增加编译面)、memmap2。

## Global Constraints

- 单 PR,`Closes #110`;worktree `../LTSearch-issue-110 -b feat/110-static-lance-turboquant`;不自动合并。
- local profile AWS-free:`cargo tree` 不得出现 aws-config/aws-sdk-s3/aws-sdk-sqs/lambda_runtime(CI feature-matrix job 强制,`.github/workflows/ci.yml:32-55`)。
- 不动 AWS 孪生路径:`src/bin/turbo_index_builder.rs`、`load_static_chunks_from_s3`、`StaticSourceConfig`/`TurboBuildConfig`/`parse_static_source_lines` 保留(仍被 aws 孪生使用)。
- v2 读路径不回归:`MmapIndex` 必须继续加载 v2(#112 前 `query_lambda::try_load_static_searcher` 仍加载旧目录);现有 `StaticIndexBuilder`(v2 writer)与其测试不动。
- 决定性铁律:manifest 无时间戳/无 UUID/无 HashMap 序列化;metadata JSON 一律经 `BTreeMap` 规范化(Lance 里存的 JSON 字符串字节序不可信——写入侧从 `HashMap` 序列化);`release_id` 排除 `dataset_path`。
- 每任务 TDD:先写失败测试 → 实现 → 绿 → commit(commit message 遵循仓库 conventional-commit 风格)。

## 设计要点(已定,执行时勿改)

**v3 产物目录**(10 个文件):

| 文件 | 内容 |
|---|---|
| `centroids.bin` / `projection.bin` / `turbo_static.bin` / `turbo_static_meta.bin` / `turbo_static_text.bin` / `turbo_static_title.bin` | 与 v2 完全同构(仅 header version=3) |
| `turbo_static_meta_ext.bin` | `MetaExtRecord` 定长数组(与 meta 平行,24 B/条) |
| `turbo_static_docid.bin` | 原始 doc_id UTF-8 连接 blob |
| `turbo_static_meta_json.bin` | 规范化 metadata JSON 连接 blob(citation 字段含其中) |
| `release_manifest.json` | 见 Task 4;不 mmap |

**release_id** = sha256(turbo_version ∥ embedding_profile ∥ codec ∥ input_fingerprint.content_digest ∥ sorted outputs(name,sha256,size)),hex 编码。内容寻址:同内容→同 id,满足 build-twice。

**input_fingerprint.content_digest** = sha256 over 按 doc_id 排序后的每行:`doc_id bytes ∥ embedding f32 LE bytes ∥ text bytes ∥ canonical metadata JSON bytes`(与磁盘布局无关)。

**config JSON**(`static-build --config` 新契约):

```json
{
  "dataset_path": "/data/artifacts/lance/v3/shard_0",
  "table_version": 2,
  "corpus_type": "legal",
  "embedding_profile": { "model_id": "jina-v5-nano/512", "dim": 512 }
}
```

(`corpus_type` 用 `CorpusType` 现有 snake_case serde,`src/models/search.rs:27-34`;Lance 表无 corpus_type 列,由 config 提供——已确认。)

**校验即 fail 清单**(AC 2):

- 源侧(Task 6):doc_id 空、text 空、metadata 非法 JSON/非 object、embedding 为 null、维度 ≠ profile.dim(=512)、含非有限值、profile.dim ≠ 512、embedding 列非 `FixedSizeList<Float32, 512>`。
- 构建侧(Task 5):重复 doc_id(HashSet)、FNV-1a hash 冲突(HashMap<u64, doc_id>;测试注入合成冲突,不可暴力求真实碰撞)、chunks/embeddings 数量不等、空输入。

---

### Task 0: 开工前置

- [ ] 清理已合并 PR 的本地分支;`git -C /Users/ruoshi/code/Lychee/LTBase/LTSearch pull --ff-only`
- [ ] `git worktree add ../LTSearch-issue-110 -b feat/110-static-lance-turboquant`
- [ ] 本计划存入 `docs/superpowers/plans/2026-07-17-issue-110-static-lance-turboquant.md`,首个 commit

### Task 1: v3 header(存储 version 字段,接受 {2,3})

**Files:**
- Modify: `src/index/header.rs`(全文件小,当前 157 行)
- Test: `tests/turbo_header_test.rs`(追加)

**Interfaces:**
- Produces: `TurboHeader::new_v3(dim, record_count)`、`TurboHeader::version()`(改为返回存储值)、`KnownRecordLayout::V3Dim512`、`pub const TURBO_VERSION_V2: u32 = 2; pub const TURBO_VERSION_V3: u32 = 3;`
- 兼容:`TurboHeader::new(dim, count)` 语义不变(构造 v2),现有调用点(`static_builder.rs:115` 等)零改动。

- [ ] **Step 1: 失败测试**(追加到 `tests/turbo_header_test.rs`)

```rust
#[test]
fn header_roundtrips_v3_version() {
    let header = TurboHeader::new_v3(512, 3);
    assert_eq!(header.version(), 3);
    let parsed = TurboHeader::from_bytes(&header.to_bytes()).unwrap();
    assert_eq!(parsed.version(), 3);
    assert_eq!(parsed.dim(), 512);
    assert_eq!(parsed.record_count(), 3);
}

#[test]
fn header_rejects_unknown_version() {
    let mut bytes = TurboHeader::new_v3(512, 1).to_bytes();
    bytes[4..8].copy_from_slice(&4u32.to_le_bytes());
    assert!(matches!(
        TurboHeader::from_bytes(&bytes),
        Err(TurboHeaderError::UnsupportedVersion { version: 4 })
    ));
}

#[test]
fn layout_v3_dim512_record_size_matches_v2() {
    let header = TurboHeader::new_v3(512, 1);
    let layout = KnownRecordLayout::from_header(&header).unwrap();
    assert_eq!(layout.record_size(), std::mem::size_of::<TurboRecord512>());
}
```

- [ ] **Step 2:** `cargo test --test turbo_header_test` → 编译失败(`new_v3` 不存在)
- [ ] **Step 3: 实现** — `TurboHeader` 增加 `version: u32` 字段;`new` 委托 `Self { version: TURBO_VERSION_V2, .. }`,新增 `new_v3`;`version()`/`to_bytes()` 用存储值;`from_bytes` 接受 `TURBO_VERSION_V2 | TURBO_VERSION_V3` 否则 `UnsupportedVersion`;`KnownRecordLayout::from_header` 增加 `(3, 512) => V3Dim512`,`record_size` 同 V2。
- [ ] **Step 4:** `cargo test --test turbo_header_test` → PASS;`cargo test` 全绿(确认 v2 路径零回归)
- [ ] **Step 5: Commit** `feat(index): TurboHeader 支持 v3 版本与 V3Dim512 布局`

### Task 2: MetaExtRecord sidecar 记录类型

**Files:**
- Create: `src/index/meta_ext.rs`
- Modify: `src/index/mod.rs`(导出)

**Interfaces:**
- Produces:

```rust
pub const META_EXT_RECORD_SIZE: usize = 24;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaExtRecord {
    pub docid_offset: u64,
    pub meta_json_offset: u64,
    pub docid_len: u32,
    pub meta_json_len: u32,
}

impl MetaExtRecord {
    pub fn doc_id_from_blob<'a>(&self, blob: &'a [u8]) -> &'a str;      // 风格同 MetaRecord::text_from_blob
    pub fn metadata_json_from_blob<'a>(&self, blob: &'a [u8]) -> &'a str;
}
```

- [ ] **Step 1: 失败测试**(`meta_ext.rs` 内联 `#[cfg(test)]`,风格同 `static_source.rs` 的 parse_tests)

```rust
#[test]
fn meta_ext_record_has_fixed_size() {
    assert_eq!(std::mem::size_of::<MetaExtRecord>(), META_EXT_RECORD_SIZE);
}

#[test]
fn meta_ext_reads_docid_and_json_from_blob() {
    let docid_blob = b"doc-1doc-2";
    let json_blob = br#"{"a":1}{"b":2}"#;
    let record = MetaExtRecord { docid_offset: 5, docid_len: 5, meta_json_offset: 7, meta_json_len: 7 };
    assert_eq!(record.doc_id_from_blob(docid_blob), "doc-2");
    assert_eq!(record.metadata_json_from_blob(json_blob), r#"{"b":2}"#);
}
```

- [ ] **Step 2:** 编译失败 → **Step 3:** 实现(字段序 u64 在前避免尾部 padding,同 `meta.rs:5-7` 注释先例)→ **Step 4:** PASS → **Step 5: Commit** `feat(index): 新增 v3 MetaExtRecord sidecar 记录`

### Task 3: MmapIndex v2/v3 双支持

**Files:**
- Modify: `src/index/mmap_index.rs`(`MmapIndex` 结构体 + `load` + 新访问器 + 新错误变体)
- Test: `tests/mmap_index_test.rs`(追加;手工构造 fixture,参考现有测试写法)

**Interfaces:**
- Consumes: Task 1 `version()`/`V3Dim512`、Task 2 `MetaExtRecord`
- Produces(#112 将消费):

```rust
impl MmapIndex {
    pub fn version(&self) -> u32;                                // header.version()
    pub fn original_doc_id(&self, i: usize) -> Option<&str>;     // v2 → None
    pub fn metadata_json(&self, i: usize) -> Option<&str>;       // v2 → None
}
```

- 结构体新增 `meta_ext_mmap: Option<Mmap>`、`docid_mmap: Option<Mmap>`、`meta_json_mmap: Option<Mmap>`(v2 时 None)。
- `load`:header 解析后按 `header.version()` 分支;v3 额外 mmap 3 个 sidecar 并校验 `meta_ext len % 24 == 0` 且条数 == record_count(新错误变体 `MetaExtCountMismatch { expected, actual }`,风格同 `MetaCountMismatch`)。

- [ ] **Step 1: 失败测试** — 3 个测试:
  - `mmap_index_loads_v3_and_exposes_original_doc_id_and_metadata_json`:手工写 v3 目录(v3 header + 1 条记录 + 3 sidecar),断言 `version()==3`、`original_doc_id(0)==Some("doc-α")`、`metadata_json(0)` 解析回原 map。
  - `mmap_index_still_loads_v2_without_ext_files`:现有 v2 fixture,断言 `version()==2`、两个新访问器返回 `None`。
  - `mmap_index_rejects_v3_with_mismatched_ext_count`:v3 header 声明 2 条但 meta_ext 只 1 条 → `MetaExtCountMismatch`。
- [ ] **Step 2:** 失败 → **Step 3:** 实现 → **Step 4:** `cargo test --test mmap_index_test` PASS + 全量 `cargo test` 绿(v2 回归确认)→ **Step 5: Commit** `feat(index): MmapIndex 双支持 v2/v3 并暴露原始 doc_id 与 metadata JSON`

### Task 4: ReleaseManifest + 内容导出 release_id

**Files:**
- Create: `src/index/release_manifest.rs`
- Modify: `Cargo.toml`(`[dependencies]` 增加 `sha2 = "0.10"`、`hex = "0.4"`——均已在 Cargo.lock,零新编译面)、`src/index/mod.rs`(导出)

**Interfaces:**
- Produces:

```rust
pub const RELEASE_MANIFEST_FILE: &str = "release_manifest.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub manifest_schema_version: u32,          // 1
    pub turbo_version: u32,                    // 3
    pub release_id: String,                    // sha256 hex
    pub source: ReleaseSource,
    pub embedding_profile: EmbeddingProfile,
    pub input_fingerprint: InputFingerprint,
    pub codec: CodecMetadata,
    pub outputs: Vec<OutputFile>,              // 按 name 排序
}
#[derive(...)] pub struct ReleaseSource { pub kind: String /*"lance"*/, pub dataset_path: String, pub table_version: u64, pub table_row_count: u64, pub corpus_type: CorpusType }
#[derive(...)] pub struct EmbeddingProfile { pub model_id: String, pub dim: u32 }
#[derive(...)] pub struct InputFingerprint { pub doc_count: u64, pub content_digest: String }
#[derive(...)] pub struct CodecMetadata { pub dim: u32, pub centroids_per_dim: u32, pub centroids_seed: u64, pub projection_seed: u64 }
#[derive(...)] pub struct OutputFile { pub name: String, pub sha256: String, pub size_bytes: u64 }

pub fn sha256_hex(bytes: &[u8]) -> String;
pub fn content_digest(rows: &[CanonicalRow]) -> String;   // CanonicalRow { doc_id, embedding, text, canonical_meta_json }
pub fn derive_release_id(m: &ReleaseManifestDraft) -> String;  // 排除 dataset_path;见设计要点
pub fn canonical_metadata_json(metadata: &HashMap<String, Value>) -> Vec<u8>;  // BTreeMap 重排后 to_vec
```

- [ ] **Step 1: 失败测试**(内联):
  - `manifest_serializes_deterministically`:同一 manifest `serde_json::to_vec` 两次字节相同;JSON 文本不含 `timestamp`/`uuid` 字样。
  - `release_id_is_content_derived_and_stable`:同输入两次 `derive_release_id` 相同;改 `dataset_path` **不**改变 release_id。
  - `release_id_changes_when_an_output_hash_changes`:改任一 output sha256 → release_id 变。
  - `canonical_metadata_json_is_key_order_independent`:两个插入顺序不同的 HashMap → 相同字节。
- [ ] **Step 2:** 失败 → **Step 3:** 实现 → **Step 4:** PASS → **Step 5: Commit** `feat(index): static release manifest 与内容导出 release_id`

### Task 5: StaticReleaseBuilder(v3 writer,结构上禁止重嵌)

**Files:**
- Create: `src/index/static_release.rs`
- Modify: `src/index/static_builder.rs`(仅把 `CENTROIDS_PER_DIM`/`CENTROIDS_SEED`/`PROJECTION_SEED`/`SUPPORTED_TYPED_DIM`/`stable_hash_doc_id`/`corpus_type_id` 改 `pub(crate)`,供复用;v2 行为零变化)、`src/index/mod.rs`(导出)
- Test: `tests/static_release_builder_test.rs`(新)

**Interfaces:**
- Consumes: Task 1-4 全部;`encode_vector`/`CentroidTable`/`ProjectionMatrix`(`src/index/assets.rs`)、`StagedDir`/`append_cleanup_failure`(`src/storage/staged_publish.rs`)、`StaticChunk`。
- Produces:

```rust
pub struct StaticReleaseBuilder;
impl StaticReleaseBuilder {
    /// embeddings 无 Option——缺失 embedding 在类型上不可表达,杜绝重嵌。无 generator 参数。
    pub fn build_release(
        &self,
        output_dir: &Path,
        chunks: &[StaticChunk],          // 调用方已按 doc_id 排序(Task 6 保证)
        embeddings: &[Vec<f32>],
        profile: &EmbeddingProfile,
        source: &ReleaseSource,
    ) -> Result<ReleaseManifest, IndexError>;
}
pub(crate) fn detect_duplicate_doc_ids(chunks: &[StaticChunk]) -> Result<(), IndexError>;
pub(crate) fn detect_hash_collisions(hashed: &[(String, u64)]) -> Result<(), IndexError>;
```

实现顺序:校验(数量相等/非空/512 维/有限/重复 doc_id/hash 冲突)→ 生成 codec 资产(同 v2 常量与 seed)→ 编码 9 文件字节(v3 header;meta_ext/docid/meta_json 与 v2 六件同循环生成;metadata 经 `canonical_metadata_json`)→ `StagedDir` 写 9 文件 → 对 staged 文件逐个 `sha256_hex` 组 `outputs` → `derive_release_id` → 写 `release_manifest.json` → `commit_replace_dir`。

- [ ] **Step 1: 失败测试**:
  - `release_builder_writes_v3_artifacts_loadable_by_mmap_index`:2 chunk(metadata 带 title/citation 字段与非 ASCII doc_id)→ build → `MmapIndex::load` 断言 version 3、text/title/corpus_type 同 v2 行为、`original_doc_id`/`metadata_json` 逐条还原、`Citation::from_metadata(解析回的 map)` 可重建 citation;`release_manifest.json` 存在且 outputs 的 sha256 与实际文件一致。
  - `release_builder_rejects_missing_512_dim`(511 维)/ `release_builder_rejects_non_finite_embedding`(含 NaN)。
  - `release_builder_rejects_duplicate_doc_id`。
  - `detect_hash_collisions_flags_two_docids_sharing_a_hash`(直接喂 `[("a",7),("b",7)]` 合成冲突——真实 FNV 碰撞不可暴力构造)。
- [ ] **Step 2:** 失败 → **Step 3:** 实现 → **Step 4:** PASS + 全量绿 → **Step 5: Commit** `feat(index): StaticReleaseBuilder 产出自描述 TurboQuant v3 release`

### Task 6: Lance 快照源(pin version + 确定性全表扫描)

**Files:**
- Create: `src/index/lance_source.rs`(无 feature 门控;lancedb 在 local 图中 AWS-free,已核实 Cargo.lock)
- Modify: `src/index/mod.rs`(导出)
- Test: `tests/lance_source_test.rs`(新;fixture 用 lancedb 写 `documents` 表,schema 抄 `src/indexing/builder.rs:193-206`,列:doc_id/text/metadata/timestamp/embedding FixedSizeList<Float32,512>)

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LanceStaticSourceConfig {
    pub dataset_path: String,
    pub table_version: u64,
    pub corpus_type: CorpusType,
    pub embedding_profile: EmbeddingProfile,   // Task 4 类型
}

pub struct LanceSnapshot {
    pub chunks: Vec<StaticChunk>,        // 已按 doc_id 升序排序
    pub embeddings: Vec<Vec<f32>>,       // 与 chunks 平行
    pub table_version: u64,              // 实际 checkout 到的版本(写 provenance)
    pub row_count: u64,
}

pub async fn load_lance_snapshot(cfg: &LanceStaticSourceConfig) -> Result<LanceSnapshot, IndexError>;
```

实现:`lancedb::connect(&cfg.dataset_path)` → `open_table("documents")` → `table.checkout(cfg.table_version)`(pin;不存在的 version 即错)→ plain `table.query().execute()`(无 `nearest_to`,全表)→ `try_collect` batches → 逐行解码:doc_id/text/metadata 复用 `lance_decode.rs` 的 StringArray downcast 思路,embedding 新写 `FixedSizeListArray` → `Float32Array` 提取 → 行校验(见"校验即 fail 清单·源侧";embedding 维度 != `profile.dim` 或 profile.dim != 512 即错)→ **collect 后按 doc_id 字符串升序排序**(确定性由本模块拥有,不信任 Lance fragment 顺序)→ 组装 `StaticChunk`(corpus_type 取 cfg)。

- [ ] **Step 1: 失败测试**(async 测试用 `#[tokio::test]`,fixture helper 建两个表版本):
  - `lance_source_reads_pinned_version_and_reuses_embeddings`:写 3 行(乱序 doc_id、真实 512 维向量)→ 记录 `table.version()` → 断言 chunks 按 doc_id 升序、embeddings 与写入的逐位相等(证明零重嵌)。
  - `lance_source_pins_version_and_ignores_later_writes`:记录 v1 → `table.add` 追加一行(版本推进)→ 用 v1 加载 → 断言只见原 3 行。
  - `lance_source_rejects_missing_embedding_row`(embedding 列 null)。
  - `lance_source_rejects_wrong_dim`(profile.dim=512 但写入 256 维列)。
  - `lance_source_rejects_malformed_metadata_json`(metadata 列存 `"not json"`)。
- [ ] **Step 2:** 失败 → **Step 3:** 实现 → **Step 4:** PASS → **Step 5: Commit** `feat(index): Lance 快照源——pin table version 的确定性全表扫描`

### Task 7: CLI 重接线(static-build → Lance 模式)

**Files:**
- Modify: `src/app.rs`(`run_static_build`:parse config 为 `LanceStaticSourceConfig` → `load_lance_snapshot().await` → `StaticReleaseBuilder::build_release` → 摘要含 release_id;删除本地 JSONL 读取与 `build_embedding_*_from_env` 调用;`parse_static_build_args` 不变,仍 `--config/--output`)
- Modify: `src/bin/ltsearch.rs:12`(usage 文案:`static-build one-shot TurboQuant v3 release from a pinned Lance snapshot: --config <json> --output <dir>`)
- Test: `src/app.rs` 内联测试更新 + `tests/static_release_cli_test.rs`(新)

**Interfaces:**
- Consumes: Task 5 `build_release`、Task 6 `load_lance_snapshot`。
- 保留不动:`TurboBuildConfig`/`parse_static_source_lines`/`load_static_chunks_from_s3`(AWS 孪生仍用;app.rs 不再 import)。

- [ ] **Step 1: 失败测试** `run_static_build_builds_v3_release_from_lance_dataset`:tempdir 里建 Lance fixture(复用 Task 6 helper)→ 写 config JSON → `run_static_build(["--config", ..., "--output", ...]).await` → 断言输出目录 10 文件齐、`MmapIndex::load` 得 version 3、返回摘要含 release_id。
- [ ] **Step 2:** 失败 → **Step 3:** 实现 → **Step 4:** PASS + `cargo build --no-default-features --features local` 通过 → **Step 5: Commit** `feat(local): static-build 切换为 pinned Lance 快照源并产出 v3 release`

### Task 8: 验收——build-twice 逐字节决定性 + AWS-free 核验

**Files:**
- Test: `tests/static_release_determinism_test.rs`(新)

- [ ] **Step 1: 测试** `static_release_build_twice_is_byte_identical_including_manifest`:同一 Lance fixture + 同 config,build 到两个不同输出目录 → 逐文件(**全部 10 个,含 `release_manifest.json`**)`fs::read` 字节比对相等,且两份 manifest 的 `release_id` 相同。metadata 用两种插入顺序构造以踩 HashMap 序列化陷阱。
- [ ] **Step 2:** PASS(此时应直接绿;若红,按 systematic-debugging 排查决定性泄漏点)
- [ ] **Step 3: AWS-free 核验**(feature-matrix 本地复现):

```bash
cargo tree --no-default-features --features local -e normal | grep -E 'aws-config|aws-sdk-s3|aws-sdk-sqs|lambda_runtime' && echo LEAK || echo CLEAN   # 期望 CLEAN
cargo test --no-default-features --features local
cargo build --no-default-features --features aws && cargo build --no-default-features --features lambda   # 孪生路径零回归
cargo clippy --all-targets -- -D warnings && cargo fmt --check
```

- [ ] **Step 4: Commit** `test(index): v3 release build-twice 逐字节决定性验收`

### Task 9: PR 与收尾

- [ ] 自审:逐条核对 issue #110 的 5 条 AC(本计划映射:AC1→Task 4/6,AC2→Task 5/6,AC3→Task 3/5,AC4→Task 4/5,AC5→Task 7+8 核验)
- [ ] `gh pr create` — 标题 `feat(index): build TurboQuant v3 from pinned static Lance release (#110)`,正文 `Closes #110` + AC 映射表 + 验证证据;**不自动合并**
- [ ] Review 意见处理(superpowers:receiving-code-review);合并后:清 worktree/分支、ff main、确认 #110 关闭、更新 epic #132/#106 勾选项

## 验证(端到端)

1. `cargo test`(全量,含 v2 回归)与 Task 8 的四条命令全绿。
2. 手动冒烟:用 Task 6 fixture helper 生成一个真实 Lance 数据集 → `cargo run --bin ltsearch -- static-build --config /tmp/.../config.json --output /tmp/.../release` → 检查 `release_manifest.json` 人读合理(provenance/hash/release_id)→ 再跑一次到新目录 `diff -r` 两目录零差异。
3. CI:push 后 feature-matrix / fast / integration 全绿(Lance 源无 feature 门控,不需要改 workflow)。

## Out of scope(#112)

release 激活/指针 CAS、`try_load_static_searcher` 经指针解析、查询响应暴露 v3 元数据与 `(dynamic_version, static_release_id)` 对。
