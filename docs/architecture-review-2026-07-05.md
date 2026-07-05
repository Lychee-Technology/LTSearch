# LTSearch 架构评审与改进方案（2026-07-05）

本文记录一次完整的架构评审：方法、发现、六个改进候选的取舍、以及最终在
`arch/deepening-refactor` 分支落地的五个重构。目的除了留档，也是给后续评审
划定基线——哪些问题已经解决、哪些是**有意保留**的现状，避免重复提案。

## 1. 评审方法

采用"深模块/浅模块"分析框架：

- **Module（模块）**：任何有 interface 与 implementation 的单元（函数、struct、trait、目录）。
- **Interface（接口）**：调用方必须知道的一切——类型签名之外还包括不变量、错误模式、环境变量、调用顺序约束。
- **深（Deep）**：小接口背后藏着大量行为，调用方获得杠杆（leverage）；**浅（Shallow）**：接口复杂度接近实现复杂度，抽象不挣钱。
- **Seam（接缝）**：接口所在处，可以在不改调用方的情况下替换行为。一个 adapter 的 seam 是假设性的；两个 adapter（含测试替身）才是真实的。
- **Deletion test（删除测验）**：想象删掉这个模块——如果复杂度凭空消失，它是直通层；如果复杂度会在 N 个调用方重现，它在挣钱。
- **Locality（局部性）**：一个概念的知识（变更、bug、校验）集中在一处的程度。

## 2. 总体判断

**分层干净、无循环依赖、测试投入充足**（tests/ 39 个文件，规模超过 src/）。
依赖方向经验证为无环的分层：

```
error / models（叶）
    ↑
storage / embedding / index
    ↑
write / indexing / query
    ↑
adapters + *_lambda 入口（bin）
```

主要摩擦不在分层，而在四类问题：

1. **死代码与假 seam 稀释导航性**——搜索一个概念会命中两套定义；
2. **关键契约缺乏 locality**——`_head` 版本指针被读写两侧各自定义、各自校验；
3. **bin 层脚手架四份拷贝**，且 write/build bin 绕过了被测试的 lib handler，测试验证的是一条平行路径；
4. **横切逻辑在三个 searcher 间复制粘贴**，其中包括最不该有两份的路径逃逸安全校验。

## 3. 改进候选与落地情况

### C1. 删除死代码与假 seam 【Strong｜已落地 `4d4eeca`】

**发现**：
- `src/turbo/`（~596 行）是被 `src/index/`（512 维 `TurboRecord512` 布局）取代的
  384 维旧 TurboQuant 完整并行实现，自带 `MmapIndex`/`encoder`/`scorer`/`types`，
  经 grep 验证**零外部引用**，但仍在编译、仍有单测在跑。任何人搜 `MmapIndex`、
  `ProjectionMatrix`、`MetaRecord` 都会命中两套定义。
- `src/bin/ltsearch.rs` 是空 `main`；`src/config.rs` 是无人使用的 `AppConfig` 存根；
  10 个 `mod.rs` 里的空结构体 `ModuleBoundary` 仅被一个 bootstrap 冒烟测试引用；
  `publisher.rs::ensure_file_exists` 已标 `#[allow(dead_code)]`。

**处理**：全部删除，净减 680 行。这些都是删除测验的反例——删掉后复杂度直接消失。

**有意保留**：`ContextBuilder` 与 `SearchRequest.corpus_weights` 目前在线上查询
路径中无消费方，但作为未来 RAG prompt 组装的预留接口**经确认保留**。后续评审
不应再提案删除。

### C2. 统一 `_head` 契约 + 真原子 CAS 【Strong｜已落地 `14eb4a8`】

**发现**（本次评审唯一的正确性风险）：
1. 版本指针 `_head` 被两个模块各自定义：写侧 `publisher.rs::HeadDocument` 与读侧
   `manifest_store.rs::ManifestHead` 是同一 JSON 的两个独立 struct，
   各自重复实现校验（version_id 非零、updated_at 合理性、
   `manifest_path == version_manifest_key(version_id)`）。改一侧不改另一侧即静默契约漂移。
2. `AwsPublishStorage::compare_and_swap` 是 **get-比较-put 三步，并非原子**。
   两个并发 index builder 可互相覆盖 `_head`，静默丢失一次发布。

**处理**：
- 新建 `storage/head.rs`：`ManifestHead` 单点拥有构造（`new` 从 version_id 派生
  manifest_path，杜绝不一致）、序列化、解析、校验；publisher 与 manifest_store
  都消费它，错误消息与原实现逐字兼容。
- `PublishStorage` 的 seam 从"字节比较"改为"版本标签"：`read` 返回
  `VersionedObject { bytes, etag }`，`compare_and_swap` 接受 `expected_etag`，
  AWS 实现用 S3 条件写（存在时 `If-Match: <etag>`，创建时 `If-None-Match: *`），
  412/409 映射为 `Ok(false)`。CAS 从"靠运气"变为"靠 S3 保证"。
- Moto 集成测试新增陈旧 ETag 拒绝、重复创建拒绝两个用例，均通过。

### C3. 组合根收敛 【Strong｜已落地 `0af85f0`】

**发现**：
- `s3_client_from_env`（endpoint override + force_path_style）在 4 个 bin 里各有一份
  拷贝；`build_embedding_generator` 在两个 builder bin 里重复（仅错误类型不同——
  上一个 "static builder parity" commit 正是这次复制的来源）。
- **write/build bin 绕过了被测试的代码**：lib 层的 `handle_write_request`/
  `handle_build_request` 只有 tests/ 在调用，真实 bin 内联了同样的 match。
- 配置静默降级：`LTSEARCH_WRITE_S3_BUCKET` 等用 `.unwrap_or_default()` 得到
  空字符串，错误推迟到第一次 S3 调用时才以难以理解的形式暴露。

**处理**：
- 新建 `src/bootstrap.rs` 作为组合根：AWS 客户端构造、build 侧嵌入生成器构造
  各一份；`WriteConfig::from_env()` / `BuildConfig::from_env()` 类型化配置，
  缺失必填项返回 `BootstrapError::MissingEnv`，在 `main()` 冷启动即失败。
- 两个 lib handler 改为 `AsyncFnOnce` 签名，write/build bin 改为真正调用它们
  （与 query bin 对齐），测试路径与生产路径重合。
- 顺带的性能修正：AWS 客户端与配置从每次调用构造改为 `main()` 构造一次、
  warm invocation 复用。

### C4. 检索公共内核 + 拆解 vector_searcher 【已落地 `491192f`】

**发现**：三个 searcher（vector/keyword/turbo）复制了五类逻辑：
`validate_top_k`（3 份 + models 里第 4 份范围检查）、路径逃逸/软链校验（vector 与
keyword 近乎逐行相同——**安全校验存在两份意味着补丁只打一半**）、score tie-break
比较器（4 份）、doc_id 去重（2 份）、`validate_query_embedding`（2 份）。
另外 `vector_searcher.rs`（689 行，仓库最大文件）混杂四个职责：LanceDB 查询、
Arrow 解码、shard 缓存记账、路径安全校验。

**处理**：
- 新建 `query/retrieval_common.rs`：上述五类逻辑各存一份；`resolve_artifact_path`
  以 `kind` 参数（"local LanceDB" / "Tantivy"）保持错误消息逐字兼容；
  `TOP_K_MAX` 单一定义收敛到 `models::search`。
- `vector_searcher.rs` 拆出 `lance_cache.rs`（缓存记账 + shard 目录安全遍历）与
  `lance_decode.rs`（Arrow → SearchResult），主文件回到"打开 shard、发查询、解码"
  单一职责（689 → ~370 行）。
- keyword 的 tie-break 从 `partial_cmp().unwrap()` 统一为 `total_cmp`，消除了
  NaN 触发 panic 的隐患。

### C5. 暂存-发布事务共享 【已落地 `c9d1c23`】

**发现**：`LocalIndexBuilder::publish_staged_build` 与
`StaticIndexBuilder::publish_staged_output` 各自实现了"写 .staging → rename 落位 →
级联清理"的文件系统事务，零共享。且静态侧较弱：暂存目录名固定（并发构建互踩）、
rename 失败后不清理已落位的部分文件（留下混版索引）。

**处理**：新建 `storage/staged_publish.rs`——`StagedDir::create / commit / abort`
一处实现事务语义（有序 rename、失败时回收已移动目标与暂存根、清理失败追加到
原错误）。两个 builder 都改为消费它；静态 builder 顺带获得唯一暂存目录名与
失败清理。动态侧的错误消息与 `.index-build-staging-` 前缀保持不变。

### C6. Router 泛型与融合参数 【Speculative｜有意不动】

`QueryRouter` 的 6 个泛型参数与 `KeywordRetriever`/`VectorRetriever` 单实现 trait
看似浅层直通，但 `router_test.rs`（1541 行）重度依赖这些替换点——**测试即第二个
adapter，seam 是真实的**，故保留。两个记录在案的小改进（未做）：

- `rrf_k = 60.0` 硬编码在 `QueryRouter::new`（router.rs），应改为可注入；
- `query_lambda.rs` bootstrap 里 `Box::leak(MmapIndex)` 换成 `Arc<MmapIndex>`，
  消除每次版本切换 re-bootstrap 时永久泄漏的 mmap 句柄。

## 4. 明确延后的事项

**Manifest 假桶名迁移**（原属 C5 范围，评审后判定为契约迁移、单独立项）：
`builder.rs` 在 manifest 中写入 `s3://local-artifacts/...` 假 URL，publisher 与
query 侧读取时又将桶名剥掉。改为 bucket-relative key 需要同时动：
`models/index.rs::validate_s3_uri` 的模型校验、query 侧 `resolve_artifact_path`、
publisher `s3_key`、约 10 个测试文件，**且会破坏已发布 manifest 的向后兼容**。
正确路径是先让读取侧同时接受两种形式，再切换写入侧，最后收紧校验——
应作为独立变更执行。

## 5. 验证

每个候选独立成 commit，每步均通过：

- `bash scripts/verify-fast.sh`（构建全部 Lambda 二进制 + 22 个非 Moto 测试套件 +
  `cargo fmt --check` + `cargo clippy --all-targets --all-features -D warnings`）
- `bash scripts/verify-moto.sh`（S3/SQS adapter 集成，16 个用例，含新增的
  条件写 CAS 用例）
- `bash scripts/e2e/run-sam-local-invoke-e2e.sh`（SAM 全链路 write → build → query，
  fixed embedding，与 CI 一致）

## 6. 附：本次涉及的模块清单

| 变更 | 新增/删除 |
| --- | --- |
| 删除 | `src/turbo/`、`src/bin/ltsearch.rs`、`src/config.rs`、`ModuleBoundary`×10、`tests/workspace_bootstrap_test.rs` |
| 新增 | `src/bootstrap.rs`、`src/storage/head.rs`、`src/storage/staged_publish.rs`、`src/query/retrieval_common.rs`、`src/query/lance_cache.rs`、`src/query/lance_decode.rs` |
| 重构 | 4 个 bin、`src/write_lambda.rs`、`src/build_lambda.rs`、`src/indexing/{builder,publisher}.rs`、`src/adapters/s3_publish.rs`、`src/storage/manifest_store.rs`、`src/query/{vector,keyword,turbo}_searcher.rs` |
