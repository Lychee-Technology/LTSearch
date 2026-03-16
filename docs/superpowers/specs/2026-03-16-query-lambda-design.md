# Query Lambda Design

## Goal

Expose the existing query path through a thin AWS Lambda entrypoint that accepts a plain `SearchRequest`, delegates to the in-process query stack, and returns either a `SearchResponse` or a clear typed error payload.

## Status Quo

The repository already contains the core query behavior in reusable library modules:

- `src/query/router.rs` validates the request, loads the active manifest, runs hybrid retrieval, applies filtering, and returns `SearchResponse`
- `src/models/search.rs` already defines the request and response contract, including `index_version`
- `src/storage/manifest_store.rs`, `src/query/keyword_searcher.rs`, and `src/query/vector_searcher.rs` already provide the concrete local-manifest and retrieval implementations used by the query path

What is missing for issue `#22` is only the runtime adapter that makes this code invocable as a Lambda function. The current binary surface does not provide that entrypoint; `src/bin/ltsearch.rs` is empty.

## User Decision

For issue `#22`, the Lambda boundary should stay library-native:

- input event: plain `SearchRequest`
- success output: plain `SearchResponse`
- failure output: typed error envelope

This issue does not introduce API Gateway request or response wrappers.

## Recommended Approach

Create a dedicated binary at `src/bin/query_lambda.rs` and keep it intentionally thin.

The binary should do only four things:

1. receive and deserialize the Lambda event into `SearchRequest`
2. construct concrete query dependencies from local configuration and artifact paths
3. call `QueryRouter`
4. translate failures into a small, explicit error response shape

All search semantics remain in the existing library modules. The Lambda binary is an adapter, not a second implementation of the query path.

## Alternatives Considered

### Option 1: Dedicated Lambda binary (recommended)

Create `src/bin/query_lambda.rs` as a standalone runtime adapter.

Pros:

- preserves a clean separation between runtime concerns and query logic
- matches the prior lambda-verification plan already checked into the repo
- gives future issues `#23` and `#24` a consistent binary-per-flow pattern

Cons:

- adds a new binary target and a small amount of runtime-specific boilerplate

### Option 2: Reuse `src/bin/ltsearch.rs`

Turn the existing empty binary into the Lambda entrypoint.

Pros:

- one less file

Cons:

- muddies the role of a generic crate-level binary
- makes future CLI or other entrypoint work harder to separate from Lambda concerns
- provides less explicit structure than the existing repo plans expect

### Option 3: Introduce a larger shared runtime adapter layer first

Create a new library module for Lambda request handling before adding the binary.

Pros:

- may provide a reusable pattern for later Lambda binaries

Cons:

- adds abstraction before the concrete needs are proven
- increases scope for an issue whose acceptance criteria only require a thin Lambda entrypoint

Given the current repository shape and issue scope, Option 1 is the best fit.

## Architecture

### Binary boundary

The new query Lambda binary should live at:

- `src/bin/query_lambda.rs`

It should own Lambda-runtime concerns only:

- runtime startup
- event decoding
- dependency construction
- response/error mapping

It should not own:

- query validation rules
- manifest loading behavior
- retrieval logic
- ranking behavior
- filtering logic

Those behaviors already belong to the library modules and should remain there.

### Dependency graph

The binary should assemble the existing concrete query stack roughly like this:

1. read local configuration for artifact/cache paths and query dependencies
2. create a `LocalManifestStore`
3. create a concrete embedding generator
4. create a `KeywordSearcher`
5. create a `VectorSearcher`
6. create a `QueryRouter`
7. invoke `search(&SearchRequest)`

The Lambda handler should be the outermost shell around that stack.

This issue assumes only minimal bootstrap wiring around dependencies that already have a clear concrete source in the repository. If a required query-time dependency does not yet have a concrete production implementation, issue `#22` may introduce only the smallest adapter needed to construct the router, but it should not expand into a broader configuration-system or provider-design project.

At the time of this spec, that caveat matters most for the embedding generator and runtime configuration surface:

- `EmbeddingGenerator` exists as a trait, but no concrete production query-time generator is clearly established yet in the current repo surface
- `src/config.rs` currently exposes only a placeholder `AppConfig`, so `#22` should not attempt to design a large general-purpose Lambda configuration system

The resulting scope rule is:

- acceptable in `#22`: a minimal, query-specific bootstrap path sufficient to construct the handler dependencies
- not acceptable in `#22`: broad config redesign, multi-runtime abstraction, or a generalized dependency container

### Runtime shape

The handler contract should remain transport-agnostic:

- request payload is `SearchRequest`
- success payload is `SearchResponse`
- application-level failures are returned as typed data, not surfaced as Lambda invocation failures

That means the success path should preserve the plain `SearchResponse` payload exactly as chosen for this issue, while the failure path should serialize a typed error envelope rather than collapsing into an unstructured runtime error string.

This keeps the binary easy to test and avoids prematurely coupling it to HTTP-specific conventions.

## Error Handling Design

The Lambda response model should distinguish client-caused failures from service-caused failures.

Recommended error envelope shape:

- `error_type`: short stable category such as `validation_error` or `execution_error`
- `message`: human-readable message derived from the underlying error

Mapping rules:

- `SearchError::Validation(_)` -> client-style error envelope
- `SearchError::Execution { .. }` -> service-style error envelope
- dependency bootstrap/configuration failures -> service-style error envelope

This issue does not need HTTP status codes because the chosen boundary is not API Gateway shaped. The important requirement is preserving a clear machine-readable distinction between bad input and internal failure.

Expected bootstrap/configuration problems that can be detected and reported intentionally by the handler should map to the service-style error envelope. True Lambda invocation failure should be reserved only for catastrophic runtime conditions that prevent the handler from producing any intentional response at all.

## Data Flow

The intended request path is:

1. Lambda runtime receives a JSON event
2. event is deserialized into `SearchRequest`
3. handler constructs the concrete query stack
4. handler calls `QueryRouter::search`
5. on success, the handler returns plain `SearchResponse`
6. on failure, the handler returns a typed error envelope

Because `QueryRouter` already sets `index_version` from the active manifest head, the Lambda path inherits that behavior automatically and should not duplicate it.

## Testing Strategy

Issue `#22` should focus on handler-level tests, not full infrastructure integration.

Create:

- `tests/query_lambda_test.rs`

The tests should verify:

1. valid request -> success response with expected `index_version`
2. invalid request -> validation-style error envelope
3. execution failure -> service-style error envelope

The preferred seam is to keep most behavior testable without needing a deployed Lambda runtime or API Gateway wrapper. Tests may call a small extracted handler function directly as long as the production binary still uses the same path.

Issue `#22` should not add LocalStack-backed query integration tests; that belongs to `#25`.

## File Changes

Expected files for this issue:

- create `src/bin/query_lambda.rs`
- create `tests/query_lambda_test.rs`
- likely modify `Cargo.toml` to add Lambda runtime dependencies

Possible but not guaranteed:

- a small shared serializable error type if the binary needs one and no suitable existing model exists

Non-goals for this issue:

- no API Gateway wrapper types
- no write Lambda or builder Lambda work
- no end-to-end query verification harness
- no README/runbook updates

## Why This Boundary Fits The Repo

This repository has already converged on a design where the query core lives in reusable library code and the lambda-verification work is split into separate issues for thin binaries plus later integration coverage.

Keeping `#22` small and explicit has three benefits:

- it preserves the current module boundaries around `QueryRouter` and the searchers
- it keeps future Lambda issues parallel in structure
- it leaves the broader end-to-end verification work to `#25`, where it belongs

## Acceptance Criteria Mapping

Issue `#22` acceptance criteria say:

- Lambda binary delegates most logic to library modules
- successful responses include `index_version`
- validation and service failures map to clear responses

This design satisfies them by:

- placing only runtime adapter code in `src/bin/query_lambda.rs`
- reusing `SearchResponse`, which already includes `index_version`
- introducing an explicit typed error envelope with validation vs execution categories

## Out Of Scope

This design intentionally excludes:

- API Gateway compatibility concerns
- HTTP status code mapping
- multi-Lambda fan-out or RPC between query components
- deployment packaging details beyond adding the binary target
- end-to-end query verification against seeded data

Those concerns belong to later issues unless a failing test proves they are strictly required for `#22`.
