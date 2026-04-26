# TurboQuant Searcher Design

## Goal

Close issue `#73` by finishing `TurboQuantSearcher` as a synchronous static-index retriever with strong correctness evidence for the TurboQuant scoring formula, deterministic top-K behavior, and benchmark/recall coverage that matches the intent of the issue without expanding scope into query-router architecture changes.

## Status Quo

The repository already contains most of the structural implementation expected by `#73`:

- `src/query/turbo_searcher.rs` already defines `TurboQuantSearcher`
- the current search path already encodes the query once, scans the index in parallel with `rayon`, and maintains bounded top-K results with a heap
- `src/index/turbo_codec.rs` already contains query encoding and scalar score computation against stored TurboQuant records
- `#72` introduced the typed 512d mmap record layout that `#73` depends on

What is still incomplete is not the existence of a searcher, but proving and tightening its behavior against the issue acceptance criteria:

- score agreement against known reference values
- term-by-term coverage of the scoring formula
- recall comparison against exact dot-product ranking
- benchmark coverage in the new typed 512d layout
- cleanup of any remaining raw byte-oriented hot-path assumptions that are no longer needed after `#72`

## User Decision

For issue `#73`, the chosen direction is option 1:

- keep `TurboQuantSearcher` synchronous
- do not change `QueryRouter` or the repo-wide retriever traits
- focus this issue on issue-oriented closure rather than broader API redesign

That means the issue sketch's async signature and trait-symmetry note are treated as future-facing ideas, not requirements to force into this PR.

## Recommended Approach

Finish `TurboQuantSearcher` as a self-contained synchronous searcher and close the issue with correctness and performance evidence.

This work should do four things:

1. keep the existing public searcher shape essentially intact
2. make the typed 512d record path the clear production hot path
3. add targeted tests for formula correctness, ranking behavior, and recall
4. replace the old benchmark smoke setup with a typed 512d benchmark-oriented harness

This keeps the scope aligned with the current codebase while still satisfying the substance of `#73`.

## Alternatives Considered

### Option 1: Issue-oriented closure on the current sync API (recommended)

Keep `TurboQuantSearcher` synchronous and concentrate on correctness, recall, and benchmark acceptance.

Pros:

- matches the current repository shape
- avoids broad interface churn in `QueryRouter` and retriever traits
- keeps the diff focused on the issue's real missing evidence
- reduces regression risk after the `#72` typed mmap change

Cons:

- does not match the async signature shown in the issue body
- leaves future router symmetry work for a separate issue

### Option 2: Match the issue sketch literally with async and trait integration

Change `TurboQuantSearcher` toward an async API and start integrating it with existing retrieval abstractions.

Pros:

- closer to the issue sketch on paper
- could reduce future integration work if turbo retrieval is promoted into the main query path soon

Cons:

- the current repo retrieval surface is synchronous today
- likely requires changes beyond `TurboQuantSearcher`, including trait and router plumbing
- mixes search-quality validation work with architecture redesign
- increases regression risk and test churn for limited near-term value

### Option 3: Split issue closure and API cleanup into separate increments

Land scoring and benchmark acceptance now, then open a follow-up for retriever symmetry.

Pros:

- keeps each change focused
- respects the actual architecture of the codebase

Cons:

- requires explicit scope management across issues or PRs
- may feel less tidy if the team expected one issue to cover both concerns

Given the current repository shape and the user's direction, option 1 is the best fit.

## Architecture

### Searcher boundary

`TurboQuantSearcher` should remain a leaf static retriever with this responsibility split:

- validate query inputs
- encode the query once into TurboQuant form
- iterate the typed mmap records in parallel
- compute a score per record
- maintain top-K candidates without full sorting
- materialize `SearchResult` values using static metadata and text

It should not own:

- manifest loading
- hybrid fusion
- router orchestration
- embedding generation
- async runtime management

Those concerns belong elsewhere in the repository and are out of scope for `#73`.

### Hot path shape

The intended production flow is:

1. `search(query_embedding, top_k)` validates dimensions and finite values
2. the query is encoded once with `encode_vector(...)`
3. the searcher scans typed mmap records with `rayon`
4. each worker computes scores and keeps a bounded heap
5. worker heaps are reduced into a final top-K heap
6. final ranked candidates are turned into `SearchResult` values with `source = SearchSource::Static`

The searcher should prefer typed record access for the production path. Raw byte slicing should not remain in the hot loop unless a typed alternative would introduce a real regression or correctness risk.

### Score computation boundary

The production scorer should be clearly oriented around the typed 512d record layout introduced by `#72`.

The governing formula remains:

```text
score = <y, x_tilde_mse> + gamma * <y, S^T * sign(qjl)>
```

Where:

- `y` is the float32 query embedding
- `x_tilde_mse` is reconstructed online from `idx` and `centroids`
- `gamma` is the stored residual norm
- `S` is the projection matrix
- `sign(qjl)` decodes the bit-packed sign vector into +/-1

For this issue, the scalar typed implementation is the correctness baseline. SIMD or popcount acceleration is optional, not mandatory. If the optimization can be added with a small, well-verified diff, it is acceptable. If it complicates correctness or testability, it should be deferred.

## Error Handling Design

No broad error model changes are needed.

Rules:

- invalid query shape or `top_k` remains `SearchError::Validation`
- encoding or scoring failures remain wrapped as `SearchError::Execution`
- malformed assets or layout mismatches continue to surface through the existing asset/index error paths

This issue should improve precision of test coverage, not redesign error taxonomy.

## Testing Strategy

Issue `#73` should close with three layers of evidence.

### 1. Formula correctness tests

Strengthen unit coverage around `src/index/turbo_codec.rs` so the formula is validated in parts, not only through end-to-end ranking.

Tests should verify:

- centroid reconstruction term behaves as expected on known vectors
- QJL sign term behaves as expected on known projected signs
- `gamma` scaling changes only the residual contribution
- whole-score agreement against known expected results within `< 0.01`

The point is to make failures diagnosable when one part of the formula drifts.

### 2. Searcher behavior tests

Strengthen `tests/turbo_searcher_test.rs` for the synchronous searcher contract.

Tests should verify:

- returned results are marked as `SearchSource::Static`
- corpus-type mapping remains correct
- stable tie-breaking remains deterministic
- bounded top-K behavior does not leak lower-ranked hits
- invalid dimensions or invalid `top_k` are rejected cleanly

### 3. Recall test

Add a deterministic synthetic 512d corpus and compare TurboQuant top-K against exact dot-product top-K.

Requirements:

- fixed seed
- deterministic corpus generation
- deterministic query generation
- exact baseline computed directly from float32 vectors
- recall threshold asserted at `> 90%` on the chosen dataset

The test should be sized for repeatable local and CI execution rather than for maximum realism.

### 4. Benchmark harness

Update `tests/turbo_searcher_benchmark_test.rs` so it reflects the typed 512d layout instead of the old 4d fixture.

The benchmark harness should:

- remain ignored by default
- build a deterministic typed static index
- exercise linear scan behavior on a materially larger corpus than the functional tests
- report elapsed time for the search call

The repository's normal local or CI environment cannot guarantee the issue's exact Lambda-vCPU target. The benchmark should therefore be treated as a reproducible harness and evidence source, not as a universally enforced timing gate in ordinary test runs.

## File Changes

Expected files for this issue:

- modify `src/query/turbo_searcher.rs`
- modify `src/index/turbo_codec.rs`
- modify `tests/turbo_searcher_test.rs`
- modify `tests/turbo_codec_test.rs`
- modify `tests/turbo_searcher_benchmark_test.rs`

Possible additional files:

- a new recall-oriented test file if separating that coverage is cleaner than expanding the existing searcher test file

## Non-Goals

This issue should not do the following:

- no async `TurboQuantSearcher::search`
- no `VectorRetriever` or `QueryRouter` trait changes
- no production router wiring to select TurboQuant retrieval
- no large query-module refactor unrelated to turbo scoring correctness
- no premature abstraction layer for interchangeable retriever backends

## Why This Boundary Fits The Repo

The current codebase has a synchronous retrieval interface in `src/query/router.rs`, and `TurboQuantSearcher` is not yet wired into that path. Meanwhile, the core TurboQuant search loop already exists.

That means the highest-value work in `#73` is to make the existing searcher correct, measurable, and tightly tested after the typed mmap transition in `#72`.

Keeping `#73` focused on scoring and search acceptance avoids conflating two different kinds of work:

- proving the TurboQuant algorithm path is correct and performant enough to be credible
- redesigning the query abstraction layer for future integration

Separating those concerns produces a smaller, safer change and leaves any future router integration to a follow-up issue that can be designed on its own merits.
