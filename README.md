# LTSearch

Hybrid search engine for RAG retrieval, combining a static TurboQuant index with vector similarity (LanceDB) and BM25 keyword search (Tantivy) via Reciprocal Rank Fusion. Ships as one AWS-free local Docker image (`ghcr.io/lychee-technology/ltsearch-local`, SQLite-backed) and as Lambda ZIP artifacts (`provided.al2023`, arm64) for AWS (see [`docs/deployment.md`](docs/deployment.md)).

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

- The unified local binary (`ltsearch`: `write` / `build` / `query` / `static-build` / `static-activate` subcommands) requires `--features local` (the default).
- Lambda handlers (`query_lambda`, `write_lambda`, `index_builder_lambda`) require `--features lambda`.
- Offline/ops binaries (`turbo_index_builder`, `static_activate`) require `--features aws`.

## Lambda Binaries

Cloud binaries are feature-gated via explicit `[[bin]]` entries in `Cargo.toml` (see Build Profiles above).

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

Static TurboQuant retrieval uses no implicit `static/` directory or `LTSEARCH_QUERY_STATIC_DIR` override: it resolves through the activation pointer `static/_head` → `static/releases/<id>/` under `LTSEARCH_QUERY_ARTIFACT_ROOT` (see the static-activate flow).

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

Offline static index builder for TurboQuant (laws, contracts, RFCs). Writes compressed binary index files; static releases are shipped and activated through the activation-pointer flow (see `docs/deployment.md`).

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

The Lambda ZIP E2E scripts run the full write → build → query pipeline against a local Moto-backed AWS environment without deploying to real AWS, using the production ZIP template (`template.yaml`).

### Prerequisites

- **SAM CLI** — `brew install aws-sam-cli`
- **AWS CLI** — for SQS polling helpers
- **Docker** — for the builder image and Moto

### Embedding modes

| Mode | Description | When to use |
|------|-------------|-------------|
| `fixed` (default) | Deterministic 3-dim stub vector, no model required | CI, quick local iteration |
| `ltembed` | Real `jinaai/jina-embeddings-v5-text-nano-retrieval` inference via the LTEmbed ONNX engine, 512-dim | Testing real semantic search locally |

The `ltembed` mode downloads an ort bundle (`model.ort`, `tokenizer.json`, `build-info.json`, `libonnxruntime.so`) during `docker build` from the public `minimal-ort-builder` release pinned in `sam/builder.Dockerfile` (override `LTEMBED_BUNDLE_URL` to test a different bundle). Rust tests that need real inference look for a sibling `../LTEmbed/ort_bundle/` checkout and skip when absent.

### Lambda ZIP invoke E2E (CI-compatible)

Packages the function ZIPs and drives them via `sam local invoke` against the production ZIP template.

```bash
# Start Moto
docker compose -f docker-compose.moto.yml up -d

# Run stub-embedding ZIP flow (matches CI sam-zip-e2e)
bash scripts/e2e/run-sam-zip-invoke-e2e.sh

# Run real-mode ZIP flow with S3→/tmp model assets (matches CI sam-ltembed-e2e;
# downloads the pinned ort bundle on first run, ~471 MB)
bash scripts/e2e/run-sam-ltembed-invoke-e2e.sh

# Stop Moto
docker compose -f docker-compose.moto.yml down -v
```

### Native local flows (Docker-free / moto-free)

```bash
# write → build → query with restart durability, native processes
bash scripts/e2e/run-local-server-flow.sh

# static release build → activate → query (v3)
bash scripts/e2e/run-static-release-flow.sh
```

## Local Single-Image Mode

The unified local image (`sam/local.Dockerfile`, published as `ghcr.io/lychee-technology/ltsearch-local`) is AWS-free: one image, five roles selected by subcommand (`write` / `build` / `query` / `static-build` / `static-activate`), backed by SQLite for durable events, build jobs, and release pointers.

```bash
docker build --platform linux/arm64 -f sam/local.Dockerfile -t ltsearch-local:dev .
docker compose -f docker-compose.local.yml up -d --wait
bash scripts/e2e/run-local-image-flow.sh   # write → build → query + restart durability
docker compose -f docker-compose.local.yml down -v
```

To run a published release image instead of a local build, set
`LTSEARCH_LOCAL_IMAGE=ghcr.io/lychee-technology/ltsearch-local:<tag>` for the compose commands
(see [`docs/deployment.md`](docs/deployment.md)).

`docker-compose.local.yml` runs three services (`write`, `build`, `query`) from the same image, sharing the named volume `ltsearch-local-data` mounted at `/var/lib/ltsearch` (`LTSEARCH_LOCAL_ROOT`). The volume holds the SQLite control plane (`ltsearch.db`) plus immutable index artifacts; `docker compose down` without `-v` preserves all state across restarts. See [`docs/deployment.md`](docs/deployment.md) for the operator guide.

### Real-LTEmbed Local Topology (E2E)

A second, test-only topology runs the same three roles with the real LTEmbed model (#141). `sam/local-ltembed.Dockerfile` compiles `ltsearch` with `--features local,ltembed` and bakes the pinned, checksum-verified linux/arm64 ort bundle into the image at `/opt/ltembed` — no Moto, no AWS env vars, no Lambda/SAM. The bundle URL/SHA256 pin stays single-sourced in `sam/builder.Dockerfile` and is injected at build time.

```bash
# Build the real image (stages the pinned LTEmbed checkout, downloads the
# ~120 MB ort bundle on first run, linux/arm64 only)
bash scripts/e2e/build-local-ltembed-image.sh

# Blackbox main chain: health → write → automatic build → query,
# asserted only via /health, /write, /query HTTP responses
bash scripts/e2e/run-local-real-flow.sh
```

Each run is fully isolated: a unique compose project (`ltsearch-real-<run_id>`), ephemeral loopback ports discovered via `docker compose port`, and project-scoped volume/network — concurrent runs do not collide. Query/build healthchecks execute a real embedding probe, so `up -d --wait` going healthy means real inference works (first model load is slow; the healthcheck allows for it). On success the runner removes all containers, volumes, and scratch files; on failure it tears the stack down but preserves service logs and recorded request/response payloads under `.e2e-tmp/ltsearch-real-<run_id>/`. This topology is not part of the PR gate; daily CI regression is tracked by #144.

## Releases

Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`, which:

- assembles the release payload with `scripts/package-release.sh --mode real`: `query_lambda.zip`, `write_lambda.zip`, `index_builder_lambda.zip`, `model-assets.zip`, `release-provenance.json`, and `SHA256SUMS`;
- builds and pushes exactly one local OCI image `ghcr.io/lychee-technology/ltsearch-local:<tag>` (arm64; `latest` only for stable semver tags);
- creates a GitHub Release with all payload files attached (hyphenated tags are marked pre-release).

Verify downloaded assets with `sha256sum -c SHA256SUMS`. `release-provenance.json` records the git SHA, workflow run, LTEmbed bundle pin, and per-artifact digests. Dry-run the assembly locally without publishing:

```bash
bash scripts/package-release.sh --mode stub --version v0.0.0-dev
```

CI validates the same assembly path on every PR via the `release-assembly` job (stub mode, checksum and provenance assertions, no publishing).

## Architecture

See [`docs/arch.md`](docs/arch.md) for system architecture and [`docs/design.md`](docs/design.md) for the detailed design specification. Deployment (local single image + Lambda ZIPs) is documented in [`docs/deployment.md`](docs/deployment.md).
