# LTSearch

Serverless hybrid search engine for RAG retrieval, combining vector similarity (LanceDB) with BM25 keyword search (Tantivy) via Reciprocal Rank Fusion. Runs on AWS Lambda + S3.

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

## Lambda Binaries

All binaries are auto-discovered from `src/bin/` — no `[[bin]]` entries in `Cargo.toml` needed.

### query_lambda

Handles search requests against the active index version.

```bash
cargo build --bin query_lambda
```

| Env Var | Description |
|---------|-------------|
| `LTSEARCH_QUERY_EMBEDDING_PROVIDER` | Embedding provider: `fixed` or `ltembed` |
| `LTSEARCH_QUERY_ARTIFACT_ROOT` | Local path to index artifacts |
| `LTSEARCH_QUERY_FIXED_EMBEDDING` | Comma-separated fixed embedding values (provider=`fixed`) |
| `LTSEARCH_QUERY_LTEMBED_MODEL_PATH` | Path to `model.safetensors` (provider=`ltembed`) |
| `LTSEARCH_QUERY_LTEMBED_CONFIG_PATH` | Path to `config.json` |
| `LTSEARCH_QUERY_LTEMBED_TOKENIZER_PATH` | Path to `tokenizer.json` |
| `LTSEARCH_QUERY_LTEMBED_POOLING` | Pooling strategy (e.g. `mean`) |
| `LTSEARCH_QUERY_STATIC_DIR` | Optional: parent dir for static TurboQuant index (`static/` subdir). Default: `LTSEARCH_QUERY_ARTIFACT_ROOT`. Set to `/app` when using Docker image. |

### write_lambda

Accepts ingest/delete requests, persists to WAL (S3), and enqueues build jobs (SQS).

```bash
cargo build --bin write_lambda
```

| Env Var | Description |
|---------|-------------|
| `LTSEARCH_WRITE_S3_BUCKET` | S3 bucket for WAL storage |
| `LTSEARCH_WRITE_SQS_QUEUE_URL` | SQS queue URL for build queue |

### index_builder_lambda

Reads WAL records, builds Tantivy + LanceDB indexes, and publishes new index versions via atomic `_head` update.

```bash
cargo build --bin index_builder_lambda
```

| Env Var | Description |
|---------|-------------|
| `LTSEARCH_BUILD_S3_BUCKET` | S3 bucket for WAL + artifacts |
| `LTSEARCH_BUILD_ARTIFACT_ROOT` | Local path for staging builds (default: `/tmp/ltsearch`) |
| `LTSEARCH_BUILD_EMBEDDING_PROVIDER` | Embedding provider: `fixed` or `ltembed` |
| `LTSEARCH_BUILD_FIXED_EMBEDDING` | Comma-separated fixed embedding values (provider=`fixed`) |
| `LTSEARCH_BUILD_EMBEDDING_DIM` | Embedding dimension |
| `LTSEARCH_BUILD_LTEMBED_MODEL_PATH` | Path to `model.safetensors` (provider=`ltembed`) |
| `LTSEARCH_BUILD_LTEMBED_CONFIG_PATH` | Path to `config.json` |
| `LTSEARCH_BUILD_LTEMBED_TOKENIZER_PATH` | Path to `tokenizer.json` |
| `LTSEARCH_BUILD_LTEMBED_POOLING` | Pooling strategy (e.g. `mean`) |

### turbo_index_builder

Offline static index builder for TurboQuant (laws, contracts, RFCs). Writes compressed binary index files for bundling into the query Lambda Docker image.

```bash
cargo build --bin turbo_index_builder
```

| Env Var | Description |
|---------|-------------|
| `LTSEARCH_BUILD_EMBEDDING_PROVIDER` | Embedding provider: `fixed` or `ltembed` (default: `fixed`) |
| `LTSEARCH_BUILD_FIXED_EMBEDDING` | Comma-separated fixed embedding values (provider=`fixed`) |
| `LTSEARCH_BUILD_EMBEDDING_DIM` | Embedding dimension |
| `LTSEARCH_BUILD_LTEMBED_MODEL_PATH` | Path to `model.safetensors` (provider=`ltembed`) |
| `LTSEARCH_BUILD_LTEMBED_CONFIG_PATH` | Path to `config.json` |
| `LTSEARCH_BUILD_LTEMBED_TOKENIZER_PATH` | Path to `tokenizer.json` |
| `LTSEARCH_BUILD_LTEMBED_POOLING` | Pooling strategy (e.g. `mean`) |

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
| `ltembed` | Real model inference. Production target: `jinaai/jina-embeddings-v5-text-nano`, 512-dim; local E2E currently still runs the legacy e5-small stack at 384-dim until the engine upgrade lands (#96) | Testing real semantic search locally |

The `ltembed` mode downloads `model.safetensors`, `config.json`, and `tokenizer.json` from HuggingFace automatically during `docker build` (model pinned by `HF_MODEL` in `sam/builder.Dockerfile`). No manual file setup is required.

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

## Architecture

See [`docs/arch.md`](docs/arch.md) for system architecture and [`docs/design.md`](docs/design.md) for the detailed design specification.
