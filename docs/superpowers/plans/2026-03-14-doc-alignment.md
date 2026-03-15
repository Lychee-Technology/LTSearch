# Doc Alignment Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Align `docs/arch.md`, `docs/design.md`, and the open GitHub issues so the project has one coherent, implementation-ready design baseline.

**Architecture:** Treat `docs/arch.md` as the high-level system scope document and `docs/design.md` as the executable design spec. Resolve cross-document conflicts first, then fill missing contracts, then tighten MVP boundaries and extension points. Keep the first pass focused on documentation clarity, not feature expansion.

**Tech Stack:** Markdown docs, GitHub Issues (`gh`), Rust-oriented interface/design conventions

---

## Scope

This plan covers one subsystem: design/documentation alignment for the serverless hybrid search engine.

Relevant issues:
- `#1` Query-plane deployment model and component boundaries
- `#2` Missing API and model types
- `#3` Delete semantics and WAL event schema
- `#4` Shard-aware version metadata and publish manifest
- `#5` Tenant isolation and filter semantics
- `#6` Embedding model and vector dimension contract
- `#7` Reranker extension point
- `#8` S3 layout, `_head`, and metadata object formats

## File Structure

- Create: `docs/superpowers/plans/2026-03-14-doc-alignment.md`
  - Execution plan for aligning docs and issues
- Modify: `docs/arch.md`
  - High-level architecture, storage layout, MVP boundaries, extension points
- Modify: `docs/design.md`
  - Component contracts, data models, manifests, consistency semantics, formal definitions

## Chunk 1: Lock the system shape

### Task 1: Resolve query-plane deployment model (`#1`)

**Files:**
- Modify: `docs/arch.md`
- Modify: `docs/design.md`
- Review: `https://github.com/Lychee-Technology/LTSearch/issues/1`

- [ ] **Step 1: Mark the current MVP topology decision in the plan notes**

Write down the chosen MVP model before editing docs:

```text
MVP query plane = single Query Lambda containing VectorSearcher, KeywordSearcher, and HybridRanker as in-process modules.
Multi-Lambda fan-out = future evolution only.
```

- [ ] **Step 2: Update the architecture diagram language in `docs/arch.md`**

Replace component names that imply separate deployed Lambdas with wording that reflects one query Lambda and internal search modules.

- [ ] **Step 3: Update the layered diagram and sequence in `docs/design.md`**

Make the diagram and sequence show in-process parallel retrieval rather than Router-to-Lambda RPC.

- [ ] **Step 4: Update component responsibility text**

Ensure `QueryRouter`, `VectorSearcher`, `KeywordSearcher`, and `HybridRanker` describe one consistent deployment boundary.

- [ ] **Step 5: Verify issue `#1` is fully addressed in both docs**

Check that diagrams, prose, and interfaces all tell the same story.

Run: `gh issue view 1`
Expected: the acceptance criteria match the revised docs.

### Task 2: Normalize storage model and publish contract (`#8`)

**Files:**
- Modify: `docs/arch.md`
- Modify: `docs/design.md`
- Review: `https://github.com/Lychee-Technology/LTSearch/issues/8`

- [ ] **Step 1: Define the canonical S3 prefixes**

Write the exact storage contract to use everywhere:

```text
index/
  _head
  versions/{version_id}/manifest.json
lance/
  versions/{version_id}/...
tantivy/
  versions/{version_id}/...
wal/
  YYYY/MM/DD/<segment>.jsonl
docs/
  versions/{version_id}/documents.parquet
```

- [ ] **Step 2: Add `_head` object format to `docs/design.md`**

Document the fields stored in `_head`, for example:

```json
{
  "version_id": 42,
  "manifest_path": "index/versions/42/manifest.json",
  "updated_at": 1710000000000
}
```

- [ ] **Step 3: Update `docs/arch.md` storage layout section**

Turn the example layout into a normative contract, not just a sketch.

- [ ] **Step 4: Cross-check `IndexVersion` and publish flow language**

Make sure data-model references are compatible with the storage layout and `_head` contract.

- [ ] **Step 5: Verify issue `#8` coverage**

Run: `gh issue view 8`
Expected: the revised docs explicitly define layout, `_head`, and manifest responsibilities.

## Chunk 2: Fill missing executable contracts

### Task 3: Define all missing request/response and status types (`#2`)

**Files:**
- Modify: `docs/design.md`
- Review: `https://github.com/Lychee-Technology/LTSearch/issues/2`

- [ ] **Step 1: Add a formal data-model section for missing types**

Add definitions for:

```rust
pub struct SearchResponse { /* results, total_count, latency_ms, index_version */ }
pub struct HealthStatus { /* status, index_version, cache_state */ }
pub enum FilterValue { /* exact/range/in values for MVP */ }
pub struct CacheStats { /* hit/miss/version/bytes */ }
pub struct IngestResponse { /* accepted_count, wal_offset, batch_id */ }
pub struct DeleteResponse { /* accepted_count, wal_offset, batch_id */ }
```

- [ ] **Step 2: Add validation rules for external models**

Document any bounds, required fields, and serialization expectations.

- [ ] **Step 3: Update interfaces and examples to use the new types consistently**

Remove any placeholder references that lack definitions.

- [ ] **Step 4: Check for missing supporting enums/aliases**

If new helper enums are introduced, define them in the same section.

- [ ] **Step 5: Verify issue `#2` coverage**

Run: `gh issue view 2`
Expected: every referenced public type now has a concrete definition.

### Task 4: Define delete semantics and WAL event schema (`#3`)

**Files:**
- Modify: `docs/arch.md`
- Modify: `docs/design.md`
- Review: `https://github.com/Lychee-Technology/LTSearch/issues/3`

- [ ] **Step 1: Choose the event model**

Use one explicit WAL record shape for both upserts and deletes:

```json
{
  "event_id": "evt_123",
  "op": "upsert",
  "doc_id": "doc_001",
  "document": {"text": "...", "metadata": {}},
  "timestamp": 1710000000000
}
```

```json
{
  "event_id": "evt_124",
  "op": "delete",
  "doc_id": "doc_001",
  "timestamp": 1710000001000
}
```

- [ ] **Step 2: State that `WriteAPI` appends WAL before enqueueing**

Update both docs so WAL is clearly a write-path durability log, not an IndexBuilder-owned side effect.

- [ ] **Step 3: Define delete visibility semantics**

Document whether deletes become searchable/non-searchable only after the next published index in MVP.

- [ ] **Step 4: Mention WAL-assisted read-after-write/read-after-delete as future work**

Keep the architecture note, but label it as a non-MVP enhancement.

- [ ] **Step 5: Verify issue `#3` coverage**

Run: `gh issue view 3`
Expected: ingest, delete, WAL, and publish semantics form one end-to-end story.

## Chunk 3: Make scale and scope explicit

### Task 5: Add shard-aware metadata without overcomplicating MVP (`#4`)

**Files:**
- Modify: `docs/arch.md`
- Modify: `docs/design.md`
- Review: `https://github.com/Lychee-Technology/LTSearch/issues/4`

- [ ] **Step 1: Introduce a manifest-level abstraction in `docs/design.md`**

Prefer adding `IndexManifest` over overloading `IndexVersion`:

```rust
pub struct IndexManifest {
    pub version_id: u64,
    pub num_shards: u32,
    pub embedding_dim: u32,
    pub shards: Vec<ShardManifest>,
    pub created_at: i64,
}
```

- [ ] **Step 2: Add `ShardManifest` details**

Include per-shard Lance/Tantivy locations and document counts.

- [ ] **Step 3: Update the sharding discussion in `docs/arch.md`**

State clearly that MVP may run with `num_shards = 1`, while the metadata model supports future fan-out.

- [ ] **Step 4: Align publish/load/cache text with the manifest model**

Any reference to loading or publishing a version should point to manifest-first behavior.

- [ ] **Step 5: Verify issue `#4` coverage**

Run: `gh issue view 4`
Expected: the docs support both single-shard MVP and multi-shard evolution.

### Task 6: Decide MVP tenant model and filter semantics (`#5`)

**Files:**
- Modify: `docs/arch.md`
- Modify: `docs/design.md`
- Review: `https://github.com/Lychee-Technology/LTSearch/issues/5`

- [ ] **Step 1: Pick the MVP scope explicitly**

Default recommendation:

```text
MVP = single-tenant.
Multi-tenant isolation = future extension.
Filters = exact-match metadata filters only.
```

- [ ] **Step 2: Update security/isolation language in `docs/arch.md`**

Keep multi-tenant guidance, but label it as a future deployment pattern if not in MVP.

- [ ] **Step 3: Add filter semantics to `docs/design.md`**

Document supported operators and execution stage, for example:
- exact match on string/boolean/numeric metadata
- post-retrieval filter in MVP
- pushdown filtering as future optimization

- [ ] **Step 4: Reflect the decision in request models**

Avoid adding `tenant_id` to MVP contracts unless it is truly required now.

- [ ] **Step 5: Verify issue `#5` coverage**

Run: `gh issue view 5`
Expected: MVP boundaries for tenancy and filters are unambiguous.

### Task 7: Make embedding dimension a documented contract (`#6`)

**Files:**
- Modify: `docs/design.md`
- Review: `https://github.com/Lychee-Technology/LTSearch/issues/6`

- [ ] **Step 1: Replace hardcoded-global wording with manifest-scoped wording**

Use language like:

```text
MVP default embedding dimension is 768.
The active index manifest records the embedding dimension used by that version.
All query and indexing operations must match the active manifest.
```

- [ ] **Step 2: Update validation rules and preconditions**

Every place that currently says "must be 768-dimensional" should either reference the active manifest or explicitly note the MVP default.

- [ ] **Step 3: Keep external model examples consistent**

Make sure examples, properties, and tests all use the same language.

- [ ] **Step 4: Verify issue `#6` coverage**

Run: `gh issue view 6`
Expected: embedding model flexibility is captured without weakening MVP clarity.

## Chunk 4: Clarify optional extensions and handoff quality

### Task 8: Mark reranking as explicit non-MVP extension (`#7`)

**Files:**
- Modify: `docs/arch.md`
- Modify: `docs/design.md`
- Review: `https://github.com/Lychee-Technology/LTSearch/issues/7`

- [ ] **Step 1: Keep reranking out of the core query path for MVP**

Add a sentence in both docs that reranking is not part of the first implementation.

- [ ] **Step 2: Define the future extension point**

State that reranking, if enabled later, runs after hybrid retrieval on the top-N merged results.

- [ ] **Step 3: Avoid hidden dependencies in interfaces**

Do not add reranker types to MVP component interfaces unless they are optional.

- [ ] **Step 4: Verify issue `#7` coverage**

Run: `gh issue view 7`
Expected: reranker scope is explicit and no longer ambiguous.

### Task 9: Final consistency sweep across both docs and issue set

**Files:**
- Modify: `docs/arch.md`
- Modify: `docs/design.md`

- [ ] **Step 1: Compare terms used in both docs**

Normalize names such as `Query Router`, `QueryRouter`, `IndexVersion`, `IndexManifest`, `WAL`, `head`, and `shard`.

- [ ] **Step 2: Remove statements that contradict MVP decisions**

Check diagrams, examples, constraints, and error handling sections.

- [ ] **Step 3: Re-read the docs in order**

Read `docs/arch.md` first, then `docs/design.md`, to make sure the second feels like a refinement of the first.

- [ ] **Step 4: Update open issues with closing notes or follow-up comments if needed**

Run: `gh issue comment <n> --body "Addressed in docs update on <date>"`
Expected: issue state reflects whether the work is resolved or still pending execution.

- [ ] **Step 5: Commit the aligned documentation changes**

```bash
git add docs/arch.md docs/design.md docs/superpowers/plans/2026-03-14-doc-alignment.md
git commit -m "docs: align architecture and design specifications"
```

## Testing and Verification

- Re-read both docs end to end after edits
- Run: `gh issue list --limit 20`
- Expected: issues `#1` through `#8` still map cleanly to documented decisions
- Run: `git diff -- docs/arch.md docs/design.md docs/superpowers/plans/2026-03-14-doc-alignment.md`
- Expected: diff shows coherent doc-only changes with no unrelated edits

## Notes for Execution

- Keep the first pass DRY and MVP-focused; do not design full multi-tenant or distributed query fan-out unless required by the docs update
- Prefer adding one manifest abstraction instead of expanding many unrelated models
- If a decision is deferred, mark it clearly as future work instead of leaving it implicit

Plan complete and saved to `docs/superpowers/plans/2026-03-14-doc-alignment.md`. Ready to execute?
