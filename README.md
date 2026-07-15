# LTSearch

Hybrid search engine for RAG retrieval, combining a static TurboQuant index with vector similarity (LanceDB) and BM25 keyword search (Tantivy) via Reciprocal Rank Fusion. Ships as container images on AWS Lambda + S3 today; Fargate support is planned (see [`docs/deployment.md`](docs/deployment.md)).

## Project Status

The MVP foundation is complete:

- Sub-plan 1: Query Core MVP
- Sub-plan 2: Write Build Publish MVP
- Sub-plan 3: Lambda Verification MVP

Current follow-on work is tracked in `Sub-plan 4: Real Embeddings + Dev Workflow`, which covers repository hygiene, streamlined verification workflows, and LTEmbed integration for real query/document embeddings.

## Prerequisites

- **Rust** — automatically installed via `rust-toolchain.toml` (1.94.0)
- **Docker** — required for Moto integration tests

## Quick Start

```bash
# Fast local verification
bash scripts/verify-fast.sh

# Moto-backed integration verification
docker compose -f docker-compose.moto.yml up -d
bash scripts/verify-moto.sh
docker compose -f docker-compose.moto.yml down -v
```

## Fast Local Checks

Use `bash scripts/verify-fast.sh` for the default local workflow. It builds all Lambda binaries, runs the non-Moto test suite, then runs `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`.

```bash
bash scripts/verify-fast.sh
```

This path is Docker-free and is the right default while iterating on most code changes.

## Moto-backed Integration Checks

Use the Moto-backed path when you need S3/SQS adapter coverage from `tests/write_build_publish_test.rs`.

```bash
docker compose -f docker-compose.moto.yml up -d
bash scripts/verify-moto.sh
docker compose -f docker-compose.moto.yml down -v
```

`scripts/verify-moto.sh` runs the Moto-dependent integration suite only; it assumes Moto is already running.

## CI

CI mirrors the same split:

- a fast Docker-free verification path for build, non-Moto tests, formatting, linting, and workflow guard checks
- a Moto-backed integration path for `tests/write_build_publish_test.rs`

## Build Profiles

AWS is an optional cargo feature (see [`docs/adr/0001-aws-optional-runtime-profiles.md`](docs/adr/0001-aws-optional-runtime-profiles.md)). The crate defaults to the AWS-free `local` profile (`default = ["local"]`), so a bare `cargo build` / `cargo test` pulls in **no** AWS SDK or Lambda runtime and produces no AWS/Lambda binary. Name a profile to build the cloud binaries:

- Lambda handlers (`query_lambda`, `write_lambda`, `index_builder_lambda`) require `--features lambda`.
- Server + offline binaries (`query_server`, `write_server`, `index_builder_server`, `turbo_index_builder`) require `--features aws`.

AWS-free local server binaries are deferred to #108.

## Lambda Binaries

All binaries are auto-discovered from `src/bin/` — no `[[bin]]` entries in `Cargo.toml` needed.

### query_lambda

Handles search requests against the active index version.

```bash
cargo build --features lambda --bin query_lambda
```

| Env Var | Description |
|---------|-------------|
| `LTSEARCH_QUERY_EMBEDDING_PROVIDER` | Embedding provider: `fixed` or `ltembed` |
| `LTSEARCH_QUERY_ARTIFACT_ROOT` | Local path to index artifacts |
| `LTSEARCH_QUERY_FIXED_EMBEDDING` | Comma-separated fixed embedding values (provider=`fixed`) |
| `LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR` | Dir with `tokenizer.json` + `build-info.json` (provider=`ltembed`) |
| `LTSEARCH_QUERY_LTEMBED_MODEL_PATH` | Path to `model.ort` |
| `LTSEARCH_QUERY_STATIC_DIR` | Optional: parent dir for static TurboQuant index (`static/` subdir). Default: `LTSEARCH_QUERY_ARTIFACT_ROOT`. Set to `/app` when using Docker image. |

### write_lambda

Accepts ingest/delete requests, persists to WAL (S3), and enqueues build jobs (SQS).

```bash
cargo build --features lambda --bin write_lambda
```

| Env Var | Description |
|---------|-------------|
| `LTSEARCH_WRITE_S3_BUCKET` | S3 bucket for WAL storage |
| `LTSEARCH_WRITE_SQS_QUEUE_URL` | SQS queue URL for build queue |

### index_builder_lambda

Reads WAL records, builds Tantivy + LanceDB indexes, and publishes new index versions via atomic `_head` update.

```bash
cargo build --features lambda --bin index_builder_lambda
```

| Env Var | Description |
|---------|-------------|
| `LTSEARCH_BUILD_S3_BUCKET` | S3 bucket for WAL + artifacts |
| `LTSEARCH_BUILD_ARTIFACT_ROOT` | Local path for staging builds (default: `/tmp/ltsearch`) |
| `LTSEARCH_BUILD_EMBEDDING_PROVIDER` | Embedding provider: `fixed` or `ltembed` |
| `LTSEARCH_BUILD_FIXED_EMBEDDING` | Comma-separated fixed embedding values (provider=`fixed`) |
| `LTSEARCH_BUILD_EMBEDDING_DIM` | Embedding dimension |
| `LTSEARCH_BUILD_LTEMBED_BUNDLE_DIR` | Dir with `tokenizer.json` + `build-info.json` (provider=`ltembed`) |
| `LTSEARCH_BUILD_LTEMBED_MODEL_PATH` | Path to `model.ort` |

### turbo_index_builder

Offline static index builder for TurboQuant (laws, contracts, RFCs). Writes compressed binary index files for bundling into the query Lambda Docker image.

```bash
cargo build --features aws --bin turbo_index_builder
```

| Env Var | Description |
|---------|-------------|
| `LTSEARCH_BUILD_EMBEDDING_PROVIDER` | Embedding provider: `fixed` or `ltembed` (default: `fixed`) |
| `LTSEARCH_BUILD_FIXED_EMBEDDING` | Comma-separated fixed embedding values (provider=`fixed`) |
| `LTSEARCH_BUILD_EMBEDDING_DIM` | Embedding dimension |
| `LTSEARCH_BUILD_LTEMBED_BUNDLE_DIR` | Dir with `tokenizer.json` + `build-info.json` (provider=`ltembed`) |
| `LTSEARCH_BUILD_LTEMBED_MODEL_PATH` | Path to `model.ort` |

Usage:
```bash
turbo_index_builder --config turbo_config.json --output /path/to/static/
```

## Local E2E Workflow

The SAM Local E2E scripts run the full write → build → query pipeline against a local Moto-backed AWS environment without deploying to real AWS.

### Prerequisites

- **SAM CLI** — `brew install aws-sam-cli`
- **AWS CLI** — for SQS polling helpers
- **Docker** — for Lambda containers and Moto

### Embedding modes

| Mode | Description | When to use |
|------|-------------|-------------|
| `fixed` (default) | Deterministic 3-dim stub vector, no model required | CI, quick local iteration |
| `ltembed` | Real `jinaai/jina-embeddings-v5-text-nano-retrieval` inference via the LTEmbed ONNX engine, 512-dim | Testing real semantic search locally |

The `ltembed` mode downloads an ort bundle (`model.ort`, `tokenizer.json`, `build-info.json`, `libonnxruntime.so`) during `docker build` from the public `minimal-ort-builder` release pinned in `sam/builder.Dockerfile` (override `LTEMBED_BUNDLE_URL` to test a different bundle). Rust tests that need real inference look for a sibling `../LTEmbed/ort_bundle/` checkout and skip when absent.

### SAM invoke E2E (CI-compatible)

Runs the full pipeline via `sam local invoke` — no persistent SAM process needed.

```bash
# Start Moto
docker compose -f docker-compose.moto.yml up -d

# Run fixed-embedding invoke flow (matches CI)
bash scripts/e2e/run-sam-local-invoke-e2e.sh

# Run LTEmbed invoke flow (downloads model on first run, ~471 MB)
LTSEARCH_E2E_LTEMBED=true bash scripts/e2e/run-sam-local-invoke-e2e.sh

# Stop Moto
docker compose -f docker-compose.moto.yml down -v
```

### SAM start-api E2E (interactive / HTTP)

Exposes `POST /write` and `POST /query` as a persistent local HTTP API. Useful for manual testing with curl or any HTTP client.

```bash
# Start Moto + SAM API in background (fixed-embedding mode)
bash scripts/e2e/start-sam-moto.sh

# Run write → build → query HTTP flow with assertions
bash scripts/e2e/run-http-flow.sh

# Teardown
bash scripts/e2e/stop-sam-moto.sh
```

After `start-sam-moto.sh`, the API is available at `http://localhost:3000`:

```bash
curl -X POST http://localhost:3000/write  -H 'Content-Type: application/json' -d @tests/fixtures/e2e/write_request.json
curl -X POST http://localhost:3000/query  -H 'Content-Type: application/json' -d @tests/fixtures/e2e/query_request.json
```

`BuildFunction` has no HTTP route and must be invoked directly:

```bash
sam local invoke BuildFunction \
  --template-file .aws-sam/build/template.yaml \
  --env-vars .e2e-tmp/env-vars.json \
  --event .e2e-tmp/build-event.json \
  --docker-network ltsearch-e2e
```

## HTTP Server Mode

除 Lambda 二进制外，三个组件还各有一个长驻 HTTP 服务二进制（`src/bin/{query,write,index_builder}_server.rs`），复用同一批 `handle_*` 请求核心，监听 `0.0.0.0:8080`，供 Fargate/ECS、本地 Compose 或 im4pe 侧编排直接以 HTTP 调用。

| Image (ghcr.io/lychee-technology/…) | Endpoints | Key env |
| --- | --- | --- |
| ltsearch-query-server | POST /query, GET /health | LTSEARCH_QUERY_{EMBEDDING_PROVIDER,S3_BUCKET,ARTIFACT_ROOT,LTEMBED_BUNDLE_DIR,LTEMBED_MODEL_PATH} |
| ltsearch-write-server | POST /write, POST /delete, GET /health | LTSEARCH_WRITE_{S3_BUCKET,SQS_QUEUE_URL} |
| ltsearch-index-builder-server | POST /build, GET /health（设 LTSEARCH_BUILD_SQS_QUEUE_URL 后自动轮询建索引） | LTSEARCH_BUILD_{S3_BUCKET,SQS_QUEUE_URL,ARTIFACT_ROOT,EMBEDDING_PROVIDER,EMBEDDING_DIM,LTEMBED_BUNDLE_DIR,LTEMBED_MODEL_PATH} |

镜像为 arm64、不内置 embedding 模型：`ltembed` 模式需把 LTEmbed bundle（model.ort / tokenizer.json /
build-info.json / libonnxruntime.so，来自 minimal-ort-builder release）挂载进容器并用
`*_LTEMBED_BUNDLE_DIR` / `*_LTEMBED_MODEL_PATH` 指向挂载路径；模型缺失或损坏时
`GET /health` 返回 503 并附修复提示（query/index-builder 探测模型完整性，write 无模型依赖故 `/health` 恒 200；query 在无 `_head`
的空索引下仍返回 200 且 `index_version` 为 null）。index-builder 设 `LTSEARCH_BUILD_SQS_QUEUE_URL` 后自动轮询
构建队列（head+1 版本分配 + CAS 发布 `_head`），无需显式 POST /build；每次构建都列举 `wal/` 前缀下
全部 WAL 段做全量快照重放，多次 write 的历史批次不会被新版本挤掉。本地全链路验证（write → SQS → 自动 build → query 命中）：
`docker compose -f docker-compose.http.yml up -d --wait && bash scripts/e2e/run-http-server-flow.sh`

## Architecture

See [`docs/arch.md`](docs/arch.md) for system architecture and [`docs/design.md`](docs/design.md) for the detailed design specification. Deployment (unified Docker image for Fargate + Lambda) is documented in [`docs/deployment.md`](docs/deployment.md).
