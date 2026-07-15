# Local-profile e2e (file-based) — Design

Date: 2026-07-15
Status: Approved (design), pending implementation plan
Related issue: #108 (partial — delivers the moto-free HTTP local loop + e2e on
today's file-based durability; the SQLite durability criterion of #108 remains
out of scope here)

## Problem

CI has no end-to-end test for the AWS-free "local" profile. Every existing e2e
(`http-e2e`, `sam-e2e`) depends on `moto` to mock S3/SQS, and all three
`*_server` binaries are gated `required-features = ["aws"]`, so there is no way
to exercise the local write→build→query loop over HTTP.

The local loop already works at the *library* level: `tests/runtime_local_test.rs`
constructs `LocalFsWalStorage`, `LocalFsBuildQueue`, `LocalFsPublishStorage`,
`NoopArtifactSync`, and `LocalManifestStore` and proves they assemble without
touching AWS. The gap is (a) HTTP servers that instantiate those local impls, and
(b) a CI job that drives them over HTTP with no moto/S3/SQS.

This design fills that gap using today's file-based durability. When #108 later
swaps a SQLite substrate in behind the same contracts, this e2e keeps validating
the profile unchanged.

## Non-goals (explicitly deferred to #108)

- SQLite durability substrate (durable event log / build-job queue / active-release
  coordinator). Today's local durability is plain files and stays that way here.
- Single-image, command-dispatch packaging (one OCI image running write/builder/query
  by subcommand).
- Docker/OCI packaging and a docker-compose variant.
- The "verify behavior across restarts" acceptance criterion of #108.
- Fargate/Lambda profile refinements (no-Lambda-adapter Fargate, etc.).

## Architecture

### Backend wiring (approach B — cfg-select inside the existing server bins)

The three existing server binaries are reused. Their `required-features` change
from `["aws"]` to `["server"]` (a feature enabled by both `local` and `aws`), and
only the composition root of each is split by cfg:

```rust
#[cfg(feature = "aws")]      // AwsS3WalStorage / AwsSqsBuildQueue / S3ArtifactSync / AwsPublishStorage
#[cfg(not(feature = "aws"))] // LocalFsWalStorage / LocalFsBuildQueue / NoopArtifactSync / LocalFsPublishStorage / LocalManifestStore
```

- `--features local` → AWS-free servers wired to the `LocalFs*` impls.
- `--features aws` → unchanged; the existing `http-e2e`/`sam-e2e` continue to
  cover this path and catch any regression from touching the bins.

The axum skeleton (`src/http/`) and the build-worker loop (`run_build_job_loop`,
already provider-neutral) are untouched. `query_server` already constructs no AWS
client and reads the active release from the local filesystem via
`LocalManifestStore`; relaxing its feature gate is sufficient for it.

Rejected alternatives:
- **A — separate `*_server_local` bins:** duplicates the server skeletons; divergence risk.
- **C — one multiplexed local binary with subcommands:** that is #108's eventual
  shape; more change than needed now.

### Local index-builder triggering

Add a `run_local_worker_loop` mirroring the existing `#[cfg(feature = "aws")]
run_sqs_worker_loop`: it constructs `LocalFsBuildQueue` as the `BuildJobSource`
and feeds it into the provider-neutral `run_build_job_loop`. `index_builder_server`
spawns this loop under `#[cfg(not(feature = "aws"))]`.

### Runtime topology (native processes, one shared dir)

All three servers run as native processes on the CI runner, sharing a single
root directory. Every local contract impl takes one `root` and uses conventional
subpaths (verified in `runtime_local_test.rs`: `LocalFsWalStorage::new(root)`,
`LocalFsBuildQueue::new(root)`, `LocalFsPublishStorage::new(root)`,
`LocalManifestStore::new(root)`), so a single shared root closes the loop:

```
$ROOT/                          (shared temp dir on the runner)
  wal/        write events appended by write_server
  queue/      build jobs enqueued by write_server, consumed by index_builder_server
  <artifacts> immutable LanceDB + Tantivy releases published by index_builder_server
  <_head/manifest>  active-release pointer advanced by index_builder_server

write_server         :PORT_W   POST /write  → append WAL + enqueue build job
index_builder_server :PORT_B   run_local_worker_loop polls queue → build → publish → advance active
query_server         :PORT_Q   POST /query  → read active release from $ROOT (NoopArtifactSync)
```

### Environment contract

- `LTSEARCH_LOCAL_ROOT` — single shared root all three local servers use (query's
  existing `LTSEARCH_QUERY_ARTIFACT_ROOT` is honored to the same path in local mode;
  the two are reconciled in the implementation plan so one value drives all servers).
- `LTSEARCH_HTTP_PORT` — per-server listen port (existing).
- Embeddings use `EmbeddingProvider::Fixed` (a deterministic fixed vector from env,
  e.g. `LTSEARCH_QUERY_FIXED_EMBEDDING` and the builder's equivalent), available
  **without** the `ltembed` feature — so CI needs no model download and no assets.

## The e2e harness

New script `scripts/e2e/run-local-server-flow.sh` (curl-driven, structurally
mirrors `run-http-server-flow.sh` minus moto/aws-init):

1. Launch the three servers as background processes against a fresh `$ROOT`; poll
   `GET /health` on each until ready (bounded retries).
2. `POST /write` 6 documents to `write_server`; poll until `index_builder_server`
   publishes v1.
3. `POST /query` to `query_server`; assert the expected document is returned.
4. Second `POST /write`; poll for v2; assert both batches are queryable.
5. A trap tears down the background servers on exit; the script exits non-zero on
   any failed assertion or timeout.

## GHA job

New job `local-e2e` in `.github/workflows/ci.yml`:

- `runs-on: ubuntu-24.04-arm`, standalone (no `needs:`) so it runs in parallel and
  fails fast.
- Steps: `actions/checkout` → `actions-rust-lang/setup-rust-toolchain` →
  `cargo build --no-default-features --features local --bin write_server
  --bin index_builder_server --bin query_server` →
  `bash scripts/e2e/run-local-server-flow.sh`.
- No image build, no moto, no model download — cheap.

## Testing / verification

- Run `scripts/e2e/run-local-server-flow.sh` locally to green.
- Confirm `--features aws` still builds the three touched bins (regression guard;
  already covered by the `feature-matrix` and `fast` jobs).
- Confirm the new `local-e2e` job is green on the PR.

## Files touched (anticipated)

- `Cargo.toml` — change `required-features` of the three `*_server` bins from
  `["aws"]` to `["server"]`.
- `src/bin/write_server.rs`, `src/bin/index_builder_server.rs`,
  `src/bin/query_server.rs` — cfg-split composition roots.
- `src/build_worker.rs` (or sibling) — add `run_local_worker_loop`.
- `scripts/e2e/run-local-server-flow.sh` — new.
- `.github/workflows/ci.yml` — new `local-e2e` job.
- Possibly a small doc/ADR note that local server binaries are now buildable under
  `local` (partial step toward #108).
