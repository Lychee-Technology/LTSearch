# Deployment: One Docker Image, Two Runtimes (Fargate + Lambda)

> **Status: HTTP server mode 已实现（本地 / Compose）；Fargate + Lambda 双运行时部署仍为规划。**
> 三个组件的 HTTP 服务二进制（`src/bin/{query,write,index_builder}_server.rs`）、各自的
> `sam/*_server.Dockerfile`、GHCR 镜像发布（`.github/workflows/publish-images.yml`）与本地
> Compose 全链路冒烟（`docker-compose.http.yml` + `scripts/e2e/run-http-server-flow.sh`）均已落地，
> index-builder 已接入 SQS→build 自动触发与版本分配。本文档描述的**统一镜像同时不变地跑在 AWS
> Lambda 与 AWS Fargate** 这一目标拓扑中，ECS 任务定义 / Lambda SAM 资源等基础设施模板仍是后续工作，
> 尚未落地。See `docs/arch.md` §22 for the architectural summary.

## Why Docker, and why both runtimes

- **Model size.** The embedding assets are ~139 MiB unzipped
  (`model.ort` ~118 MiB + `tokenizer.json` ~16.4 MiB + `libonnxruntime.so` ~4.6 MiB)。#111 实测
  更正了本文早先「模型超 Lambda Layer 限制」的归因：真正吃预算的是**函数二进制**
  （lance/datafusion 依赖，real 模式解压 235.7 MiB，strip 后 180.5 MiB）——模型资产单独看装得进
  Layer，但「函数 + Layer 解压合计 ≤ 250MB」硬限下两者放不下同一个包。因此 ZIP 部署路径改为
  **S3→/tmp 冷启动供给**（#111）：`scripts/package-model-assets.sh` 产出带逐文件 sha256 manifest
  的 `dist/model-assets/`，部署前上传到 ArtifactBucket 的 `ModelAssetPrefix` 前缀；query/build
  冷启动下载校验到 `/tmp/ltembed`（`/tmp` 不占 250MB 预算，默认 512MB ephemeral 足够）。strip
  已强制纳入 `scripts/package-lambda-zips.sh`，`scripts/check-lambda-size-budget.sh` 在 CI 持续
  断言单函数 250MB/架构/资产 hash。容器镜像 lineage 保留为兼容形态（10 GB image limit 下更宽裕）。
- **Lambda** is ideal for spiky, event-driven traffic and the batch index builder.
- **Fargate** gives an always-on service that loads the model **once per task** (no per-invoke cold
  start of a ~118 MB ONNX model) and removes the 15-minute / 10 GB `/tmp` ceilings that constrain
  the index builder on Lambda.

The goal is to avoid maintaining two artifacts: the **same image** should run in both places.

## Mechanism: AWS Lambda Web Adapter

Each component becomes a plain **HTTP server** listening on `0.0.0.0:8080`. The transport-agnostic
request cores already exist and are reused unchanged:

| Component | HTTP surface (target) | Reused core |
| --- | --- | --- |
| query | `POST /query`, `GET /health` | `handle_search_request` (`src/query_lambda.rs`) |
| write | `POST /write`, `POST /delete`, `GET /health` | `handle_write_request` (`src/write_lambda.rs`) |
| index_builder | `POST /build`, `GET /health` | `handle_build_request` (`src/build_lambda.rs`) |

The [AWS Lambda Web Adapter](https://github.com/awslabs/aws-lambda-web-adapter) is baked into each
image as a Lambda extension:

```dockerfile
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.x /lambda-adapter /opt/extensions/lambda-adapter
```

- **On Lambda**: the extension starts the app, waits for `AWS_LWA_READINESS_CHECK_PATH` to return
  200, then bridges each API Gateway / Function URL / SQS event to `http://localhost:8080`.
- **On Fargate/ECS**: `/opt/extensions/*` is never read; the container just runs the HTTP server.
  ECS health checks hit `GET /health` directly.

### Web Adapter environment knobs

| Variable | Value | Notes |
| --- | --- | --- |
| `AWS_LWA_PORT` | `8080` | Port the app listens on |
| `AWS_LWA_READINESS_CHECK_PATH` | `/health` | Adapter waits for 200 before serving |
| `AWS_LWA_INVOKE_MODE` | `buffered` | `response_stream` only if streaming is added |
| `AWS_LWA_PASS_THROUGH_PATH` | `/build` | Target design for the Lambda SQS EventSource column: raw event POSTed here. **Not set in the published images** — `/build` does not decode SQS event envelopes yet; today's automatic build path is the Fargate-side SQS worker loop (`LTSEARCH_BUILD_SQS_QUEUE_URL`) |

## Cargo build profiles (AWS is optional)

The AWS SDK and the Lambda runtime are now **optional cargo features**, not baked into every build
(ADR-0001, `docs/adr/0001-aws-optional-runtime-profiles.md`). The domain core compiles AWS-free by
default (`default = ["local"]`):

- **Server images** compile under `--features aws` (server binaries `query_server` / `write_server`
  / `index_builder_server` require the `aws` profile).
- **Lambda images** compile under `--features lambda` (the `query_lambda` / `write_lambda` /
  `index_builder_lambda` handlers require the `lambda` profile, which implies `aws`).
- A bare `cargo build` (the `local` profile) pulls in **no** AWS SDK or Lambda runtime and produces
  **no** AWS/Lambda binary. AWS-free local server binaries are deferred to #108.

## Image structure

Reuse the existing multi-stage build (`sam/builder.Dockerfile` as the shared compile stage), and
change only the runtime base and entrypoint. This is implemented as
`sam/{query,write,index_builder}_server.Dockerfile`:

```dockerfile
# --- runtime (per component), replacing public.ecr.aws/lambda/provided:al2023-arm64 ---
FROM public.ecr.aws/amazonlinux/amazonlinux:2023
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1 /lambda-adapter /opt/extensions/lambda-adapter
COPY --from=builder /<component>_server /app/server        # HTTP server binary
ENV AWS_LWA_PORT=8080 AWS_LWA_READINESS_CHECK_PATH=/health LTSEARCH_HTTP_PORT=8080
EXPOSE 8080
CMD ["/app/server"]
```

Unlike the Lambda lineage (`sam/{query,index_builder}_lambda.Dockerfile`), the HTTP server images
do **not** bake `/ltembed-assets` (or the static TurboQuant index): model assets are mounted at
runtime — see "Model assets" below.

This also **unifies the two divergent Dockerfiles** that exist today: the top-level `Dockerfile`
(x86, bakes `static/`, `CMD [bootstrap]`) is folded into the arm64 `sam/` lineage so static-index
baking lives in exactly one place.

## Platform mapping (all three components)

| Component | Fargate (always-on) | Lambda (event-driven) |
| --- | --- | --- |
| query | ECS **service** behind an ALB; desired count scales on CPU/latency | Function behind API Gateway / Function URL |
| write | ECS **service** behind an ALB | Function behind API Gateway / Function URL |
| index_builder | ECS **task** driven by an SQS worker loop (or scheduled) | Function with an SQS **EventSource** mapping |

The **same image** is deployed in either column; only the surrounding infrastructure differs.

## Model assets and architecture portability

The two image lineages handle model assets differently; the binary/bundle build is shared
(`sam/builder.Dockerfile` build args `LTEMBED_MODE=real` + `LTEMBED_BUNDLE_URL`, pinned default:
`minimal-ort-builder` **v1.0.9**, `jina-embeddings-v5-text-nano-retrieval` q4f16 **linux-arm64**;
the default `LTEMBED_MODE=stub` builds the `fixed` provider with no model — CI default):

- **Lambda ZIP + S3→/tmp 模型资产**（生产路径，#111）：`scripts/package-model-assets.sh` 只构建
  `sam/builder.Dockerfile` 的 `bundle` stage（URL + sha256 pin 的单一来源），产出
  `dist/model-assets/`（含 `manifest.json`：bundle provenance + 逐文件 sha256/bytes），部署前
  `aws s3 cp --recursive dist/model-assets s3://<ArtifactBucket>/<ModelAssetPrefix>/`。
  query/build 函数以 `LTSEARCH_{QUERY,BUILD}_LTEMBED_S3_BUCKET/_S3_PREFIX` 定位资产，冷启动时
  （`src/embedding/model_assets.rs`）按 manifest 逐文件下载 + sha256 校验到
  `LTSEARCH_{QUERY,BUILD}_LTEMBED_BUNDLE_DIR=/tmp/ltembed`（`..._LTEMBED_MODEL_PATH=
  /tmp/ltembed/model.ort`）；manifest 最后落盘作完整性标记，warm 容器复用 `/tmp` 免重复下载。
  **write 函数零模型 env、零下载代码，可独立部署**。资产未就位/下载失败时 query/build 启动报错
  直接指出 `model assets not provisioned` 与 S3 配置排查点。
- **Lambda images** (`sam/{query,index_builder}_lambda.Dockerfile`) bake the bundle into the image
  (`COPY --from=builder /ltembed-assets /ltembed-assets`) and point
  `LTSEARCH_{QUERY,BUILD}_LTEMBED_BUNDLE_DIR=/ltembed-assets` /
  `LTSEARCH_{QUERY,BUILD}_LTEMBED_MODEL_PATH=/ltembed-assets/model.ort` at the baked path.
  （兼容形态；ZIP + S3→/tmp 资产供给为 #106 之后的默认交付。）
- **HTTP server images** (`sam/*_server.Dockerfile`, published to GHCR) do **not** contain model
  assets. Operators mount an LTEmbed bundle (model.ort / tokenizer.json / build-info.json /
  libonnxruntime.so, from a `minimal-ort-builder` release) into the container and set
  `LTSEARCH_{QUERY,BUILD}_LTEMBED_BUNDLE_DIR` / `LTSEARCH_{QUERY,BUILD}_LTEMBED_MODEL_PATH` to the
  mount path. A missing/corrupt bundle surfaces as `GET /health` → 503 with a repair hint.
- In both lineages `libonnxruntime.so` is resolved from the bundle dir (or `ORT_DYLIB_PATH`).
- **Architecture must match.** `ort` uses `load-dynamic`, so the compiled binary + the bundled
  `.so` are portable **only within the same CPU arch**. The pinned bundle is arm64, so both Fargate
  and Lambda must run on **arm64 (Graviton)**. Targeting x86_64 Fargate requires an x86_64
  `minimal-ort-builder` bundle and a matching `LTEMBED_BUNDLE_URL`.

## Runtime environment variables (unchanged by the runtime bridge)

| Component | Key variables |
| --- | --- |
| query | `LTSEARCH_QUERY_S3_BUCKET`, `LTSEARCH_QUERY_ARTIFACT_ROOT`, `LTSEARCH_QUERY_EMBEDDING_PROVIDER`, `LTSEARCH_QUERY_LTEMBED_*` (static retrieval resolves via the activation pointer `static/_head` → `static/releases/<id>/`, no `LTSEARCH_QUERY_STATIC_DIR`) |
| write | `LTSEARCH_WRITE_S3_BUCKET`, `LTSEARCH_WRITE_SQS_QUEUE_URL` |
| index_builder | `LTSEARCH_BUILD_S3_BUCKET`, `LTSEARCH_BUILD_ARTIFACT_ROOT`, `LTSEARCH_BUILD_EMBEDDING_PROVIDER`, `LTSEARCH_BUILD_EMBEDDING_DIM`, `LTSEARCH_BUILD_LTEMBED_*` |

On Fargate, prefer an EFS mount or a large task ephemeral volume for `*_ARTIFACT_ROOT` so synced S3
artifacts persist for the life of the task.

## Open items before implementation

- ~~Add the HTTP server entrypoints (thin axum wrappers around the existing `handle_*` cores).~~ **Done** — `src/bin/{query,write,index_builder}_server.rs` + `src/http/`.
- ~~Wire the SQS → build trigger and version allocation (see `docs/design.md` → Known Gaps #1).~~ **Done** — index-builder auto-polls `LTSEARCH_BUILD_SQS_QUEUE_URL`, allocates head+1 version and CAS-publishes `_head`.
- Provide the ECS task definitions / Lambda SAM resources for both columns.
- Publish images to ECR for the AWS runtimes. HTTP server images already publish to **GHCR** via `.github/workflows/publish-images.yml`; an ECR push/deploy job for Fargate + Lambda does not exist yet.
