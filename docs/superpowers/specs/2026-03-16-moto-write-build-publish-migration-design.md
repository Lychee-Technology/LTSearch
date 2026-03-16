## Title

Replace LocalStack with Moto for write-build-publish integration coverage

## Context

Issue `#21` introduced S3- and SQS-backed integration coverage for the write-build-publish flow using LocalStack. That work is now merged, but the desired local AWS mock backend has changed: the repository should use `motoserver/moto` instead of LocalStack.

The existing implementation already has the right high-level boundaries:

- AWS SDK-backed adapters live under `src/adapters/`
- the integration harness lives in `tests/write_build_publish_test.rs`
- CI starts a Docker Compose service before running `cargo test`
- docs and planning artifacts describe the LocalStack-based approach

The goal of this follow-up is not to redesign the flow. It is to switch the infrastructure mock provider while preserving the same adapter boundaries, test coverage, and end-to-end semantics.

## Goal

Replace LocalStack with Moto across the write-build-publish integration surface so that:

1. the integration tests run against Moto-backed S3 and SQS endpoints
2. CI starts Moto instead of LocalStack before `cargo test`
3. docs and design artifacts describe the current Moto-backed setup instead of the old LocalStack-backed one

## Scope

In scope:

- replace the compose service used for S3/SQS integration tests
- update CI workflow commands to use the Moto compose file
- update the Rust integration harness endpoint and naming
- rename LocalStack-specific test and harness identifiers to Moto-specific ones
- update current operational docs/specs/plans to reflect the Moto-based setup

Out of scope:

- changing the AWS SDK adapter interfaces
- redesigning the async trait refactor from `#21`
- altering the end-to-end behavior being verified
- changing issue `#22` query Lambda work

## Approaches Considered

### 1. Swap the backend provider only, keep adapter/test semantics the same (recommended)

Keep the current AWS SDK adapters, keep the integration harness design, and replace only the underlying mock AWS service plus its endpointing and related documentation.

Pros:

- preserves the investment already made in `#21`
- keeps changes focused on infrastructure setup and naming
- minimizes risk of accidental behavior drift in write/build/publish logic

Cons:

- still requires a repo-wide pass to clean up LocalStack-specific naming and docs

### 2. Introduce a generalized mock-AWS abstraction layer first

Add an intermediate provider/config abstraction so the tests can swap between LocalStack and Moto.

Pros:

- could make future provider swaps easier

Cons:

- adds complexity without a demonstrated need for multiple providers
- expands the migration into a design exercise instead of a focused replacement

### 3. Change only CI and leave the test/document names alone

Start Moto in CI but keep code and docs referring to LocalStack.

Pros:

- smallest immediate diff

Cons:

- leaves the repo internally inconsistent
- creates long-term confusion for engineers reading tests and docs

Given the requested scope, approach 1 is the right fit.

## Recommended Design

Adopt approach 1.

Treat this work as an infrastructure-provider migration, not a behavior change. The adapters remain AWS SDK-based and continue targeting S3 and SQS APIs. The integration test continues validating:

- WAL append to S3
- queue enqueue to SQS
- single-batch consume/build/publish orchestration
- published manifest and `_head` activation

What changes is the service used to emulate those AWS APIs locally and in CI.

## Architecture

### Runtime infrastructure

Replace the LocalStack compose file with a Moto compose file:

- use `motoserver/moto`
- use Moto’s documented server port, `5000`
- access the service through `http://localhost:5000`

The tests should keep using `localhost` instead of `127.0.0.1`, matching Moto’s documented expectations.

### Harness and naming

`tests/write_build_publish_test.rs` should be updated so its naming reflects the actual provider in use.

Examples:

- `LocalstackHarness` -> `MotoHarness`
- `localstack_smoke_test_can_create_bucket_and_queue` -> `moto_smoke_test_can_create_bucket_and_queue`
- `*_against_localstack` -> `*_against_moto`

The harness behavior itself should remain the same:

1. create unique bucket and queue names
2. build AWS SDK clients for S3 and SQS
3. wait until bucket/queue creation succeeds
4. exercise the end-to-end write-build-publish flow

### CI workflow

The CI `test` job should continue to:

1. start the Docker-based AWS mock service
2. run `cargo test`
3. always tear the service down

Only the compose filename and provider change. The workflow structure remains the same.

### Documentation strategy

The repository should describe the current implementation truthfully.

That means updating:

- active design docs
- implementation plans
- verification commands
- references to the compose filename and endpoint port

To avoid unnecessary path churn, historical spec/plan filenames may remain unchanged even if they still contain `localstack` in the filename. Their contents, however, should explain that the current implementation now uses Moto.

## Data Flow

The end-to-end data flow does not change:

1. `WriteApi` validates and appends WAL records
2. WAL bytes are stored in S3 through the AWS SDK-backed adapter
3. batch metadata is enqueued to SQS through the AWS SDK-backed adapter
4. the integration harness receives one queue message
5. WAL records are read back from S3
6. `LocalIndexBuilder` builds artifacts locally
7. `IndexPublisher` uploads artifacts and updates `_head`
8. the test verifies manifest and `_head` state

Only the backing emulated AWS server changes from LocalStack to Moto.

## Error Handling

Failure visibility should remain stage-oriented as in the current integration design:

- readiness/bootstrap failures should identify bucket or queue setup problems
- WAL failures should identify S3 read/write issues
- queue failures should identify SQS send/receive/delete issues
- publish failures should identify manifest upload or `_head` update issues

The migration should not collapse these into generic provider-switch errors.

## Test Design

The migration should preserve both kinds of tests already present in `tests/write_build_publish_test.rs`:

- real Moto-backed S3/SQS integration tests
- narrow mock HTTP tests for error-propagation behavior

The mock HTTP tests are not tied to LocalStack and should remain unchanged except where naming needs consistency.

Primary verification should include:

1. CI workflow guard test
2. the focused write-build-publish integration suite
3. the full Rust test suite

## Tooling and Environment

### Compose

Use a Moto compose file rather than the old LocalStack file. Recommended name:

- `docker-compose.moto.yml`

Recommended command flow:

1. `docker compose -f docker-compose.moto.yml up -d`
2. `cargo test --test write_build_publish_test -- --nocapture`
3. `docker compose -f docker-compose.moto.yml down -v`

### Endpointing

Use:

- endpoint: `http://localhost:5000`
- fixed test region
- static test credentials
- path-style S3 access where needed

## Files

Expected runtime-sensitive file changes:

- `.github/workflows/ci.yml`
- `tests/test_ci_workflow.py`
- `tests/write_build_publish_test.rs`
- `docker-compose.moto.yml`
- removal of `docker-compose.localstack.yml`

Expected doc updates:

- `docs/design.md`
- `docs/superpowers/specs/2026-03-15-localstack-write-build-publish-design.md`
- `docs/superpowers/plans/2026-03-15-localstack-write-build-publish.md`
- `docs/superpowers/plans/2026-03-14-write-build-publish-mvp.md`
- `docs/superpowers/plans/2026-03-14-lambda-verification-mvp.md`

## Verification Plan

Primary verification for the migration:

1. `python3 -B tests/test_ci_workflow.py`
2. `docker compose -f docker-compose.moto.yml up -d`
3. `cargo test --test write_build_publish_test -- --nocapture`
4. `docker compose -f docker-compose.moto.yml down -v`
5. `cargo test`

## Acceptance Criteria Mapping

This migration is complete when:

- the write-build-publish integration tests pass against Moto-backed S3/SQS
- CI starts Moto rather than LocalStack before `cargo test`
- test names and harness names no longer claim to use LocalStack
- current docs and command examples describe Moto rather than LocalStack

## Future Follow-up

If the repository later needs to support multiple local AWS mock providers, that should be treated as a separate design effort. This migration intentionally avoids adding a generalized provider abstraction because the current need is simply to make Moto the one supported local AWS mock backend.
