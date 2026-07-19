# Issue #112 PR-2 — 查询双版本 + v3 端到端 + 目录约定移除 + AWS 拉取 + e2e

## Context

#112（静态 release 显式激活 + 双版本查询）的 PR-1（#138）已合并 `096d780`：激活命令、`StaticReleaseHead`、SQLite `static_release_head` 表 CAS、AWS `static_activate` bin 全部就位。剩余验收项由 PR-2 交付：

- 查询按静态指针解析、缓存并上报 `(dynamic_version, static_release_id)`，单请求不混 release（AC4）；
- 静态结果 v3 metadata/filter/原始 doc_id/citation 端到端（AC5）——现状 `turbo_searcher` 物化恒 `metadata: None`、doc_id 是 u64 串、citation 只从 title 手搓，`filter.rs:40` 会把 metadata:None 的静态结果全丢；
- 彻底移除隐式 `<root>/static` 目录约定（用户裁决①，pointer-only）；
- AWS 查询侧按指针惰性拉取 release（用户裁决②）。

原计划全文（Task 8-15 骨架 + 设计要点，勿重议）：`docs/superpowers/plans/2026-07-18-issue-112-static-release-activation.md`。本文件是按 main 现状核实后的 PR-2 细化版，含三个 PR-1 携带项：① fs_publish 对 `static/_head` 防御 reject；② `build_v3_release_fixture`/`RecordingPublishStorage` 夹具提取共享；③ `static_activate_cli_test` 收紧为经 `SqliteStaticReleaseStore` 断言。

分支 `feat/112-dual-version-query`，worktree `../LTSearch-issue-112-pr2`。每任务 TDD、每 commit 全绿。PR 正文 `Closes #112`，不自动合并。

## 执行裁定（本次规划新定，PR 正文需向 owner 明示）

1. **`resolve_versioned_handler` 泛型化为 key `K: PartialEq + Clone`**（`CachedQueryHandler<K>`），`QueryService` 固化 `K=(u64, Option<String>)`。既有 u64 护栏测试（`[7,7,8]`、`[7,8,8]` 竞态）原样编译通过，零回归。
2. **不新增静态 retriable 常量**：release 目录内容寻址且不可变、激活不删旧 release，bootstrap 按 load_key 捕获的 release_id 加载，无竞态失败窗口；指针中途翻转只是下次 resolve 的正常缓存失效。dynamic 侧沿用现有 `ACTIVE_VERSION_CHANGED_DURING_BOOTSTRAP` 重试。
3. **/query 与 /health 的 `static_release_id` 都取自 cached pair**（handler 冻结值 / `service.cached_static_release_id()`），不重新读指针。
4. **e2e 并入现有 `local-e2e` job**（不新增 job，`test_ci_workflow.py` 只补 assertIn）；Lance fixture **不能**复用动态管线产物（dim=3 且 `<root>/lance/` 是构建后分片索引非源 documents 表），改用新 `examples/emit_static_lance_fixture.rs`（`required-features=["local"]`；arrow/lancedb 是普通依赖，example 可用）。
5. **AWS sync 顺序 manifest-last 式**：读 `static/_head` 对象入内存 → 若本地缺 release 则拉 `static/releases/<id>/*` → 最后才落 `<root>/static/_head` 文件（崩溃后指针文件只在 release 齐备时出现）。指针读取用 `AwsPublishStorage::read`，release 目录复用 `sync_prefix`。
6. **"二次 sync 不重下"用因果断言**（moto 无请求计数）：首拉后删远端 `static/releases/<id>/*`（留指针），二次 sync 仍 Ok 且本地 manifest 仍在——若错误地重拉会 404 失败。
7. 文档残留（`README.md:89`、`docs/deployment.md:146`、`docs/arch.md:703` 的 `LTSEARCH_QUERY_STATIC_DIR`）做最小行级修正；完整 runbook 属 #113 不做。

## 任务序列

依赖链：T0 → T8 → (T8b ∥ T8c) → T9 → T10 → T11 → T12 → T13 → T14 → T15。

### Task 0: 开工前置
- 清理已合并分支；`git pull --ff-only`；`git worktree add ../LTSearch-issue-112-pr2 -b feat/112-dual-version-query`
- 本计划存入 `docs/superpowers/plans/2026-07-18-issue-112-pr2-dual-version-query.md`，首个 commit

### Task 8: 读侧 StaticReleaseStore（SQLite + 文件 + 分发器）
**Files:** Create `src/storage/static_release_store.rs`（trait `StaticReleaseStore::load_active_release() -> Result<Option<StaticReleaseHead>, StaticReleaseStoreError>`，缺失折叠 `Ok(None)`；`LocalStaticReleaseStore` 读 `<root>/static/_head` 文件）、`src/local/sqlite/static_release.rs`（`SqliteStaticReleaseStore` 经 `SqliteDb` 读 `static_release_head` 行，对齐 `SqliteManifestStore` 同步风格）；Modify `src/storage/mod.rs`、`src/local/sqlite/mod.rs`、`src/local/mod.rs`（导出）、`src/query_lambda.rs`（`static_release_store_for(artifact_root)` 分发器，镜像 `manifest_store_for` :73：`<root>/ltsearch.db` 存在→SQLite，否则文件）。
**测试（内联）：** SQLite 空行→None、经 `LocalPublishStorage` CAS 后读回、坏 JSON→`Invalid`；文件版无文件→None、写 `<dir>/static/_head` 后读回；分发器行为差异断言。
**携带项③同任务完成：** `tests/static_activate_cli_test.rs:148` 断言收紧为经 `SqliteStaticReleaseStore::load_active_release` 读。
**Commit:** `feat(storage): StaticReleaseStore 指针读侧（SQLite+文件）与分发器`

### Task 8b: fs_publish 防御 reject（携带项①）
**Files:** Modify `src/local/fs_publish.rs`——`reject_retired_head`（:21，现只判 `INDEX_HEAD_KEY` 且只在 read/CAS 调用）改为 `reject_head_key`，**精确相等**匹配 `INDEX_HEAD_KEY | STATIC_HEAD_KEY`，并在 `upload_file`（:98）/`upload_directory`（:81）首行也调用。
**测试（内联追加）：** `upload_file(STATIC_HEAD_KEY, …)` 拒绝且不落盘；`STATIC_HEAD_KEY` 的 read/CAS 拒绝；`upload_directory(static_release_dir_key("a"*64), …)` 正常落盘（不误伤）；既有 `head_key_is_retired_and_rejected` 保持绿。
**Commit:** `fix(local): fs_publish 拒绝 static/_head 的 upload/read/CAS，指针仅经 SQLite`

### Task 8c: tests/support 共享夹具（携带项②）
**Files:** Create `tests/support/mod.rs`（`#![allow(dead_code)]`，沿用 `tests/common/mod.rs` 的 `mod xxx;` 惯例）：收敛 `build_v3_release_fixture`/`temp_dir`/`finite_embedding`/`citation_metadata`/`corrupt_one_byte`/`FIXTURE_MODEL_ID`/`RecordingPublishStorage`（含 `conflict_on_compare_and_swap` 预植现值语义）。Modify `tests/static_activation_test.rs`（删 :66 与 :271 副本）、`tests/write_build_publish_test.rs`（删 :513 副本）、`tests/publisher_test.rs`（删 :23 副本）改 `mod support;`。
**约束：** support 内容须 local 与 aws 两 profile 都编译（只依赖 `PublishStorage`/`StaticReleaseBuilder`，无 aws 依赖）。验收 = 三处现有测试全绿（迁移即重构，无新断言）。
**Commit:** `refactor(test): 抽取 tests/support 共享 v3 release fixture 与 RecordingPublishStorage`

### Task 9: TurboQuantSearcher v3 暴露
**Files:** Modify `src/query/turbo_searcher.rs`——只改 materialization（:85-116），按 `self.index.version() == 3` 分支；**不动** `records()`/`TurboRecordSlice`（v3 已被折叠进 `V2Dim512` arm，打分路径同构）。v3：`doc_id = original_doc_id(record_index)`（缺失保守回退 u64 串）、`metadata = serde_json 解析 metadata_json(...)`、`citation = Citation::from_metadata(&md)`（`src/models/search.rs:131`）；解析失败回退 `metadata: None` + `log::warn!`（searcher 无 WarningSink 句柄），不 panic。v2 现行为逐字不变。
**Test:** 新 `tests/turbo_searcher_v3_test.rs`（用 support 夹具建含 citation 字段 metadata 的真 v3 release，`MmapIndex::load` + `Box::leak`）：
- `v3_searcher_exposes_original_doc_id_metadata_and_citation`（doc-α / resource_id / url / title / lang 断言）
- `v3_searcher_results_survive_metadata_filter`（`apply_filters` lang==zh 留 α 剔 β）
- `v3_searcher_falls_back_to_none_on_unparseable_metadata`（篡改 metadata_json 为合法 UTF-8 但非 JSON → None 不 panic）
- v2 护栏 `v2_searcher_keeps_hashed_docid_and_none_metadata`
**Commit:** `feat(query): TurboQuantSearcher 暴露 v3 原始 doc_id/metadata/citation 并支持 filter`

### Task 10: SearchResponse & HealthBody 增 static_release_id
**Files:** Modify `src/models/search.rs`（`SearchResponse` :182 增 `#[serde(default, skip_serializing_if = "Option::is_none")] pub static_release_id: Option<String>`）、`src/query/router.rs`（增**数据字段** `static_release_id: Option<String>` + `with_static_release_id(mut self, …) -> Self`——非泛型换型 builder；`search()` 组装时写入）、`src/http/mod.rs`（`HealthBody` :33 增同名字段）、`src/http/query.rs`（`/health` 成功分支填 `service.cached_static_release_id()`，其余分支 None）。
**Test:** router 内联（设/不设两例）+ `tests/http_query_test.rs` 追加（seed `<root>/static/_head` 后 `/health` 含字段；无指针为 null。注意 :75/:167 的 `remove_var("LTSEARCH_QUERY_STATIC_DIR")` 留待 T11/T13 删）。
**Commit:** `feat(query): 响应与健康检查上报 static_release_id`

### Task 11: 缓存键 (dynamic_version, static_release_id) + 指针解析加载（删目录 env）
**Files:** Modify `src/query_service.rs`（`CachedQueryHandler<K>` + `resolve_versioned_handler`/`_with_retry` 泛型化〔裁定 1〕；`resolve_handler` load_key 组合 dynamic version + `static_release_store_for(&root).load_active_release()?.map(|h| h.release_id)`；`cached_version()` 取 key.0，新增 `cached_static_release_id()` 取 key.1）、`src/query_lambda.rs`（bootstrap 接 pair：dynamic 冻结不变〔`FixedManifestStore` + 现有版本竞态重试〕，静态按传入 id 装载并 `with_static_release_id`；`try_load_static_searcher(artifact_root, release_id: Option<&str>)` 改为 `None → Ok(None)`、`Some(id) → MmapIndex::load(<root>/static/releases/<id>/) + Box::leak`；**删除** :255-259 的 `LTSEARCH_QUERY_STATIC_DIR` env 与 `.join("static")` 逻辑）。
**Test:** `tests/query_service_test.rs` 追加（K=元组）：
- `cache_rebuilds_when_static_release_changes_even_if_dynamic_version_stable`（dynamic 恒 7，static [r1,r1,r2] → bootstrap 2 次）
- `cache_rebuilds_when_dynamic_version_changes_static_stable`（[7,7,8] × 恒 r1 → 2 次）
- `cache_hits_when_pair_unchanged`（(7,None) 三连 → 1 次）
- `retry_covers_dynamic_version_change_with_static_stable`（证裁定 2：静态无需独立 retriable）
- 既有两条 u64 护栏测试**不改仍绿**
**Commit:** `feat(query): 查询按 (dynamic_version, static_release_id) 对解析与缓存，单请求不混 release`

### Task 12: AWS 查询侧拉取（指针 + 按 release_id 惰性拉）
**Files:** Modify `src/adapters/s3_artifact_sync.rs`——`synced_artifact_prefixes()` 改 `["index/", "lance/"]`；`sync` 追加（按裁定 5 顺序）：读 `static/_head` 对象（`AwsPublishStorage::read` 或等价 get）入内存 → `StaticReleaseHead::from_json` 得 id → 本地 `<root>/static/releases/<id>/release_manifest.json` 不存在才 `sync_prefix("static/releases/<id>/")` → 最后 `fs::write(<root>/static/_head)`（裸写，绕开 fs_publish 守卫）；新增 `pub fn with_client(bucket, client)`（测试注入 Moto client，避免进程 env 竞态；`new` 保留走 `s3_client_from_env`）。
**Test:** 内联守卫改为 `synced_artifact_prefixes_excludes_static`；`tests/write_build_publish_test.rs` 追加 `s3_sync_pulls_pointer_and_active_release_once`（MotoHarness：upload+activate → sync 断两文件落盘 → 删远端 release 对象 → 二次 sync 仍 Ok 且本地 manifest 在〔裁定 6〕）。
**Commit:** `feat(aws): 查询侧按指针 release_id 惰性拉取 v3 release，去掉 static 批量前缀`

### Task 13: 移除目录约定残留
**Files:** Modify `src/indexing/publisher.rs`（删 :174-178 `upload_directory("static",…,Overwrite)` 块 + :412-428 `static_artifact_source`）、`src/index/mmap_index.rs`（删 :14 `IMAGE_STATIC_DIR`、:336 `load_from_image`、:340 `global_from_image` 及关联 static——已核实零调用方纯死代码）、`Dockerfile`（删 :16-20 COPY/ENV；连带删 `static/.gitkeep` 与 `.gitignore:14` `static/*.bin`）、`tests/http_query_test.rs`（删 :75/:167 remove_var）、`tests/publisher_test.rs`（删 `create_static_source_build` :230-236，:317-349 断言改写为 `dynamic_publish_produces_no_static_keys`）、文档最小修正（README.md:89、docs/deployment.md:146、docs/arch.md:703）。Create `tests/test_image_no_static_convention.py`（守卫：Dockerfile 无 `/app/static`/`LTSEARCH_QUERY_STATIC_DIR`；mmap_index.rs 无 `IMAGE_STATIC_DIR`）。
**门禁：** `grep -rn 'LTSEARCH_QUERY_STATIC_DIR\|IMAGE_STATIC_DIR\|/app/static' src/ tests/ scripts/ Dockerfile` 无残留；全量测试 + 双 profile build 绿。
**Commit:** `refactor: 移除隐式 static 目录约定，静态服务只经指针解析`

### Task 14: e2e — build→activate→query v3 端到端
**Files:** Create `examples/emit_static_lance_fixture.rs`（`required-features=["local"]`，`Cargo.toml` 加 `[[example]]`；arrow+lancedb 造 512 维 documents 表，metadata 含 resource_id/source_type/source_ref/title/url/lang，写法对齐 `tests/static_release_cli_test.rs`）、`scripts/e2e/run-static-release-flow.sh`、`tests/test_static_release_e2e.py`（结构守卫：脚本存在、build→activate→query 顺序、`static_release_id`/`citation`/`/health` 翻转字样）。Modify `.github/workflows/ci.yml`（`local-e2e` job 加 example 构建步 + 脚本步）、`tests/test_ci_workflow.py`（`local_e2e` 块补两条 assertIn；job 集合仍 9 个不变）。
**脚本流程**（local profile、moto/docker-free，复用 lib.sh 与 run-local-server-flow.sh 范式）：
1. example 产 Lance fixture（数据集 A）；
2. 最小动态管线 write→build（`LTSEARCH_BUILD_FIXED_EMBEDDING` 512 维——query bootstrap 校验 embedding dim 与 dynamic manifest 一致，故动态侧也须 512）；
3. `ltsearch static-build --config → --output relA` → `ltsearch static-activate --release relA --root $ROOT`；
4. 起 `ltsearch query`（`LTSEARCH_QUERY_EMBEDDING_PROVIDER=fixed` + 512 维 `LTSEARCH_QUERY_FIXED_EMBEDDING`），`POST /query`（带 `filters:{"lang":"zh"}`、`include_metadata:true`）：断 `static_chunks[0].doc_id` 为原始串、`.citation` 非空、`static_release_id == 激活 id`（内联 python3 断言，请求 JSON 内联不动 `test_e2e_fixtures.py`）；
5. 改一行 fixture → 造 relB → activate → `/health` 的 `static_release_id` 翻转 + 下一请求命中新 release。
**Commit:** `test(e2e): static-build→activate→query v3 端到端流并入 local-e2e`

### Task 15: 验收 + 开 PR + 收尾
- 全量核验（原计划「验证」节）：local build/test + `cargo tree` AWS-free 无泄漏；aws build/test；lambda build；clippy `-D warnings` + fmt；`python3 -m pytest tests/test_*.py -q`；`bash scripts/e2e/run-static-release-flow.sh`；手动冒烟（含激活第二 release 翻转）。
- `gh pr create` 标题 `feat(query): 双版本查询与 v3 静态检索端到端 (#112 PR-2)`，正文 `Closes #112` + AC 映射 + 本文件「执行裁定」摘录 + 验证证据；**不自动合并**；review 走 superpowers:receiving-code-review。
- 合并后：清 worktree/分支、ff main、确认 #112 关闭、勾 epic #132/#106 对应项、更新记忆。

## AC 映射

| AC | 覆盖 |
|---|---|
| AC4 pair 缓存/上报、不混 release | T8 + T10 + T11 |
| AC5 v3 metadata/filter/原始 ID/citation 端到端 | T9 + T10 + T13 + T14 |
| AC1/AC2/AC3 | PR-1 已交付；T13 拔掉隐式入口补全 AC1 |

## 风险
- `resolve_versioned_handler` 泛型化触碰 pub API：唯一外部调用方是 `tests/query_service_test.rs`，K 推断使旧测试免改；若编译推断问题，退路是保留 u64 版薄壳。
- e2e 依赖动态+静态双管线同 512 维：脚本内两侧 env 必须一致，写死同一常量。
- `examples/` 首个 example：确认 `cargo build --no-default-features --features local --example …` 在 feature-matrix 下不进默认构建面（example 仅 local-e2e job 构建）。
