# Issue #113: 统一 local image 与 Lambda ZIP release artifacts — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用 tag 触发的 release 自动化取代组件镜像发布：发布一个 local OCI image（`ghcr.io/lychee-technology/ltsearch-local`）+ 3 个 Lambda 函数 ZIP + `model-assets.zip`，全部带 SHA256SUMS 与 provenance；删除 server 镜像栈与 image-based Lambda 残留；文档校准到实际形态。

**Architecture:** 新增 `scripts/package-release.sh` 作为本地可跑的 release 组装器（复用既有三个打包脚本），`release.yml` 是它的薄封装（tag `v*` 触发 → GitHub Release + GHCR push）；CI 新增 `release-assembly` job 以 stub 模式校验组装路径而不发布。结构守卫测试先行（TDD），墓碑测试把删除固化。

**Tech Stack:** GitHub Actions（ubuntu-24.04-arm）、`gh` CLI、docker（arm64、无 buildx）、bash + python3 heredoc、纯 unittest 结构守卫。

## Global Constraints

- issue AC 的「model-layer ZIP」按 #111 改道调整为 `model-assets.zip`（S3→/tmp 冷启动，非 Lambda Layer），PR/issue 中说明。
- `server` cargo feature 必须保留（`local` 与 `aws` 共用 axum）；只删 3 个 `*_server` bin target。
- `local` profile 保持 AWS-free 依赖图（feature-matrix 守卫）。
- `ci.yml` 全文（含注释）不得含 ASCII 子串 "deploy"（`tests/test_ci_workflow.py` 墓碑断言）。
- 镜像 arm64-only，`docker build --platform linux/arm64` + `docker inspect` 架构断言（#130 口径），不引入 buildx。
- 不引入 cargo-lambda / 第三方 release action；GitHub Release 用预装 `gh` CLI。
- `docs/superpowers/plans|specs/` 快照文档不改（#130 口径）；`docs/architecture-review-2026-07-05.md` 是日期快照，不改。
- 分支 `feat/113-release-artifacts`，单 PR `Closes #113`，不自动合并。

## 用户裁决（2026-07-19）

1. server 栈彻底删除（含 bin、Dockerfile、http-e2e、docker-compose.http.yml、publish-images.yml）。
2. image-based Lambda 全部移除（sam-e2e job、template.sam-e2e.yaml、`*_lambda.Dockerfile`、根 Dockerfile）。
3. tag `v*` 触发 release.yml；main push 只跑 CI。
4. 模型资产发布为 `model-assets.zip`。

派生（决策 2 强制后果，PR 说明）：`scripts/e2e/{start-sam-moto,stop-sam-moto,run-http-flow}.sh` + `tests/test_sam_start_api_e2e.py` 独占依赖 template.sam-e2e.yaml，一并删除。

---

### Task 1: 结构守卫先行（red）

**Files:**
- Create: `tests/test_release_workflow.py`
- Modify: `tests/test_ci_workflow.py`（job 集合与断言块）
- Modify: `tests/test_image_no_static_convention.py`（改指 `sam/local.Dockerfile`）
- Modify: `tests/test_readme_workflow.py`（增 Releases 断言）
- Delete: `tests/test_sam_invoke_e2e.py`、`tests/test_sam_start_api_e2e.py`

**Interfaces:**
- Produces: 守卫锁定的契约——`scripts/package-release.sh --mode real|stub --version <tag>` 产出 `dist/release/{query_lambda.zip,write_lambda.zip,index_builder_lambda.zip,model-assets.zip,release-provenance.json,SHA256SUMS}`；`.github/workflows/release.yml` 结构；`ci.yml` 含 `release-assembly` job。后续任务按此实现。

- [ ] **Step 1: 写 `tests/test_release_workflow.py`**（四个测试：release.yml 结构、package-release.sh 契约、ci release-assembly、墓碑）——完整代码见任务执行时按本文档 §契约 编写，风格仿 `test_ci_workflow.py`（纯 unittest、字符串断言、`_parse_jobs`）。墓碑清单：`.github/workflows/publish-images.yml`、`template.sam-e2e.yaml`、`docker-compose.http.yml`、根 `Dockerfile`、`sam/{query,write,index_builder}_{server,lambda}.Dockerfile`（6 个）、`src/bin/{query,write,index_builder}_server.rs`（3 个）、`scripts/e2e/{run-http-server-flow,run-sam-local-invoke-e2e,start-sam-moto,stop-sam-moto,run-http-flow}.sh`（5 个）均不存在；`Cargo.toml` 无 `query_server|write_server|index_builder_server`。
- [ ] **Step 2: 更新 `test_ci_workflow.py`**：job 集合改 `{fast, feature-matrix, integration, sam-zip-e2e, sam-ltembed-e2e, local-image-e2e, local-e2e, release-assembly}`；删 `sam_e2e`（L115-131）与 `http_e2e`（L133-152）断言块；`fast` 块加 `assertIn("run: python3 -B tests/test_release_workflow.py")`；新增 `release-assembly` 断言块（`needs: integration`、`--mode stub`、`sha256sum -c SHA256SUMS`、`assertNotIn("ghcr")`、`assertNotIn("gh release")`）。
- [ ] **Step 3: `test_image_no_static_convention.py` 改 `DOCKERFILE_PATH = REPO_ROOT / "sam" / "local.Dockerfile"`**，docstring 更新。
- [ ] **Step 4: `test_readme_workflow.py` 增** `test_readme_documents_release_artifacts`：`assertIn("## Releases")`、`assertIn("scripts/package-release.sh")`、`assertIn("ghcr.io/lychee-technology/ltsearch-local")`、`assertNotIn("ltsearch-query-server")`。
- [ ] **Step 5: `git rm tests/test_sam_invoke_e2e.py tests/test_sam_start_api_e2e.py`**
- [ ] **Step 6: 验证 red**：`python3 -B tests/test_release_workflow.py` 失败（目标文件未建）；`python3 -B tests/test_ci_workflow.py` 失败（job 未删/未加）。
- [ ] **Step 7: Commit** `test(release): 结构守卫先行——release 产物契约 + 退役墓碑（#113）`

### Task 2: 删除清单（AC-d）

**Files:**
- Delete: `.github/workflows/publish-images.yml`、`sam/{query,write,index_builder}_server.Dockerfile`、`src/bin/{query,write,index_builder}_server.rs`、`docker-compose.http.yml`、`scripts/e2e/run-http-server-flow.sh`、`template.sam-e2e.yaml`、`sam/{query,write,index_builder}_lambda.Dockerfile`、`Dockerfile`（根）、`scripts/e2e/{run-sam-local-invoke-e2e,start-sam-moto,stop-sam-moto,run-http-flow}.sh`
- Modify: `Cargo.toml`（删 3 个 `[[bin]]`，L114-127）、`.github/workflows/ci.yml`（删 `sam-e2e`/`http-e2e` job）、`src/aws_wiring.rs:3`、`src/app.rs:341`、`sam/local.Dockerfile`（注释改写）

- [ ] **Step 1: `git rm` 上述文件**
- [ ] **Step 2: `Cargo.toml` 删 `[[bin]] query_server/write_server/index_builder_server` 三块**；`server` feature 与其余 bin/example 不动。
- [ ] **Step 3: `ci.yml` 删 `sam-e2e`、`http-e2e` 两 job**
- [ ] **Step 4: 注释改写**：`src/aws_wiring.rs:3`（"extracted from the retired index_builder_server bin; reused by index_builder_lambda"）、`src/app.rs:341`（改引 `src/aws_wiring.rs`）、`sam/local.Dockerfile` 头注释（"#113 起即发布镜像"，删与 `*_server.Dockerfile` 的对比句）。
- [ ] **Step 5: 悬挂引用扫描**（期望仅剩 `docs/architecture-review-2026-07-05.md` 快照与 `.gitignore` 无害条目）：

```bash
grep -rn -e 'docker-compose.http' -e 'query_server' -e 'write_server' -e 'index_builder_server' \
  -e '_server.Dockerfile' -e '_lambda.Dockerfile' -e 'template.sam-e2e' -e 'run-http-server-flow' \
  -e 'run-sam-local-invoke' -e 'run-http-flow' -e 'start-sam-moto' -e 'publish-images' \
  . --exclude-dir=target --exclude-dir=.git --exclude-dir=vendor --exclude-dir=dist \
  --exclude-dir=__pycache__ --exclude-dir=superpowers
```

- [ ] **Step 6: 验证**：`cargo build --no-default-features --features aws`；`bash scripts/verify-fast.sh`；墓碑测试绿、`test_ci_workflow.py` 仍红（release-assembly 未加，预期）。
- [ ] **Step 7: Commit** `refactor(release)!: 删除 server 镜像栈与 image-based Lambda 残留（#113）`

### Task 3: `scripts/package-release.sh`

**Files:**
- Create: `scripts/package-release.sh`（`chmod +x`）

**Interfaces:**
- Consumes: `scripts/package-lambda-zips.sh`（env `LTSEARCH_LTEMBED_MODE`）、`scripts/package-model-assets.sh`、`scripts/check-lambda-size-budget.sh`、`sam/builder.Dockerfile` ARG `LTEMBED_BUNDLE_URL`/`LTEMBED_BUNDLE_SHA256`（sed 提取，单一来源）。
- Produces: `dist/release/` 六件套。provenance schema：`{schema_version:1, tag, git_sha, built_at, workflow:{repository,run_id,run_url}|null, ltembed_mode, ltembed_bundle:{url,sha256}, local_image:{ref,platform:"linux/arm64",dockerfile:"sam/local.Dockerfile"}, artifacts:[{name,bytes,sha256}]}`；`SHA256SUMS` 为 `sha256sum -c` 兼容格式（`<hex>  <name>` 两空格），覆盖 4 zip + provenance。

- [ ] **Step 1: 实现脚本**：`--mode real|stub`（默认 real）、`--version <tag>`（默认 `git describe --tags --exact-match` 否则 `dev-<short-sha>`）；顺序调三脚本 → 收拢 zips → `(cd dist && zip -q -r -X release/model-assets.zip model-assets)` → python3 heredoc 生成 provenance + SHA256SUMS（哈希在 python 内做，macOS 无 sha256sum）。workflow 字段从 `GITHUB_REPOSITORY`/`GITHUB_RUN_ID`/`GITHUB_SERVER_URL` 读取，缺省 null。镜像 digest 有意不记（push 前未知）。
- [ ] **Step 2: 本地 dry-run**：`bash scripts/package-release.sh --mode stub --version v0.0.0-dev`；`(cd dist/release && shasum -a 256 -c SHA256SUMS)`；python 打开 provenance 抽查字段。
- [ ] **Step 3: 验证守卫**：`python3 -B tests/test_release_workflow.py` 中 package-release 测试转绿。
- [ ] **Step 4: Commit** `feat(release): package-release.sh 组装全套 release 产物 + checksums/provenance（#113）`

### Task 4: CI `release-assembly` job（AC-c）

**Files:**
- Modify: `.github/workflows/ci.yml`（新 job + fast 步骤）

- [ ] **Step 1: 新增 job**（`needs: integration`、arm64、timeout 120、setup-python）：跑 `python3 -B tests/test_release_workflow.py` → `bash scripts/package-release.sh --mode stub --version v0.0.0-ci`（注：stub 模式仍下载 471MB pinned bundle，与 sam-ltembed-e2e 同成本）→ `cd dist/release && sha256sum -c SHA256SUMS` → python heredoc 断言 provenance（`schema_version==1`、`ltembed_mode=="stub"`、artifacts 名集合、bundle sha256 长度 64）。**全文避开 "deploy" 子串**。
- [ ] **Step 2: `fast` job 加 `run: python3 -B tests/test_release_workflow.py`**
- [ ] **Step 3: 验证**：`python3 -B tests/test_ci_workflow.py` 绿；`python3 -B tests/test_release_workflow.py` 的 ci 测试绿。
- [ ] **Step 4: Commit** `ci: release-assembly job 校验 release 组装路径（不发布）（#113）`

### Task 5: `.github/workflows/release.yml`（AC-a/b）

**Files:**
- Create: `.github/workflows/release.yml`

- [ ] **Step 1: 实现**（单 job `release`，`on: push: tags: ["v*"]`，`permissions: {contents: write, packages: write}`，`ubuntu-24.04-arm`，timeout 120，`FORCE_JAVASCRIPT_ACTIONS_TO_NODE24: true`）：
  1. checkout@v6 → LTEmbed 锁定 rev 暂存（从原 publish-images.yml L21-25 逐字复用）。
  2. `bash scripts/package-release.sh --mode real --version "$GITHUB_REF_NAME"`。
  3. `docker build --platform linux/arm64 -f sam/local.Dockerfile -t "ghcr.io/lychee-technology/ltsearch-local:$GITHUB_REF_NAME" .` + `docker inspect --format '{{.Architecture}}'` arm64 断言。
  4. GHCR login（`secrets.GITHUB_TOKEN` 裸 `docker login --password-stdin`）→ push 版本 tag；`latest` 仅当 `[[ "$GITHUB_REF_NAME" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]`。
  5. `gh release create "$GITHUB_REF_NAME" dist/release/* --title "$GITHUB_REF_NAME" --generate-notes --verify-tag`，连字符 tag 加 `--prerelease`（`GH_TOKEN: ${{ github.token }}`）。
- [ ] **Step 2: 验证**：`python3 -B tests/test_release_workflow.py` 全绿；yaml parse。
- [ ] **Step 3: Commit** `feat(release): tag 触发 release workflow——GitHub Release + GHCR local 镜像（#113）`

### Task 6: 文档校准（AC-e，含 #103 尾巴）

**Files:**
- Modify: `README.md`、`docs/deployment.md`（整篇重写）、`docs/arch.md` §21/§22、`docs/design.md` ~L1455、`CONTEXT.md`、`docs/adr/0001-aws-optional-runtime-profiles.md`（addendum）

- [ ] **Step 1: README**：L3 tagline 改「one AWS-free local image + Lambda ZIPs」；删 "## HTTP Server Mode" → 新 "## Local Single-Image Mode"（一镜像五子命令、`docker-compose.local.yml`、SQLite 卷 `ltsearch-local-data`@`/var/lib/ltsearch`、restart 语义）；"## Local E2E Workflow" 删 SAM invoke/start-api 小节；新 "## Releases"（tag → 6 资产 + GHCR 镜像、`sha256sum -c`、dry-run 命令）；Build Profiles 删 `*_server` 行与过时 #108 句。保留 `test_readme_workflow.py` 钉住的字符串。
- [ ] **Step 2: docs/deployment.md 重写**为 "Deployment: Local Single Image + Lambda ZIPs"：①release 产物构成；②本地部署（GHCR 镜像、compose、SQLite 卷、env、子命令表）；③AWS ZIP runbook（template.yaml、解包 zip 到 dist/*、model-assets.zip → `aws s3 cp --recursive`、HTTP API 三路由、SQS+redrive、env 表、保留 S3→/tmp 依据段）；④static release 激活 runbook（本地 `static-build`→`static-activate`；AWS `static_activate` bin：env `LTSEARCH_STATIC_S3_BUCKET`、IAM（前缀 Put/GetObject + `static/_head` conditional write）、lost-CAS、回滚=重激活旧 id）；⑤发布验证（SHA256SUMS + provenance 字段 + tag 纪律：只 tag main 上已绿 commit）。
- [ ] **Step 3: docs/arch.md**：§22 重写「Local single image + Lambda ZIP」（保留 arm64 可移植性注意）；§21 删 image 路线块/表列，注明 ZIP S3→/tmp 唯一谱系 + release 交付 model-assets.zip。
- [ ] **Step 4: docs/design.md** Compute 条目改 ZIP 现实；grep `Fargate|PackageType` 清扫。
- [ ] **Step 5: CONTEXT.md** deployables 段补 release 产物、注明 server bin 已删（feature 仍在）。
- [ ] **Step 6: docs/adr/0001 addendum**（日期 2026-07-19，#113：server bin/镜像移除，profile 图不变）。
- [ ] **Step 7: 验证**：`python3 -B tests/test_readme_workflow.py`；过时 token grep。
- [ ] **Step 8: Commit** `docs: 校准部署文档到 local 单镜像 + Lambda ZIP 形态（#113，含 #103 尾巴）`

### Task 7: 端到端验证

- [ ] `bash scripts/verify-fast.sh`
- [ ] 全部 `python3 -B tests/test_*.py`
- [ ] `sam validate --lint --template-file template.yaml`
- [ ] 本地 Compose 全链路：`docker build --platform linux/arm64 -f sam/local.Dockerfile -t ltsearch-local:dev .` → `docker compose -f docker-compose.local.yml up -d --wait` → `bash scripts/e2e/run-local-image-flow.sh` → `docker compose -f docker-compose.local.yml down -v`
- [ ] 确认 zip e2e 覆盖 `/delete` 路由（原 sam-e2e 覆盖物）；缺则小幅扩展 `run-sam-zip-invoke-e2e.sh`
- [ ] Commit（如有 fixup）

### Task 8: PR 与收尾

- [ ] PR `Closes #113`：说明 AC "Layer ZIP"→`model-assets.zip`（#111 改道）、start-api 三件套派生删除；合并后 checklist：先打 `v0.1.0-rc1` 冒烟 release workflow（GHCR 首推权限/6 资产/`docker pull`）再打稳定 tag；GHCR 三个 `ltsearch-*-server` package 标 deprecated；清 worktree/分支、ff main、关 #113、勾 epic #132/#106；按 #106 四 AC 核对关 epic。

## 关键风险

GHCR 首推 403（org 限制）→ rc tag 冒烟；ci.yml 不跑 tag → tag 纪律写进 deployment.md；"deploy" 墓碑（含中文注释）；real 构建成本与 sam-ltembed-e2e 同量级（120min timeout 对齐）。
