# LTSearch

Serverless hybrid search engine for RAG retrieval, combining vector similarity (LanceDB) with BM25 keyword search (Tantivy) via Reciprocal Rank Fusion. Runs on AWS Lambda + S3.

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
| `LTSEARCH_QUERY_EMBEDDING_PROVIDER` | Embedding provider (`fixed` for MVP) |
| `LTSEARCH_QUERY_ARTIFACT_ROOT` | Local path to index artifacts |
| `LTSEARCH_QUERY_FIXED_EMBEDDING` | Comma-separated fixed embedding values |

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
| `LTSEARCH_BUILD_EMBEDDING_PROVIDER` | Embedding provider (`fixed` for MVP) |
| `LTSEARCH_BUILD_FIXED_EMBEDDING` | Comma-separated fixed embedding values |
| `LTSEARCH_BUILD_EMBEDDING_DIM` | Embedding dimension |

## Architecture

See [`docs/arch.md`](docs/arch.md) for system architecture and [`docs/design.md`](docs/design.md) for the detailed design specification.
