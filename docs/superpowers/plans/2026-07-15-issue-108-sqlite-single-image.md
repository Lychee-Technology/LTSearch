# Issue #108 — SQLite single-image local retrieval loop — Implementation Plan

> **For agentic workers:** this is a design-level plan for a large issue. It is
> decomposed into 5 independently-reviewable work packages (recommend filing as
> sub-issues of #108). Each package should be turned into a bite-sized TDD plan via
> superpowers:writing-plans before implementation.

## Context

Issue **#108** ("交付单镜像 SQLite 本地动态检索链路", child of epic #106,
`ready-for-agent`) delivers the AWS-free **Local** profile as a production shape:
one OCI image running write/index-builder/query **by command selection**, backed
by **SQLite** for the durable document-event log, the build-job queue, and the
active dynamic-release pointer, with **immutable LanceDB/Tantivy artifacts on a
shared volume**, proven by a **moto-free Compose flow that survives restarts**.

Builds on epic #116/#118 (local composition roots + HTTP servers + native-process
e2e on *file-based* durability). #108 replaces that durability substrate with
SQLite behind the **same provider-neutral contracts** (`WalStorage`,
`BuildQueue`+`BuildJobSource`, `ManifestStore`, and the `index/_head` CAS currently
on `PublishStorage`), leaving the HTTP surface and the artifact byte-storage
unchanged.

### Why SQLite (not LanceDB) for the control plane

The local durability substrate is three mutation-heavy, contended, transactional
workloads (atomic event+job write, queue claim/lease/retry/dead-letter, single-value
CAS pointer). SQLite is purpose-built for this: cross-table `BEGIN…COMMIT`, row-level
conditional `UPDATE … RETURNING`, WAL + `busy_timeout` cross-process atomicity, in-place
updates. LanceDB — already used for the **immutable vector artifacts** — is the wrong
tool here: table-level optimistic concurrency (no cross-table ACID → cannot satisfy the
atomic event+job write), no `RETURNING`/row-lock claim primitive, and version/manifest
bloat under frequent small mutations. Design principle: use each engine where it fits —
SQLite for the control plane, LanceDB for the vector artifacts.

### Considered alternatives for the SQLite driver (rusqlite vs Turso)

**Turso Database** (`tursodatabase/turso`, the Rust rewrite of SQLite formerly "Limbo";
v0.7.0 as of 2026-07) was evaluated. Appeal: async-native (no `spawn_blocking` wrapper),
pure Rust, `BEGIN CONCURRENT`/MVCC, SQLite-compatible file format. **Rejected for now**
because the substrate's acceptance criteria are precisely durability and crash/restart
correctness, and Turso is pre-1.0 with <100% SQLite compatibility and some experimental
features — the wrong place to take engine-maturity risk. Our multi-process shared-volume
topology (write + builder containers writing one `.db`) further stresses a young engine's
file-locking, where real SQLite's multi-process WAL is battle-tested. The "pure Rust /
no C compile" advantage is muted here (the build already carries a gcc/g++ toolchain for
lance/datafusion/onnxruntime), and `BEGIN CONCURRENT`'s concurrent-write benefit is
marginal at this workload's tiny write volume. Because the driver lives behind the
provider-neutral contracts (`src/local/sqlite/*` only), swapping to Turso later — once it
reaches 1.0 with full SQLite compatibility — is low blast-radius. **Revisit at Turso 1.0.**

## Approved decisions

1. **rusqlite** with the `bundled` feature (compiles SQLite from source — no system
   lib, matches the repo's vendored-protoc/ort philosophy). Traits are `async`; wrap
   blocking rusqlite calls in `tokio::task::spawn_blocking` over a `Connection` behind
   an `Arc<Mutex<…>>` / small pool.
2. **SQLite is the sole local durability backend.** The file-based durability impls
   from #116 (`LocalFsWalStorage`, `LocalFsBuildQueue`, and the `index/_head` file CAS)
   are **retired**; the #116 native e2e switches to SQLite. Artifact bytes stay
   file-based (`LocalFsPublishStorage` for lance/tantivy/manifest; `NoopArtifactSync`).
3. **Full queue semantics**: atomic claim + visibility lease + retry-with-backoff +
   dead-letter (needs a small, backward-compatible worker-loop change).
4. **Single `ltsearch` binary** with hand-rolled `env::args()` subcommand dispatch
   `write|build|query|static-build` (matches `src/bin/turbo_index_builder.rs`; no
   `clap`). One image; Compose selects role via `command: ["write"]`.

**Scope boundary:** #108 builds and CI-exercises the single image + moto-free restart
Compose. **Publishing** to GHCR and retiring the 3-component publish path is **#113** —
out of scope; design the Dockerfile/tag so #113 adopts it unchanged.

## Architecture

### SQLite substrate (rusqlite, bundled)

One DB at `<LTSEARCH_LOCAL_ROOT>/ltsearch.db`, **WAL journal mode**
(`PRAGMA journal_mode=WAL; PRAGMA busy_timeout=…`) so the three containers sharing the
volume read concurrently while a single writer commits. Schema (created idempotently on
startup by every subcommand):

- `wal_events(event_id TEXT PRIMARY KEY, batch_id, segment_key, doc_id, op, document_json, timestamp)` — durable document-event log.
- `build_jobs(batch_id TEXT PRIMARY KEY, body, state /* ready|claimed|dead */, attempts, available_at, claimed_at)` — queue with claim/lease/retry.
- `dead_jobs(batch_id TEXT PRIMARY KEY, body, attempts, last_error, died_at)` — dead-letter.
- `active_head(id INTEGER PRIMARY KEY CHECK (id=1), version_id, manifest_path, updated_at, etag)` — single-row active-release pointer.

### Contract impls (new `src/local/sqlite/…`, behind existing traits)

| Contract (trait @ file) | New SQLite impl | Behavior |
|---|---|---|
| `WalStorage` (`src/write/wal.rs:7`) | `SqliteWalStorage` | `append(key,bytes)` inserts the batch's JSONL rows in one transaction; `read(key)` selects rows for `segment_key=key`, reconstructs JSONL bytes. Mirrors `LocalFsWalStorage` byte-contract. |
| `BuildQueue::enqueue` (`src/write/api.rs:20`) | `SqliteBuildQueue` | `enqueue(QueueBatch)` = INSERT a `build_jobs` row `state='ready'`. Shares a transaction with the WAL append (write-path atomicity). |
| `BuildJobSource::receive`/`ack`/**`nack`** (`src/contracts.rs:29`) | `SqliteBuildJobSource` (same struct) | `receive` = atomic `UPDATE … SET state='claimed', claimed_at=now WHERE state='ready' AND available_at<=now RETURNING …` (+ reclaim expired leases). `ack` = DELETE. `nack(job,err)` = if `attempts+1 >= MAX_ATTEMPTS` move to `dead_jobs`, else `UPDATE … state='ready', attempts+1, available_at=now+backoff`. |
| `ManifestStore` (`src/storage/manifest_store.rs:36`) | `SqliteManifestStore` | `load_head` reads `active_head` row → `ManifestHead`; `load_active_manifest` then reads `<root>/<manifest_path>` (still a file). |
| `index/_head` CAS on `PublishStorage` (`src/indexing/publisher.rs:187`) | hybrid `LocalPublishStorage` | `compare_and_swap`/`read` of `INDEX_HEAD_KEY` route to the `active_head` row (conditional `UPDATE … WHERE etag IS :expected`, rows-affected = swapped); all other keys route to the filesystem as `LocalFsPublishStorage` does today. |
| `PublishStorage` artifact bytes; `ArtifactSync` | unchanged | lance/tantivy/manifest stay files on the shared volume; `NoopArtifactSync` stays a no-op (query reads via `resolve_artifact_path`, `src/query/retrieval_common.rs:87`). |

Reused unchanged (trait-only consumers): `WriteApi`/`WriteAheadLog`,
`IndexPublisher`/`LocalIndexBuilder`, `next_version_id`/`process_queue_message`/
`run_build_job_loop` (`src/build_worker.rs`), routers (`src/http/{write,build,query}.rs`),
`serve`/`port_from_env`.

### Write-path atomicity (AC-1)

`WriteApi::ingest` calls `wal.append` then `queue.enqueue` (`src/write/api.rs:150-175`).
For "atomically records document events **and** a pending build job before ack", both
must land in one SQLite transaction. Since `SqliteWalStorage` and `SqliteBuildQueue`
share one `Arc<Mutex<Connection>>`, a transaction wrapper over the shared handle makes
the event insert + job insert commit together, keeping the `WalStorage`/`BuildQueue`
trait shapes. Confirm the exact seam in `api.rs:150-201`; verify no caller relies on
independent append/enqueue.

### Worker loop: outcome signaling (AC-2)

`run_build_job_loop_once` (`src/build_worker.rs:156-193`) **acks always** today (drops on
failure). Add a backward-compatible outcome channel:
- Add `async fn nack(&self, job: &BuildJob, error: &str) -> Result<(), String>` to
  `BuildJobSource` (`src/contracts.rs:29`) **with a default impl calling `self.ack(job)`**
  — existing impls (`LocalFsBuildQueue`, `SqsBuildJobSource`) keep current behavior.
- Loop `ack`s on success, `nack`s on failure. `SqliteBuildJobSource` overrides `nack` for
  real retry/DLQ. (Also lets a future SQS impl stop dropping on failure — correctness win.)
- `MAX_ATTEMPTS` (default 3) + backoff via env (`LTSEARCH_BUILD_MAX_ATTEMPTS`; unspecified
  in the issue, so we choose sane defaults).

### Single `ltsearch` binary (packaging)

- Extract each server composition root into a lib entrypoint:
  `ltsearch::app::run_write().await` / `run_build()` / `run_query()` /
  `run_static_build(args)` — each builds its router from the **local** adapters (SQLite
  durability + fs artifacts) and calls `serve(...)`. Reuse #118's `run_*_local` fns if
  present; else create them here and have #118's bins delegate.
- New `src/bin/ltsearch.rs`: `match std::env::args().nth(1).as_deref() { Some("write") =>
  run_write().await, … , _ => usage_and_exit() }` (hand-rolled, per `turbo_index_builder.rs`).
- Config: single `LTSEARCH_LOCAL_ROOT` (add `LocalConfig` if #117 hasn't) → derives
  `wal/`, artifact dirs, `ltsearch.db`. `LTSEARCH_HTTP_PORT` per role. Fixed embeddings
  (`*_EMBEDDING_PROVIDER=fixed`) so no model download.
- `[[bin]] ltsearch` with `required-features = ["server"]`.

### Packaging & e2e

- `sam/local.Dockerfile`: builder stage builds `ltsearch` with `--features local`;
  runtime `amazonlinux:2023` arm64 copies `/ltsearch` → `/app/ltsearch`,
  `ENV LTSEARCH_HTTP_PORT=8080`, `EXPOSE 8080`, **no LWA**, no `CMD` (Compose supplies
  the subcommand). Extend `sam/builder.Dockerfile`'s stub branch to build `ltsearch` under
  `local`.
- `docker-compose.local.yml`: one image, 3 services differing only by `command:`; **one
  named volume** `ltsearch-data` mounted at `LTSEARCH_LOCAL_ROOT=/var/lib/ltsearch` in all
  three (holds WAL, `ltsearch.db`, `index/`+`lance/` artifacts); no moto/aws-init/AWS env;
  loopback ports + `/health` healthchecks like `docker-compose.http.yml`.
- `scripts/e2e/run-local-image-flow.sh`: `up -d --wait` → `POST /write` → poll query
  `/health` `index_version>=1` → `POST /query` hit → **restart preserving the volume**
  (`down` **without `-v`**, then `up -d --wait`) → assert query still serves the built
  version → new `POST /write` → poll `index_version` bump → `POST /query` new-doc hit
  (builder still claims from persisted SQLite queue post-restart). Model on
  `scripts/e2e/run-http-server-flow.sh`.
- `.github/workflows/ci.yml`: add `local-image-e2e` (`needs: integration`, no awscli/sam):
  build `ltsearch-e2e-builder` (stub) + `sam/local.Dockerfile`, compose up, run flow,
  `down -v` in `always()`.
- Update `tests/test_ci_workflow.py` for the new job. Keep `feature-matrix`'s AWS-free-graph
  guard green (`ltsearch` local build must not pull `aws-*`/`lambda_runtime`).

## Critical files

- **New:** `src/local/sqlite/{mod,schema,wal,queue,head,manifest}.rs`, `src/bin/ltsearch.rs`,
  `src/app.rs` (run_* entrypoints), `sam/local.Dockerfile`, `docker-compose.local.yml`,
  `scripts/e2e/run-local-image-flow.sh`.
- **Modify:** `Cargo.toml` (add `rusqlite` bundled; `[[bin]] ltsearch`; drop retired
  file-durability wiring), `src/contracts.rs` (`nack` default), `src/build_worker.rs:156-193`
  (ack/nack), `src/write/api.rs:150-201` (single-transaction write), `src/local/mod.rs`
  (export sqlite; retire fs durability), `sam/builder.Dockerfile`, `.github/workflows/ci.yml`,
  `tests/test_ci_workflow.py`.
- **Retire after cutover:** `src/local/fs_wal.rs`, `src/local/fs_build_queue.rs`, and the
  `index/_head` file-CAS branch of `src/local/fs_publish.rs` (fold head into SQLite; keep
  artifact bytes on fs).

## Reuse (do not re-implement)

`WriteApi`/`WriteAheadLog`, `IndexPublisher`/`LocalIndexBuilder`,
`run_build_job_loop`/`next_version_id`/`process_queue_message`, the routers +
`serve`/`port_from_env`, `resolve_artifact_path` (`src/query/retrieval_common.rs:87`),
`materialize_latest_snapshot` (`src/indexing/builder.rs:372`), the
`ManifestHead`/`ActiveManifest`/`IndexManifest` types, `turbo_index_builder.rs`'s
arg-parse style. `tests/runtime_local_test.rs` is the SQLite construction-test template.

## Work packages (recommend as sub-issues of #108)

1. **SQLite substrate + schema** — rusqlite (bundled), `src/local/sqlite/*`, WAL-mode init,
   `SqliteWalStorage` + `SqliteManifestStore` + head-CAS hybrid publish; unit tests.
2. **SQLite queue + worker outcome signaling** — `SqliteBuildQueue`/`SqliteBuildJobSource`
   (claim/lease/retry/DLQ), `BuildJobSource::nack` default, worker ack/nack; unit tests for
   retry + dead-letter.
3. **Atomic write path** — single-transaction event+job commit before ack; retire
   file-durability impls; SQLite as the sole local backend.
4. **`ltsearch` single binary** — extract `run_*` app entrypoints, subcommand dispatcher,
   `LTSEARCH_LOCAL_ROOT` config.
5. **Image + moto-free restart e2e** — `sam/local.Dockerfile`, `docker-compose.local.yml`,
   `run-local-image-flow.sh`, `local-image-e2e` CI job, `test_ci_workflow.py` update.

Sequencing: 1→2→3 sequential on the SQLite layer; 4 depends on 3; 5 depends on 4.

## Verification

- **Unit**: `cargo test --no-default-features --features local --lib` — WAL round-trip,
  atomic claim, retry→dead-letter after MAX_ATTEMPTS, head CAS conflict, atomic write-path
  event+job. Mirror `tests/runtime_local_test.rs`.
- **Native e2e (fast)**: `scripts/e2e/run-local-server-flow.sh` (from #116) on the SQLite
  backend, driving 3 `ltsearch` subcommand processes sharing a temp `LTSEARCH_LOCAL_ROOT`,
  plus a same-process restart proving durability without Docker.
- **Image e2e (authoritative)**: `local-image-e2e` green on the PR — one image, 3 services,
  shared volume, moto-free, including the down-without-`-v` → up restart assertion.
- **Guards**: `feature-matrix` AWS-free-graph check stays green; `scripts/verify-fast.sh`
  (fmt + clippy `-D warnings` both profiles) green; `http-e2e`/`sam-e2e` unaffected (worker
  `nack` default preserves AWS behavior).

## Prerequisites

- PR #115 (lance 8 / lancedb 0.31 build fix) merged so `main` builds.
- Epic #116 / issue #118 (local composition roots + HTTP servers + native e2e) landed;
  #108 swaps their durability substrate for SQLite and adds the single binary + image.
