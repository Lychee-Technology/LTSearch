# Deployment: Local Single Image + Lambda ZIPs

> **Status: 实际交付形态（#106/#113）。** 本地部署 = 一个 AWS-free 单镜像
> （`ghcr.io/lychee-technology/ltsearch-local`，SQLite 持久化）；AWS 部署 = 三个
> Lambda ZIP（`provided.al2023` / arm64，HTTP API + SQS 触发）+ S3→/tmp 模型资产。
> 早先的「统一镜像跑 Fargate + Lambda（Web Adapter）」路线已被本形态取代
> （#103 superseded；组件 server 镜像与 image-based Lambda 已于 #113 移除）。
> See `docs/arch.md` §22 for the architectural summary.

## 1. Release 产物构成

每个 `v*` tag 由 `.github/workflows/release.yml` 产出（组装逻辑全部在
`scripts/package-release.sh`，本地可用 `--mode stub` dry-run）：

| 产物 | 说明 |
| --- | --- |
| `ghcr.io/lychee-technology/ltsearch-local:<tag>` | 恰一个 local OCI 镜像（arm64；`latest` 仅稳定语义版本），`sam/local.Dockerfile` 原样构建 |
| `query_lambda.zip` / `write_lambda.zip` / `index_builder_lambda.zip` | GitHub Release 资产；`bootstrap` 置 zip 根，real 模式编译 + strip（`scripts/package-lambda-zips.sh`） |
| `model-assets.zip` | GitHub Release 资产；解压得 `model-assets/`（`manifest.json` + model.ort 等，`scripts/package-model-assets.sh` 产出） |
| `SHA256SUMS` | `sha256sum -c` 兼容，覆盖 4 个 zip + provenance |
| `release-provenance.json` | schema_version=1：tag、git sha、workflow run、LTEmbed bundle pin（URL+sha256）、镜像 ref、逐产物 sha256/bytes |

下载后验证：`sha256sum -c SHA256SUMS`。发布前 `scripts/check-lambda-size-budget.sh`
强制单函数解压 ≤250MB、bootstrap AArch64、资产 hash/预算复核。

**可复现性边界**：构建输入全部钉死——base 镜像按 digest pin + dnf releasever 锁
（`sam/builder.Dockerfile` / `sam/local.Dockerfile`）、Rust toolchain 1.94.0、
`Cargo.lock` + vendored stub、LTEmbed rev 随 lockfile、ort bundle URL+sha256 pin；
归档 mtime 与 provenance `built_at` 统一取 `SOURCE_DATE_EPOCH`（默认 HEAD 提交
时间，TZ=UTC 打包）。同一 commit 重复运行 `package-release.sh`，4 个 zip 与本地
provenance 字节级一致；CI 版 provenance 含 workflow run 字段（run 各异，属预期
元数据）。OCI 镜像 rebuild 走同一 pinned 输入，但镜像层哈希含构建时间戳，digest
不承诺逐字节复现——以 GHCR registry 的已发布 digest 为权威。

**Tag 纪律**：CI 不在 tag 上运行——只对 main 上已绿的 commit 打 tag。CI 的
`release-assembly` job 在每个 PR 上以 stub 模式校验同一条组装路径（不发布）。

## 2. 本地部署（AWS-free 单镜像 + SQLite 卷）

一个镜像五个角色，子命令选择：`write` / `build` / `query` / `static-build` /
`static-activate`（`ENTRYPOINT /app/ltsearch`，无 CMD，角色由编排方注入）。
无 AWS SDK、无 Moto、无 Lambda 运行时（feature `local`，CI feature-matrix 强制）。

```bash
# 消费发布镜像（推荐）：pull 后经 LTSEARCH_LOCAL_IMAGE 注入 Compose
docker pull ghcr.io/lychee-technology/ltsearch-local:<tag>
LTSEARCH_LOCAL_IMAGE=ghcr.io/lychee-technology/ltsearch-local:<tag> \
  docker compose -f docker-compose.local.yml up -d --wait
# write :19081 /write /delete，query :19080 /query（127.0.0.1 绑定）
```

不设 `LTSEARCH_LOCAL_IMAGE` 时 Compose 用本地构建的 `ltsearch-local:dev`
（`docker build --platform linux/arm64 -f sam/local.Dockerfile -t ltsearch-local:dev .`，
开发/CI 路径）。注意 `down` 等后续 compose 命令需带同一环境变量。

`docker-compose.local.yml` 拓扑：`write` / `build` / `query` 三服务共用同一镜像，
共享命名卷 `ltsearch-local-data` 挂载在 `/var/lib/ltsearch`
（`LTSEARCH_LOCAL_ROOT`）。卷内是唯一的持久状态：

- `ltsearch.db` — SQLite 控制面（durable events、build 队列、动态 `_head` 与静态
  `static_release_head` 指针；写路径一律 `BEGIN IMMEDIATE`）；
- 不可变制品 — lance/、index 版本目录、`static/releases/<id>/`。

重启语义：`docker compose down`（不带 `-v`）保留卷，重启后 write→build→query 全
链路与已激活版本原样恢复（`scripts/e2e/run-local-image-flow.sh` 持续断言）；
`down -v` 等于清空实例。备份 = 备份该卷。

镜像不内置 embedding 模型：`ltembed` 模式把 LTEmbed bundle 挂载进容器并以
`LTSEARCH_{QUERY,BUILD}_LTEMBED_BUNDLE_DIR` / `..._LTEMBED_MODEL_PATH` 指向挂载
路径；缺省 `fixed` provider 无模型依赖。

### real-LTEmbed E2E 拓扑（测试专用，#141）

`sam/local-ltembed.Dockerfile` + `docker-compose.local-ltembed.yml` 提供真实模型
的黑盒 E2E 拓扑，**不是发布物**（发布镜像仍是 `sam/local.Dockerfile` 的 fixed
拓扑）。与发布拓扑的差异：

- 镜像以 `--features local,ltembed` 编译，并把锁定校验的 linux/arm64 ort bundle
  烘焙进 `/opt/ltembed`（pin 单一来源在 `sam/builder.Dockerfile`，构建脚本提取
  注入，不允许第二处硬编码）；
- Compose 卷/网络不写死 name、host 端口为 loopback 临时端口，runner 以
  `-p ltsearch-real-<run_id>` 注入独立 project——并发/重复运行互不冲突；
- build 角色也发布端口：query/build 的 `/health` 内跑真实 embedding probe，
  `up -d --wait` 变绿即真实推理可用。

```bash
bash scripts/e2e/build-local-ltembed-image.sh   # 物化 LTEmbed checkout + 注入 pin + arm64 构建
bash scripts/e2e/run-local-real-flow.sh         # health → write → 自动 build → query（纯 HTTP 断言）
```

清理语义：无论成败 runner 都 `down -v --remove-orphans`；失败时先把
`compose ps`、各服务日志与全部请求/响应载荷落盘
`.e2e-tmp/ltsearch-real-<run_id>/` 并保留，成功时连该目录一并删除。公共接口在
`scripts/e2e/local_http_lib.sh`（#142/#143 契约套件复用）。每日 CI 回归归 #144。

## 3. AWS 部署（Lambda ZIP + HTTP API + SQS）

生产模板 `template.yaml`（`sam validate --lint` 于 CI 持续校验）：
`provided.al2023` / arm64 / `Handler: bootstrap`，三函数 `CodeUri` 指向
`dist/{write,query,index_builder}_lambda/`。

### 部署步骤

```bash
# 1) 取 release 资产并解包到模板期望的 CodeUri 布局
unzip query_lambda.zip -d dist/query_lambda
unzip write_lambda.zip -d dist/write_lambda
unzip index_builder_lambda.zip -d dist/index_builder_lambda

# 2) 上传模型资产（EmbeddingProvider=ltembed 生产档需要；fixed 冒烟可跳过）
unzip model-assets.zip   # 得 model-assets/
aws s3 cp --recursive model-assets "s3://<ArtifactBucket>/<ModelAssetPrefix>/"

# 3) 部署
sam deploy --template-file template.yaml --stack-name ltsearch \
  --resolve-s3 --capabilities CAPABILITY_IAM
```

### 拓扑与触发

| 资源 | 说明 |
| --- | --- |
| HTTP API | `POST /write`、`POST /delete`（WriteFunction）、`POST /query`（QueryFunction，MemorySize 3008）；base URL 见 stack output `ApiUrl` |
| SQS `BuildQueue` | write 入队 → BuildFunction（BatchSize 1、`ReportBatchItemFailures` partial-batch、Timeout 900）；`VisibilityTimeout: 5400`（≥6× 函数 timeout） |
| `BuildDeadLetterQueue` | redrive `maxReceiveCount: 3`，保留 14 天 |
| `ArtifactBucket` | WAL、索引制品、动态 `_head`、静态 `static/releases/` + `static/_head`、模型资产前缀 |

### 模型资产（S3→/tmp 冷启动，#111）

模型资产不打进函数 ZIP：函数二进制（lance/datafusion 依赖，real 解压 235.7 MiB、
strip 后 180.5 MiB）已逼近 Lambda 250MB 单包硬限，「函数 + Layer 合计 ≤250MB」
装不下两者，故 ZIP 路径采用 S3→/tmp 供给。query/build 以
`LTSEARCH_{QUERY,BUILD}_LTEMBED_S3_BUCKET/_S3_PREFIX` 定位资产（模板默认指向
`ArtifactBucket`/`ModelAssetPrefix`，默认 `ltembed/v1.0.9`，与
`sam/builder.Dockerfile` 的 bundle pin 同步 bump），冷启动按 `manifest.json`
逐文件下载 + sha256 校验到 `/tmp/ltembed`（`/tmp` 不占 250MB 预算，默认 512MB
ephemeral 足够；manifest 最后落盘作完整性标记，warm 容器免重复下载）。
**write 函数零模型 env、零下载代码，可独立部署。** 资产未就位时 query/build 启动
报错直接指出 `model assets not provisioned` 与 S3 配置排查点。

`libonnxruntime.so` 从 bundle 目录解析（`ort` 走 load-dynamic），二进制与 `.so`
仅在同 CPU 架构内可移植——pinned bundle 为 arm64，函数必须跑 **arm64
(Graviton)**；x86_64 需要 x86_64 bundle 与匹配的 `LTEMBED_BUNDLE_URL`。

### 运行时环境变量

| 组件 | 关键变量 |
| --- | --- |
| query | `LTSEARCH_QUERY_S3_BUCKET`、`LTSEARCH_QUERY_ARTIFACT_ROOT`、`LTSEARCH_QUERY_EMBEDDING_PROVIDER`、`LTSEARCH_QUERY_LTEMBED_*`（静态检索经激活指针 `static/_head` → `static/releases/<id>/` 解析，无 `LTSEARCH_QUERY_STATIC_DIR`） |
| write | `LTSEARCH_WRITE_S3_BUCKET`、`LTSEARCH_WRITE_SQS_QUEUE_URL` |
| index_builder | `LTSEARCH_BUILD_S3_BUCKET`、`LTSEARCH_BUILD_ARTIFACT_ROOT`、`LTSEARCH_BUILD_EMBEDDING_PROVIDER`、`LTSEARCH_BUILD_EMBEDDING_DIM`、`LTSEARCH_BUILD_LTEMBED_*` |

## 4. Static release 激活 runbook（#110/#112）

静态 TurboQuant v3 release 与动态索引版本独立发布/激活；查询响应同时报告
`(dynamic_version, static_release_id)` 对。build 与 activate 严格分离。

### 本地（单镜像 / 原生进程）

```bash
# 构建不可变 release（release_id 内容导出，逐字节确定性）
ltsearch static-build --config <config.json> --output <release_dir>

# 校验（manifest/输出 hash/Lance provenance/embedding profile）→ 安装到
# <root>/static/releases/<id>/ → CAS 翻 SQLite static_release_head 指针
ltsearch static-activate --release <release_dir> --root <LTSEARCH_LOCAL_ROOT> \
  [--expect-model-id <id>] [--expect-dim <n>]
```

### AWS（`static_activate` bin，`--features aws`）

```bash
LTSEARCH_QUERY_S3_BUCKET=<query 栈读取的 ArtifactBucket> \
  static_activate --release <release_dir> [--expect-model-id <id>] [--expect-dim <n>]
```

- **目标桶 = query 栈读取的桶**（`LTSEARCH_QUERY_S3_BUCKET` 是唯一来源；旧
  `LTSEARCH_STATIC_S3_BUCKET` 已弃用，若设且不同值会硬报错防旧 runbook 发错桶）。
- 三步严格有序：本地 verify → 逐对象 CreateOnly 上传 `static/releases/<id>/`
  （`.bin` 先、`release_manifest.json` 最后作完整性标记）→ CAS 翻 `static/_head`。
  幂等可续传：中断重跑回填缺失对象；已存在对象回读校验 sha256/size 后才跳过。
- **IAM 最小集**：对 `arn:aws:s3:::<bucket>/static/*` 的 `s3:PutObject`（CreateOnly
  依赖 `If-None-Match` 条件写）+ `s3:GetObject`（回读校验与指针 CAS 读侧）。
- **lost-CAS 语义**：并发激活恰一 winner，输者得明确报错（与 SQLite 侧等价）。
- **回滚 = 重激活旧 release**：release 不可变、按 id 寻址，对旧 `<release_dir>`
  重跑 activate 即把指针翻回（对象已在位，只走校验 + CAS）。

## 5. Cargo build profiles（AWS 可选）

AWS SDK 与 Lambda 运行时是可选 cargo feature（ADR-0001）。默认 `local` profile
AWS-free（bare `cargo build` 不拉任何 AWS SDK；CI feature-matrix 持续断言）：

- 统一本地二进制 `ltsearch`（五子命令）：`--features local`（默认）。
- Lambda handlers（`query_lambda` / `write_lambda` / `index_builder_lambda`）：
  `--features lambda`（隐含 `aws`）。
- 离线/运维二进制（`turbo_index_builder` / `static_activate`）：`--features aws`。
