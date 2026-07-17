# Issue #111 — 共享 LTEmbed Lambda Layer 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
> 执行开工时先把本计划拷贝为 `docs/superpowers/plans/2026-07-17-issue-111-ltembed-layer.md`(仓库惯例位置)。

**Goal:** 把 pinned 的 arm64 LTEmbed 模型 + ONNX Runtime 资产打成 version-pinned Lambda Layer(解到 `/opt/ltembed`),由 query 与 index-builder ZIP 函数共享;write 函数不带模型资产;构建自动化记录 hash 并断言压缩/解压/架构/250MB 运行时预算;启动失败清晰指出 Layer 缺失或不兼容。单 PR,`Closes #111`。

**Architecture:** 资产路径在 Rust 侧已完全 env 可配(`ltembed_config_from_env`),`/opt/ltembed` 只是 env 值,无需改路径解析逻辑。Layer 打包复用 `sam/builder.Dockerfile` 的下载/pin(抽独立 `bundle` stage 并补 sha256),新脚本产出 `dist/ltembed_layer.zip` + hash manifest;尺寸预算脚本既是 spike 决策门也是常驻 CI 守卫;生产 `template.yaml` 加 `LayerVersion` 并把默认 provider 翻回 ltembed/512(#94 裁决,模板注释早已承诺);stub zip e2e 经派生模板剥 Layer 保持绿。

**Tech Stack:** bash 打包脚本 + 内嵌 python3(zipfile/hashlib/ELF 头解析)、Docker(AL2023 bundle/builder stage,`--platform linux/arm64`)、AWS SAM(`AWS::Serverless::LayerVersion`、`sam local invoke` 本地 Layer 支持)、python unittest 结构守卫、Rust(错误信息预检,`--features lambda,ltembed`)。

## Context(为什么做)

- Epic #106 要求 AWS 部署走 ZIP 产物;#109(PR #134/#135 已合并)交付了 3 个函数 ZIP + 生产模板,但模型资产仍缺——模板默认 fixed/3 维冒烟档,注释明确「ltembed env + Lambda Layer 由 #111 交付,届时默认翻回 ltembed」。
- `docs/deployment.md:13-16` 与 `docs/arch.md:665-669` 断言资产「超 Lambda Layer 限制,必须容器镜像」——**该断言很可能错误**:Lambda 硬限是「函数 + 全部 Layer 解压合计 ≤ 250MB」,资产解压 ~140MB(model.ort ~118 + tokenizer.json ~16 + libonnxruntime.so ~4.6),tarball 压缩实测 119,571,616 bytes(~114MiB,本次规划 HEAD 实测),函数 ZIP 解压余量 ~110MB,Rust release 二进制预计 30–60MB。
- Epic 执行方案(`docs/superpowers/plans/2026-07-16-epic-106-execution.md` Task B1)裁决:第一步是**尺寸 spike 决策门**,实测合计 ≤250MB → Layer 路线;超限 → S3→/tmp fallback 并连锁调整 #113。spike 结论回报 issue #111。
- 前置 #109 已全部合并(issue 评论确认),#111 可开工。

## Global Constraints

- 所有 Lambda 产物 Linux arm64;编译只在 AL2023 容器内(glibc 2.34);**不引入 cargo-lambda**。
- `local` profile 保持 AWS-free 依赖图(feature-matrix job 强制)。
- e2e 用 `scripts/e2e/*.sh` + python 结构守卫模式;新脚本必须有对应 `tests/test_*.py` 守卫。
- 开工前:清理已合并分支 → `git pull --ff-only` 更新 main → `git worktree add ../LTSearch-issue-111 -b feat/111-ltembed-layer`。
- PR 正文 `Closes #111`;**不自动合并**;合并后清 worktree/分支、ff main、核对 AC 关 issue。
- bundle pin 单一来源:URL + sha256 只在 `sam/builder.Dockerfile` ARG 出现;其他脚本经 build-arg 传递/覆盖。

## 决策门语义(spike)

Task 1+2 完成后立即在本机跑 real 模式打包 + 预算脚本,拿到实测数字:

- **预算脚本通过(预期)** → 把实测表(层压缩/解压、各函数解压、合计、余量)评论回报 issue #111,继续 Task 3–7。
- **超限(意外)** → 停止后续任务,评论回报 issue #111 并等待改道裁决(S3→/tmp fallback,连锁调整 #111 AC 与 #113 交付清单)。已完成的 bundle stage + hash 工作在 fallback 路线仍可复用。

---

## Task 1: `sam/builder.Dockerfile` 抽 `bundle` stage + sha256 pin

**Files:**
- Modify: `sam/builder.Dockerfile`(现 59 行,下载块在行 11-24)
- Modify: `sam/query_lambda.Dockerfile:5`、`sam/index_builder_lambda.Dockerfile:5`(`COPY --from=builder /ltembed-assets` → `COPY --from=bundle`)

**Interfaces:**
- Produces: docker build target `bundle`,资产在镜像 `/ltembed-assets/`;ARG `LTEMBED_BUNDLE_URL`(既有 v1.0.9 pin)与新 ARG `LTEMBED_BUNDLE_SHA256`(默认值 = v1.0.9 tarball 实测 sha256)。
- 后续 Task 2 的 layer 脚本用 `--target bundle` 构建,不触发 cargo 编译、不需要 LTEmbed 源 checkout。

**Steps:**

- [ ] **Step 1: 捕获 pin 值** — `curl -sL "https://github.com/Lychee-Technology/minimal-ort-builder/releases/download/v1.0.9/jinaai__jina-embeddings-v5-text-nano-retrieval_q4f16_linux-arm64.tar.gz" | shasum -a 256`(tarball 应为 119,571,616 bytes;顺手 `curl ... -o /tmp/b.tgz && tar -tzvf` 记录逐文件解压尺寸备用)。
- [ ] **Step 2: 改 Dockerfile** — 头部改为多 stage;`builder` stage 用 `COPY --from=bundle` 取资产(stub 时目录为空,COPY 无害):

```dockerfile
FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS bundle
RUN dnf install -y tar gzip && dnf clean all
ARG LTEMBED_MODE=stub
# ort_bundle tarball for jina-embeddings-v5-text-nano-retrieval, with
# model.ort, tokenizer.json, build-info.json, libonnxruntime.so (linux/arm64)
# under a leading ./ (hence --strip-components=1). Bump URL + SHA256 together.
ARG LTEMBED_BUNDLE_URL=https://github.com/Lychee-Technology/minimal-ort-builder/releases/download/v1.0.9/jinaai__jina-embeddings-v5-text-nano-retrieval_q4f16_linux-arm64.tar.gz
ARG LTEMBED_BUNDLE_SHA256=<Step 1 实测值>
RUN mkdir -p /ltembed-assets && \
    if [ "$LTEMBED_MODE" != "stub" ]; then \
      if [ -z "$LTEMBED_BUNDLE_URL" ] || [ -z "$LTEMBED_BUNDLE_SHA256" ]; then \
        echo "LTEMBED_MODE=real requires LTEMBED_BUNDLE_URL and LTEMBED_BUNDLE_SHA256" >&2; exit 1; \
      fi; \
      curl -fSL "$LTEMBED_BUNDLE_URL" -o /tmp/ltembed-bundle.tar.gz && \
      echo "$LTEMBED_BUNDLE_SHA256  /tmp/ltembed-bundle.tar.gz" | sha256sum -c - && \
      tar -xzf /tmp/ltembed-bundle.tar.gz -C /ltembed-assets --strip-components=1 && \
      rm /tmp/ltembed-bundle.tar.gz && \
      test -f /ltembed-assets/model.ort && test -f /ltembed-assets/tokenizer.json && \
      test -f /ltembed-assets/build-info.json && test -f /ltembed-assets/libonnxruntime.so; \
    fi

FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS builder
# ……(原行 2-4 rustup 等不变;原行 5-24 下载块删除,改为:)
ARG LTEMBED_MODE=stub
COPY --from=bundle /ltembed-assets /ltembed-assets
# ……(原行 25-59 WORKDIR/patch/cargo build 不变)
```

  注意 `bundle` stage 基础镜像自带 curl(AL2023 有 curl-minimal,若 `curl -fSL` 不可用则 dnf 加 curl——实现时以构建通过为准)。
- [ ] **Step 3: 验证** — `DOCKER_BUILDKIT=1 docker build --platform linux/arm64 --target bundle --build-arg LTEMBED_MODE=real -f sam/builder.Dockerfile .` 通过(sha256 校验绿);再跑 `LTSEARCH_LTEMBED_MODE=stub bash scripts/package-lambda-zips.sh` 确认 stub 全量编译路径未破。
- [ ] **Step 4: Commit** — `git commit -m "build(sam): split bundle stage with sha256-pinned LTEmbed assets (#111)"`

## Task 2: Layer 打包脚本 + 尺寸预算守卫(spike 工具)

**Files:**
- Create: `scripts/package-model-layer.sh`(可执行)
- Create: `scripts/check-lambda-size-budget.sh`(可执行)
- Create: `tests/test_model_layer_packaging.py`(结构守卫,先写 = red)

**Interfaces:**
- Produces: `dist/ltembed_layer.zip`(zip 根 `ltembed/{model.ort,tokenizer.json,build-info.json,libonnxruntime.so}` → Lambda 解到 `/opt/ltembed/`)+ `dist/ltembed_layer.manifest.json`(字段:`bundle_url`、`bundle_sha256`、`layer_zip_sha256`、`compressed_bytes`、`uncompressed_bytes`、`files[]{name,bytes,sha256}`、`arch: "aarch64"`)。
- `check-lambda-size-budget.sh [dist_dir]`:对 `query_lambda.zip`、`index_builder_lambda.zip` 断言 `unzip(fn)+unzip(layer) ≤ 250*1024*1024`(>240MB 打 warning);断言 layer 内 `libonnxruntime.so` 与各函数 `bootstrap` 的 ELF `e_machine == 0xB7`(AArch64,读 offset 0x12 小端 u16,不依赖 `file` 命令);断言 layer 四文件齐全;打印尺寸表。退出码非 0 即失败。

**Steps:**

- [ ] **Step 1: 写结构守卫测试(red)** — `tests/test_model_layer_packaging.py`,镜像 `tests/test_lambda_zip_packaging.py` 的纯静态断言风格:

```python
class ModelLayerPackagingTest(unittest.TestCase):
    def test_layer_script_builds_bundle_stage_and_stages_opt_layout(self):
        script = REPO_ROOT / "scripts/package-model-layer.sh"
        text = script.read_text()
        self.assertTrue(os.access(script, os.X_OK))
        for needle in ["set -euo pipefail", "sam/builder.Dockerfile", "--platform linux/arm64",
                       "--target bundle", "LTEMBED_MODE=real", "ltembed_layer.zip",
                       "ltembed_layer.manifest.json", "sha256"]:
            self.assertIn(needle, text)
        self.assertNotIn("cargo-lambda", text)

    def test_budget_script_asserts_250mb_and_aarch64(self):
        script = REPO_ROOT / "scripts/check-lambda-size-budget.sh"
        text = script.read_text()
        self.assertTrue(os.access(script, os.X_OK))
        for needle in ["set -euo pipefail", "250", "0xB7", "e_machine", "bootstrap", "libonnxruntime.so"]:
            self.assertIn(needle, text)
```

  运行 `python3 -B tests/test_model_layer_packaging.py` → FAIL(文件不存在)。
- [ ] **Step 2: 写 `scripts/package-model-layer.sh`**:

```bash
#!/usr/bin/env bash
# 打包共享 LTEmbed Lambda Layer（#111）：只构建 builder.Dockerfile 的 bundle
# stage（无 cargo 编译、无 LTEmbed 源依赖），资产 staging 到 zip 根 ltembed/，
# Lambda 挂载后解到 /opt/ltembed/。逐文件 sha256 + 尺寸写入 manifest。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DIST_DIR="${LTSEARCH_DIST_DIR:-$REPO_ROOT/dist}"
readonly BUNDLE_IMAGE="${LTSEARCH_BUNDLE_IMAGE:-ltsearch-model-bundle}"

DOCKER_BUILDKIT=1 docker build \
  --platform linux/arm64 \
  --target bundle \
  --build-arg LTEMBED_MODE=real \
  ${LTEMBED_BUNDLE_URL:+--build-arg LTEMBED_BUNDLE_URL="$LTEMBED_BUNDLE_URL"} \
  ${LTEMBED_BUNDLE_SHA256:+--build-arg LTEMBED_BUNDLE_SHA256="$LTEMBED_BUNDLE_SHA256"} \
  --tag "$BUNDLE_IMAGE" \
  --file "$REPO_ROOT/sam/builder.Dockerfile" \
  "$REPO_ROOT"

container_id="$(docker create --platform linux/arm64 "$BUNDLE_IMAGE")"
trap 'docker rm -f "$container_id" >/dev/null' EXIT

staging="$DIST_DIR/ltembed_layer"
rm -rf "$staging" "$DIST_DIR/ltembed_layer.zip" "$DIST_DIR/ltembed_layer.manifest.json"
mkdir -p "$staging/ltembed"
docker cp "$container_id:/ltembed-assets/." "$staging/ltembed/"
(cd "$staging" && zip -q -X -r "$DIST_DIR/ltembed_layer.zip" ltembed)

python3 - "$DIST_DIR" <<'PY'
import hashlib, json, pathlib, sys
dist = pathlib.Path(sys.argv[1])
zip_path = dist / "ltembed_layer.zip"
files = []
for p in sorted((dist / "ltembed_layer/ltembed").iterdir()):
    files.append({"name": p.name, "bytes": p.stat().st_size,
                  "sha256": hashlib.sha256(p.read_bytes()).hexdigest()})
manifest = {
    "bundle_url": "<从 builder.Dockerfile ARG 同步/或环境覆盖>",
    "layer_zip_sha256": hashlib.sha256(zip_path.read_bytes()).hexdigest(),
    "compressed_bytes": zip_path.stat().st_size,
    "uncompressed_bytes": sum(f["bytes"] for f in files),
    "files": files, "arch": "aarch64",
}
(dist / "ltembed_layer.manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
PY
echo "packaged model layer into $DIST_DIR" >&2
```

  (实现时把 `bundle_url`/`bundle_sha256` 以 env 传入 heredoc,与 Dockerfile ARG 默认值保持同步;grep Dockerfile 提默认值亦可,择简。)
- [ ] **Step 3: 写 `scripts/check-lambda-size-budget.sh`** — bash 包 python3 heredoc:解压尺寸经 `zipfile.ZipFile(...).infolist()` 求和;ELF 检查读 zip 内文件头 18 字节取 `e_machine`(小端 u16 == 0xB7);对 `query_lambda.zip`、`index_builder_lambda.zip` 各算 `fn + layer` 合计,`> 250*1024*1024` 即 exit 1,`> 240MB` 打 warning;末尾打印表格(层压缩/解压、各函数解压、合计、余量)。
- [ ] **Step 4: green** — `python3 -B tests/test_model_layer_packaging.py` PASS。
- [ ] **Step 5: 跑 spike(决策门)** — 需本机有 LTEmbed checkout(`scripts/e2e/lib.sh` 的 `prepare_local_ltembed_checkout` 逻辑,cargo git cache 可供源):

```bash
bash scripts/package-model-layer.sh
LTSEARCH_LTEMBED_MODE=real bash scripts/package-lambda-zips.sh   # 需先 prepare .sam-local-deps/LTEmbed
bash scripts/check-lambda-size-budget.sh
```

  按「决策门语义」评论回报 issue #111(`gh issue comment 111 --repo Lychee-Technology/LTSearch`),通过则继续。
- [ ] **Step 6: Commit** — `git commit -m "feat(layer): package pinned LTEmbed layer with hash manifest and 250MB budget guard (#111)"`

## Task 3: Rust 启动错误清晰指出 Layer 缺失/不兼容

**Files:**
- Modify: `src/embedding/ltembed.rs:48-64`(`from_config`)
- Test: 同文件 `#[cfg(test)]` 单测(缺失路径 → 错误文案断言)

**Interfaces:**
- Consumes: `LTEmbedConfig{bundle_dir, model_path}`(query 侧 env `LTSEARCH_QUERY_LTEMBED_*`,build 侧 `LTSEARCH_BUILD_LTEMBED_*`,调用点无需改)。
- Produces: 双侧共享的预检——路径不存在 → 错误含「model Layer not attached」+ 具体路径;`OnnxEngine::from_bundle_dir` 失败 → 错误含 bundle_dir + arm64/损坏提示。HTTP `/health`(`src/http/query.rs`、`src/http/build.rs`)沿用 probe,自动受益,无需改。

**Steps:**

- [ ] **Step 1: 写失败测试(red)** — 在 `src/embedding/ltembed.rs` 测试模块(feature `ltembed` 下):

```rust
#[test]
fn from_config_reports_missing_layer_paths() {
    let config = LTEmbedConfig {
        bundle_dir: "/opt/ltembed".to_string(),
        model_path: "/opt/ltembed/model.ort".to_string(),
    };
    let error = LTEmbedEmbeddingGenerator::from_config(&config, EmbeddingInputKind::Query)
        .err()
        .expect("missing bundle dir must fail");
    let message = error.to_string();
    assert!(message.contains("/opt/ltembed"), "{message}");
    assert!(message.contains("Layer"), "{message}");
    assert!(message.contains("not attached"), "{message}");
}
```

  `cargo test --no-default-features --features lambda,ltembed --lib embedding::ltembed`(需 `.sam-local-deps` patch 或 stub?——**注意**:stub 引擎无真实 `from_bundle_dir` 行为,该测试只走预检分支,在文件不存在时预检先行返回,stub/real 均可;若 stub 的 `OnnxEngine` 符号不全导致无法编译,则将该测试放入 real-mode CI 覆盖并在本地用 `.sam-local-deps/LTEmbed` 跑)→ 先 FAIL。
- [ ] **Step 2: 实现预检(green)**:

```rust
pub fn from_config(
    config: &LTEmbedConfig,
    input_kind: EmbeddingInputKind,
) -> Result<Self, EmbeddingError> {
    for (label, path) in [
        ("bundle dir", config.bundle_dir.as_str()),
        ("model", config.model_path.as_str()),
    ] {
        if !std::path::Path::new(path).exists() {
            return Err(EmbeddingError::Generation {
                message: format!(
                    "LTEmbed {label} not found at '{path}' — model Layer not attached \
                     or assets missing (expect the Layer to extract under /opt/ltembed; \
                     check the function's Layers configuration)"
                ),
            });
        }
    }
    let engine = OnnxEngine::from_bundle_dir(
        &config.bundle_dir,
        &config.model_path,
        OnnxEngineConfig::default(),
    )
    .map_err(|error| EmbeddingError::Generation {
        message: format!(
            "LTEmbed bootstrap failed for bundle_dir '{}': {error} — \
             verify the model Layer matches linux/arm64 and is not corrupt",
            config.bundle_dir
        ),
    })?;
    Ok(Self { engine, input_kind })
}
```

  维度不匹配(挂错模型)已有既有守卫:`src/query_lambda.rs:168-174` embedder 输出 dim vs manifest `embedding_dim`。build-info.json 的 model_id 交叉校验按 YAGNI 不纳入本 PR(打包侧 sha256 + arch 断言已守住供给面)。
- [ ] **Step 3: 验证** — 上述 cargo test PASS;`cargo build --no-default-features --features lambda`(无 ltembed)不受影响;`cargo clippy --no-default-features --features lambda,ltembed -- -D warnings`。
- [ ] **Step 4: Commit** — `git commit -m "feat(embedding): report missing/incompatible model layer at ltembed bootstrap (#111)"`

## Task 4: `template.yaml` — LayerVersion、挂载、默认翻回 ltembed/512

**Files:**
- Modify: `template.yaml`(Parameters 行 8-28;QueryFunction 行 82-101;BuildFunction 行 103-128;新增 ModelLayer 资源)
- Modify: `scripts/e2e/lib.sh:281-336`(`make_zip_e2e_template` 剥 Layer)
- Modify: `tests/test_lambda_zip_packaging.py`(默认值断言翻转)

**Interfaces:**
- Produces: `ModelLayer`(`AWS::Serverless::LayerVersion`,`ContentUri: dist/ltembed_layer.zip`);Query/Build `Layers: [!Ref ModelLayer]` + env `LTSEARCH_{QUERY,BUILD}_LTEMBED_BUNDLE_DIR=/opt/ltembed`、`..._MODEL_PATH=/opt/ltembed/model.ort`;Write 不挂 Layer、无 LTEMBED env(AC-5)。Parameters:`EmbeddingProvider` Default `ltembed`、`EmbeddingDim` Default `'512'`(#94);`FixedEmbedding` 保留供 stub e2e 覆盖。

**Steps:**

- [ ] **Step 1: 更新结构守卫断言(red)** — `tests/test_lambda_zip_packaging.py::test_production_template_uses_zip_httpapi_and_sqs_redrive`:`assertIn("Default: fixed")` → `assertIn("Default: ltembed")`,补 `Default: '512'`;在 `tests/test_model_layer_packaging.py` 加模板断言:

```python
    def test_template_attaches_layer_to_query_and_build_only(self):
        text = (REPO_ROOT / "template.yaml").read_text()
        for needle in ["AWS::Serverless::LayerVersion", "ContentUri: dist/ltembed_layer.zip",
                       "CompatibleArchitectures", "/opt/ltembed", "/opt/ltembed/model.ort"]:
            self.assertIn(needle, text)
        write_block = text.split("WriteFunction:")[1].split("QueryFunction:")[0]
        self.assertNotIn("Layers", write_block)
        self.assertNotIn("LTEMBED", write_block)
```

  两个测试文件先 FAIL。
- [ ] **Step 2: 改 `template.yaml`(green)**:

```yaml
  ModelLayer:
    Type: AWS::Serverless::LayerVersion
    Properties:
      LayerName: !Sub '${AWS::StackName}-ltembed'
      Description: Pinned LTEmbed model + ONNX Runtime assets (linux/arm64), extracts to /opt/ltembed
      ContentUri: dist/ltembed_layer.zip
      CompatibleArchitectures: [arm64]
      CompatibleRuntimes: [provided.al2023]
      RetentionPolicy: Delete
```

  QueryFunction 加 `Layers: [!Ref ModelLayer]` + 两个 `LTSEARCH_QUERY_LTEMBED_*` env;BuildFunction 同构(`LTSEARCH_BUILD_LTEMBED_*`);Parameters 默认翻转 + Description 更新(去掉「#111 交付时…」占位语)。
- [ ] **Step 3: 剥 Layer 保 stub e2e** — `make_zip_e2e_template` 的 python 块中(生成派生模板处)加:

```python
for logical_id in ("WriteFunction", "QueryFunction", "BuildFunction"):
    template["Resources"][logical_id]["Properties"].pop("Layers", None)
template["Resources"].pop("ModelLayer", None)
```

  (stub e2e 已用 `--env-vars` 覆盖 provider=fixed/dim=3,LTEMBED env 残留无害。)
- [ ] **Step 4: 验证** — `python3 -B tests/test_lambda_zip_packaging.py` + `python3 -B tests/test_model_layer_packaging.py` PASS;`sam validate --lint --template-file template.yaml` PASS;本地跑 `bash scripts/e2e/run-sam-zip-invoke-e2e.sh`(stub 全链路)仍绿。
- [ ] **Step 5: Commit** — `git commit -m "feat(sam): attach shared LTEmbed layer to query/build, default ltembed/512 (#111)"`

## Task 5: real 模式 Layer e2e 脚本 + CI job

**Files:**
- Create: `scripts/e2e/run-sam-layer-invoke-e2e.sh`(可执行;镜像 `run-sam-zip-invoke-e2e.sh` 112 行的结构)
- Modify: `.github/workflows/ci.yml`(加 `sam-layer-e2e` job,镜像 `sam-zip-e2e` 行 90-103 结构)
- Modify: `tests/test_model_layer_packaging.py`(补 e2e 脚本与 CI job 断言)
- 检查 `tests/test_ci_workflow.py` 是否枚举 job 清单,若有则同步。

**Interfaces:**
- Consumes: `prepare_local_ltembed_checkout`(`scripts/e2e/lib.sh:126-171`)、`assert_zip_layout`(lib.sh:263-273)、moto helpers、`make_apigw_event`/`make_sqs_event`。
- Produces: real 模式全链路验证——ltembed 512 维真实嵌入下 write→SQS→build→query 通,预算守卫内嵌。

**Steps:**

- [ ] **Step 1: 守卫断言先行(red)** — `tests/test_model_layer_packaging.py` 加:e2e 脚本存在/可执行/含 `package-model-layer.sh`、`check-lambda-size-budget.sh`、`LTSEARCH_LTEMBED_MODE=real`、`prepare_local_ltembed_checkout`;`ci.yml` 含 `sam-layer-e2e:` 与 `run-sam-layer-invoke-e2e.sh`;`lib.sh` 的 `make_zip_e2e_template` 含 `pop("Layers"`。
- [ ] **Step 2: 写 e2e 脚本** — 流程:`prepare_local_ltembed_checkout` → `LTSEARCH_LTEMBED_MODE=real bash scripts/package-lambda-zips.sh` → `bash scripts/package-model-layer.sh` → `assert_zip_layout` ×3 → `bash scripts/check-lambda-size-budget.sh` → 派生模板(复用 `make_zip_e2e_template` 注入 moto env,但**保留 Layers/ModelLayer**——给 `make_zip_e2e_template` 加第二参数 `keep_layers=1`,或新增 `make_layer_e2e_template` helper,择简且守卫同步)→ `sam local invoke` write→SQS→build→query,断言 build 返回 `{'batchItemFailures': []}`、query 200 且返回真实语义命中(不再覆盖 provider,走模板默认 ltembed/512)。注意派生模板 ContentUri 需改绝对路径(与 CodeUri 同法)。
- [ ] **Step 3: 加 CI job**:

```yaml
  sam-layer-e2e:
    needs: integration
    runs-on: ubuntu-24.04-arm
    timeout-minutes: 120
    steps:
      - uses: actions/checkout@v6
      - uses: actions/setup-python@v6
        with:
          python-version: '3.x'
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          cache: true
      - run: python3 -B tests/test_model_layer_packaging.py
      - run: python3 -m pip install --upgrade pip awscli aws-sam-cli
      - run: sam validate --lint --template-file template.yaml
      - run: docker compose -f docker-compose.moto.yml up -d
      - run: bash scripts/e2e/run-sam-layer-invoke-e2e.sh
      - if: always()
        run: docker compose -f docker-compose.moto.yml down -v
```

  (需 rust toolchain 供 `cargo fetch` 取 LTEmbed git checkout,与 `sam-e2e` job 同法。)
- [ ] **Step 4: 验证** — 守卫测试 PASS;本地跑 `run-sam-layer-invoke-e2e.sh` 全绿(重:下载 114MB bundle + real 编译;CI 上为准,与 #109 惯例一致)。
- [ ] **Step 5: Commit** — `git commit -m "test(e2e): real-mode layer invoke e2e + sam-layer-e2e CI job (#111)"`

## Task 6: 文档修正

**Files:**
- Modify: `docs/deployment.md`(行 13-16「exceeds the Lambda Layer limit → container image required」;行 ~106-119 bundle/ORT 段补 /opt/ltembed 与 Layer 部署说明)
- Modify: `docs/arch.md` §21(行 603-632)与 §22(行 665-669)

**Steps:**

- [ ] **Step 1: 改写要点** — 删除/更正「超 Layer 限」断言,替换为实测事实:bundle 压缩 ~114MiB、解压 ~140MB,与函数解压合计落在 250MB 硬限内(引用 `dist/ltembed_layer.manifest.json` 与 `check-lambda-size-budget.sh` 的 CI 守卫);ZIP 路径资产经共享 Layer 交付,文档化 `/opt/ltembed` 路径表(镜像路径 `/ltembed-assets` 与 Layer 路径 `/opt/ltembed` 双列);pin(URL+sha256)单一来源在 `sam/builder.Dockerfile` `bundle` stage;write 函数无 Layer 可独立部署;容器镜像 lineage 保留为兼容形态并更正其理由叙述。
- [ ] **Step 2: 验证** — `grep -rn "exceed.*[Ll]ayer" docs/` 无残留错误断言;若 `tests/test_readme_workflow.py` 等守卫断言 docs 内容,跑一遍全量 python 守卫:`for t in tests/test_*.py; do python3 -B "$t"; done`。
- [ ] **Step 3: Commit** — `git commit -m "docs: correct layer-limit claim; document /opt/ltembed layer delivery (#111)"`

## Task 7: PR 与收尾

- [ ] 全量本地验证:`cargo test --no-default-features --features lambda`、`cargo test`(local 默认档)、全部 python 守卫、`sam validate`、stub zip e2e。
- [ ] 发 PR:标题 `feat(sam): 为 Lambda ZIP 交付共享 LTEmbed Layer`,正文含 AC 逐条映射 + spike 实测表 + `Closes #111`;**不自动合并**。
- [ ] 合并后:清 worktree/分支、`git pull --ff-only` main;#111 由 PR 自动关闭,补 AC 核对评论;epic #106 侧 Wave B 仅剩 #112(并行),之后 #113。

## AC 映射

| AC | 落点 |
|---|---|
| 1. `/opt` 文档化路径 + query/builder 配置使用 | Task 4(env=/opt/ltembed)+ Task 6(文档) |
| 2. Layer 与两函数 ZIP 同为 linux arm64 | Task 1(`--platform linux/arm64`)+ Task 2(ELF e_machine 断言) |
| 3. hash 记录 + 压缩/解压/架构/运行时预算检查 | Task 1(sha256 pin)+ Task 2(manifest + budget 脚本)+ Task 5(入 CI) |
| 4. 启动失败清晰指出 layer 缺失/不兼容 | Task 3(from_config 预检 + 错误包装) |
| 5. write 无 Layer 可独立部署 | Task 4(Write 不挂 Layer/无 LTEMBED env,守卫断言 NotIn) |

## Verification(端到端)

1. `bash scripts/package-model-layer.sh && LTSEARCH_LTEMBED_MODE=real bash scripts/package-lambda-zips.sh && bash scripts/check-lambda-size-budget.sh` → 打印尺寸表,合计 ≤250MB。
2. `bash scripts/e2e/run-sam-layer-invoke-e2e.sh` → real 模式 write→SQS→build→query 全链路绿(512 维真实嵌入)。
3. `bash scripts/e2e/run-sam-zip-invoke-e2e.sh` → stub 路径回归仍绿。
4. CI:`sam-layer-e2e`、`sam-zip-e2e`、`feature-matrix`、`local-e2e` 全绿。

## 风险

- **spike 意外超限**(函数二进制 >110MB,几率低):按决策门停在 Task 2 Step 5,回报 issue 改道 S3→/tmp,Task 1/2 的 pin+hash 工作可复用。
- **`sam local invoke` 的 Layer 本地化行为**:SAM 对本地 zip ContentUri 的 LayerVersion 支持解压挂 `/opt`,但派生模板需把 ContentUri 转绝对路径(与既有 CodeUri 同法);若遇 SAM 版本差异,e2e 里退回「把层内容直接铺进函数镜像目录」不可取——应固定 aws-sam-cli 版本排查。
- **stub 模式下 Task 3 单测的可编译性**:预检分支不依赖真实引擎,但 `ltembed` feature 编译需 patch;本地跑 real 单测需 `.sam-local-deps/LTEmbed`(lib.sh helper 可备),CI 已在 `sam-layer-e2e` 内覆盖。
