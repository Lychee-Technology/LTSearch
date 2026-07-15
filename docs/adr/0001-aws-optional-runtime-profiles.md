# ADR-0001: AWS-Optional Runtime Profiles

- Status: Accepted
- Date: 2026-07-14
- Issue: #107 (follow-on: #108)

## Context

LTSearch began life as an AWS-native, Lambda-only engine. The AWS SDKs
(`aws-config`, `aws-sdk-s3`, `aws-sdk-sqs`) and the `lambda_runtime` were
unconditionally compiled into the library. As a result:

- A bare `cargo build` always pulled the full AWS SDK stack and the Lambda
  runtime, even for a purely local build with no cloud dependency.
- The domain core referenced infrastructure directly, so there was no way to
  construct or exercise the write / build / query runtime without AWS types in
  the dependency graph.
- Local deployment (SQLite-backed durable events, build jobs, and active-release
  coordination — see `CONTEXT.md`) could not be built or shipped as an
  AWS-free artifact.

We wanted a build where AWS is one adapter among others, not a compile-time
requirement, while keeping the existing AWS/Lambda deployment path byte-for-byte
unchanged.

## Decision

### Cargo feature model

The crate exposes five features (see `Cargo.toml`), with the AWS-free `local`
profile as the default:

```toml
default = ["local"]
local  = ["server"]
server = ["dep:axum"]
aws    = ["server", "dep:aws-config", "dep:aws-sdk-s3", "dep:aws-sdk-sqs"]
lambda = ["aws", "dep:lambda_runtime"]
ltembed = ["dep:ltembed"]
```

- `local` — the default; an AWS-free build that pulls in the long-running HTTP
  server layer (`server` → `axum`) but no cloud SDK.
- `server` — the shared axum HTTP layer, used by both `local` and `aws`.
- `aws` — adds the AWS SDK adapters on top of `server`.
- `lambda` — `aws` plus the Lambda runtime; the only profile that produces the
  Lambda handler binaries.
- `ltembed` — optional real-embeddings engine, orthogonal to the runtime split.

### Provider-neutral contract families

The domain core depends only on provider-neutral traits, gathered behind the
facade `src/contracts.rs`. There are four contract families, each with a local
and an AWS implementation:

| Family | Contract(s) | Local impl | AWS impl |
| --- | --- | --- | --- |
| document events | `WalStorage` | `LocalFsWalStorage` | `AwsS3WalStorage` |
| build jobs | `BuildQueue` (producer) + `BuildJobSource` (consumer) | `LocalFsBuildQueue` | `AwsSqsBuildQueue` + `SqsBuildJobSource` |
| artifact access | `PublishStorage` (read/write) + `ArtifactSync` (query-side download) | `LocalFsPublishStorage` + `NoopArtifactSync` | `AwsPublishStorage` + `S3ArtifactSync` |
| active-release coordination | `ManifestStore` | `LocalManifestStore` | `FixedManifestStore` (in-memory) |

`BuildJobSource` and `ArtifactSync` are the two consumer-side contracts added by
#107 to close the gap that previously forced the worker loop and the query-side
sync to touch SQS/S3 directly. AWS adapters live under `#[cfg(feature = "aws")]`;
the local implementations carry no infrastructure types.

## Consequences

- **Bare `cargo build` is AWS-free.** The default `local` profile compiles
  without `aws-config`, `aws-sdk-s3`, `aws-sdk-sqs`, or `lambda_runtime` in its
  dependency graph.
- **Every AWS/Lambda command must name its profile.** The AWS/Lambda binaries are
  gated by `required-features`, so they only build under an explicit profile:
  - Lambda binaries (`query_lambda`, `write_lambda`, `index_builder_lambda`)
    require `--features lambda`.
  - Server + offline binaries (`query_server`, `write_server`,
    `index_builder_server`, `turbo_index_builder`) require `--features aws`.
  - A bare `cargo build` (local) produces **no** AWS/Lambda binary.
- **Local server binaries are deferred to #108.** The AWS-free, SQLite-backed
  local server binaries are not shipped by #107. #107 proves the local runtime
  *constructs* via `tests/runtime_local_test.rs` (alongside
  `tests/runtime_aws_test.rs`); shipping runnable local server binaries is #108's
  scope.
- **The existing AWS/Lambda deployment path is unchanged.** Adapter public
  signatures and the Lambda handler binaries are byte-for-byte the same under
  `--features lambda` / `--features aws`.

## Guard invariant

CI enforces the AWS-free local graph in the `feature-matrix` job
(`.github/workflows/ci.yml`). After building the local profile, it asserts that
none of the four AWS/Lambda crates appear in the local dependency graph:

```bash
cargo build --no-default-features --features local
for pkg in aws-config aws-sdk-s3 aws-sdk-sqs lambda_runtime; do
  if cargo tree --no-default-features --features local -i "$pkg" >/dev/null 2>&1; then
    echo "::error::$pkg leaked into the local build graph"; exit 1
  fi
done
```

`cargo tree ... -i <pkg>` succeeds only when `<pkg>` is an inverted-dependency of
the local build; any success is a leak and fails the job. The same job also
builds and tests the `aws` profile and builds the three `lambda` binaries, so a
regression in any profile is caught.
