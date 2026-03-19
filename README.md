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
# Build all binaries
cargo build

# Run unit tests
cargo test

# Run integration tests (requires Moto)
docker compose -f docker-compose.moto.yml up -d
cargo test --test write_build_publish_test -- --nocapture

# Lint
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

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
