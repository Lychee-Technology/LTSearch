# Issue #111 — LTEmbed 模型资产交付实现计划(v2:S3→/tmp 改道)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **v2 说明**:v1(Lambda Layer 路线)被尺寸 spike 决策门否决——实测 real 函数 ZIP 解压 235.7 MiB(strip 后 180.6 MiB),+ Layer 解压 139.0 MiB 远超 Lambda「函数+Layer 解压合计 ≤ 250MB」硬限;预算杀手是 lance/datafusion 依赖体量而非模型。实测表与裁决见 issue #111 评论(2026-07-17)。v1 已提交且保留的工件:`sam/builder.Dockerfile` 的 `bundle` stage + sha256 pin。本文档 v2 全面替换 v1 的任务表。

**Goal:** query/index-builder ZIP 函数冷启动时从 S3 拉取 pinned LTEmbed 模型资产(逐文件 sha256 校验)到 `/tmp/ltembed`;write 函数零模型依赖;打包自动化记录 hash、强制 strip、断言函数 ZIP 250MB 预算与 arm64 架构;启动失败清晰指出资产未就位。单 PR,`Closes #111`(AC 按改道调整,已在 issue 评论说明)。

**Architecture:** 资产供给三层:(1) `bundle` stage(已提交)下载+sha256 校验 pinned tarball;(2) `scripts/package-model-assets.sh` 提取到 `dist/model-assets/`(manifest.json 含逐文件 sha256/bytes + provenance),部署前上传到函数可读的 S3 前缀(e2e 上传到 moto);(3) Rust `src/embedding/model_assets.rs`(gate `aws`+`ltembed`)在两个 lambda bin 的 async main 启动期按 `LTSEARCH_{SIDE}_LTEMBED_S3_{BUCKET,PREFIX}` 下载 manifest → 逐文件 GET+sha256 验证 → temp+rename 落盘 → manifest.json 最后写入作完整性标记;warm 容器 `/tmp` 复用经 manifest+尺寸快查跳过。`from_config` 的路径预检与既有维度校验做运行期兜底。strip 纳入 `package-lambda-zips.sh`(在 builder 镜像内执行,ZIP lineage 专属),函数 ZIP 解压从 235.7 → ~180 MiB,恢复对 250MB 单函数硬限的余量。

**Tech Stack:** Rust(aws-sdk-s3 既有依赖 + 新增可选 `sha2`)、bash + python3 heredoc、Docker bundle stage、AWS SAM(无 Layer)、moto e2e、python unittest 结构守卫。

## Global Constraints(继承 v1)

- 所有 Lambda 产物 linux/arm64;编译只在 AL2023 容器内;不引入 cargo-lambda。
- `local` profile 保持 AWS-free;新增 `sha2` 挂在 `ltembed` feature 下(纯 Rust,已在 aws-sigv4 依赖树中)。
- e2e 脚本 + python 结构守卫成对;PR `Closes #111`,不自动合并。
- pin 单一来源:`sam/builder.Dockerfile` bundle stage 的 `LTEMBED_BUNDLE_URL` + `LTEMBED_BUNDLE_SHA256`。

## 关键数字(spike 实测,2026-07-17)

| 项 | 值 |
|---|---|
| bundle tarball(v1.0.9)压缩 / sha256 | 119,571,616 B / `4d781723…6af9` |
| 资产解压合计(5 文件) | 145,785,876 B(139.0 MiB) |
| real query ZIP 解压(未 strip / strip) | 235.7 / 180.6 MiB |
| 单函数 250MB 硬限余量(strip 后) | ~69 MiB |
| `/tmp` 默认 ephemeral | 512 MB(资产 139 MiB + 查询 artifacts,余量足) |

---

## Task 2v2: 打包自动化 — strip + model-assets + 预算守卫

**Files:**
- Modify: `scripts/package-lambda-zips.sh`(提取后在 builder 镜像内 strip)
- Create: `scripts/package-model-assets.sh`(替换 v1 的 package-model-layer.sh;`--target bundle` build → `dist/model-assets/{model.ort,tokenizer.json,build-info.json,libonnxruntime.so,SHA256SUMS}` + `manifest.json`)
- Rewrite: `scripts/check-lambda-size-budget.sh`(v2:三函数各自解压 ≤ 250MB 硬限断言 + AArch64 ELF + `dist/model-assets` manifest sha256 复核 + 资产 ≤ 350MiB /tmp 预算 + 尺寸表)
- Rename+rewrite: `tests/test_model_layer_packaging.py` → `tests/test_model_assets_packaging.py`

**manifest.json 契约(Rust 侧按此反序列化,多余字段忽略):**
```json
{"bundle_url": "...", "bundle_sha256": "...", "arch": "aarch64", "tmp_path": "/tmp/ltembed",
 "files": [{"name": "model.ort", "bytes": 123754192, "sha256": "..."}, ...]}
```

**Steps:** 守卫红 → 三脚本落地 → 守卫绿 → `package-model-assets.sh` + real zips + budget **必须 PASS**(strip 后)→ commit `feat(packaging): strip lambda zips, stage S3 model assets with hash manifest (#111)`。

## Task 3v2: Rust 冷启动资产供给 + 启动错误文案

**Files:**
- Create: `src/embedding/model_assets.rs`(`#[cfg(all(feature = "aws", feature = "ltembed"))]`,在 `src/embedding/mod.rs` 挂载)
- Modify: `src/bin/query_lambda.rs`、`src/bin/index_builder_lambda.rs`(main 启动期 provider==ltembed 且 S3 env 齐时 provision;失败即 init 报错退出)
- Modify: `src/embedding/ltembed.rs`(v1 预检文案「Layer not attached」→ 资产未就位 + S3 提示)
- Modify: `Cargo.toml`(`sha2` optional,入 `ltembed` feature)

**接口:**
```rust
pub struct ModelAssetSource { pub bucket: String, pub prefix: String }
// 读 LTSEARCH_{side}_LTEMBED_S3_BUCKET/_S3_PREFIX;都缺 → Ok(None);缺一 → Err
pub fn model_asset_source_from_env(side: &str) -> Result<Option<ModelAssetSource>, String>
pub async fn provision_model_assets(client: &aws_sdk_s3::Client, source: &ModelAssetSource, bundle_dir: &str) -> Result<(), String>
```
warm 快查:本地 `manifest.json` 可解析且 files 尺寸全匹配 → skip;下载序:文件 temp+rename,manifest 最后写。错误文案含 `model assets not provisioned` + s3 URI + bundle_dir。

**单测:** env 组合(None/Err/Some)、sha256_hex、warm 快查(tempdir 构造);S3 路径交给 e2e。`cargo test --no-default-features --features lambda,ltembed`。

## Task 4v2: template.yaml — S3 资产 env + 默认 ltembed/512(无 Layer)

- 无 `LayerVersion`/`Layers`(v1 未提交的模板改动全部替换)。
- Parameters:`EmbeddingProvider` Default `ltembed`、`EmbeddingDim` Default `'512'`、`FixedEmbedding` 保留、新增 `ModelAssetPrefix`(Default `ltembed/v1.0.9`)。
- Query/Build env 增:`LTSEARCH_{SIDE}_LTEMBED_S3_BUCKET: !Ref ArtifactBucket`、`LTSEARCH_{SIDE}_LTEMBED_S3_PREFIX: !Ref ModelAssetPrefix`、`LTSEARCH_{SIDE}_LTEMBED_BUNDLE_DIR: /tmp/ltembed`、`LTSEARCH_{SIDE}_LTEMBED_MODEL_PATH: /tmp/ltembed/model.ort`;Write 零模型 env(AC-5)。
- `scripts/e2e/lib.sh` 回退 v1 的 keep-layers 改动(不再需要)。
- 守卫:`test_model_assets_packaging.py` 断模板 S3/tmp env 与 Write 排除;`test_lambda_zip_packaging.py` 翻默认断言(同 v1)。
- 验证:守卫绿 + `sam validate --lint` + stub zip e2e 回归绿。

## Task 5v2: real 模式 S3-assets e2e + CI job

- Create `scripts/e2e/run-sam-ltembed-invoke-e2e.sh`:prepare checkout → real zips + model-assets → budget PASS → `aws s3 cp dist/model-assets → moto bucket/ltembed/v1.0.9/` → 派生模板(通用 make_zip_e2e_template,注入 moto env;不覆盖 provider/dim,走默认 ltembed/512)→ write→SQS→build→query 断言(200 / index_version 1 / doc-rust-hybrid)。
- ci.yml 加 `sam-ltembed-e2e` job(镜像 sam-zip-e2e 结构 + rust toolchain);`test_ci_workflow.py` job 集合更新。

## Task 6v2: 文档

- v1 未提交的 docs 修正改写为 S3→/tmp 叙事:250MB 断言更正为「模型资产本身可入 Layer,但函数二进制 + Layer 合计超限(实测表)→ ZIP 路线经 S3→/tmp 冷启动供给」;`/tmp/ltembed` 路径表;strip 纳入打包;write 独立部署;容器镜像 lineage 保留。

## Task 7v2: 全量验证 + PR

- `cargo test` 两档 + clippy、全部 python 守卫、`sam validate`、stub zip e2e、(本机可选)ltembed e2e;PR 正文:AC 调整说明(Layer→S3,引 issue 评论)+ spike 表 + `Closes #111`;不自动合并。

## AC 映射(改道后)

| 原 AC | 调整后落点 |
|---|---|
| 1. Layer 解到 /opt 文档化路径 | S3 前缀 → `/tmp/ltembed` 文档化路径(T4/T6) |
| 2. Layer+ZIP 同 arm64 | 资产+ZIP 同 arm64:bundle stage pin + budget 脚本 ELF 断言(T2) |
| 3. hash + 压缩/解压/架构/预算检查 | manifest sha256 + strip + 250MB 单函数断言 + /tmp 预算(T2,入 CI T5) |
| 4. 启动失败指出 layer 缺失/不兼容 | provision 失败/路径预检指出资产未就位+S3 URI(T3) |
| 5. write 不挂 layer 可部署 | write 零模型 env/零 fetch 代码(T4) |
