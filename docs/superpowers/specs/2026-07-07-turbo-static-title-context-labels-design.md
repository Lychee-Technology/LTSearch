# TurboQuant: static title context labels (#101)

- **Issue:** [#101](https://github.com/Lychee-Technology/LTSearch/issues/101) — Enrich static index with document titles for LLM context labels
- **Parent epic:** #71 (related: #91 covered the `include_metadata` axis; static title is an independent structural gap)
- **Date:** 2026-07-07
- **Status:** approved design

## Problem

Design doc §6 defines the LLM context format as `[法规/合同 #1] <title>`. The
**dynamic** chains (keyword/vector) populate `citation.title` from document
metadata via `Citation::from_metadata`, and #91 preserved it even under
`include_metadata=false`.

The **static Turbo chain** cannot: `SearchResult.citation` is hardcoded to
`None` (`src/query/turbo_searcher.rs:93`) because the static index format
`MetaRecord` (`src/index/meta.rs`) stores only `doc_id / corpus_type /
text_offset / text_len` — there is no title field. Static chunks therefore
render as the bare `[法规 #1]` (`ContextBuilder` degrades gracefully:
`chunk_block` omits the title when it is `None`).

## Goal

Carry a per-chunk title through the static index format so static chunks render
`[法规 #1] 民法典`. This is an **index-format change requiring a full rebuild of
the static index**.

Non-goals: changing the dynamic chains, the `ContextBuilder` rendering, or the
`filter.rs` strip logic — all already consume `citation.title` and need no
change.

## Key decisions

1. **Title storage: separate variable-length blob.** Add
   `turbo_static_title.bin`, addressed by new `title_offset/title_len` fields in
   `MetaRecord`, mirroring the existing `turbo_static_text.bin` pattern. Chosen
   over reusing the text blob for symmetry and clarity; the marginal cost is one
   extra file.

2. **Surface title via `Citation`.** `turbo_searcher` populates
   `SearchResult.citation` with a title. `citation.title` is already the
   canonical source-label channel: `ContextBuilder::chunk_block` reads it, and
   `strip_metadata` (`filter.rs`) deliberately clears `metadata` while
   **preserving** `citation` so labels survive `include_metadata=false`. A new
   parallel `SearchResult.title` field was rejected — it would fork that channel
   and touch the freshly-finalized (#100) context contract and `filter.rs`.

3. **Version bump, fail closed.** Bump `TURBO_VERSION` 1 → 2. Old v1 images are
   rejected loudly by the existing `TurboHeader::from_bytes` version check. No
   dual-load compatibility path — the issue mandates a full rebuild.

4. **Title source key: `metadata["title"]`.** The builder extracts the title
   from each chunk's `metadata` map under the `"title"` key — the same key the
   dynamic chain reads via `Citation::from_metadata`. `StaticSourceLine` already
   parses `metadata`; the builder currently ignores it.

## Changes by component

### `src/index/meta.rs`
- Add fields to `MetaRecord`: `title_offset: u64`, `title_len: u32`.
- `META_RECORD_SIZE` 32 → 40. **Field order matters:** appending the fields at
  the end yields 48B (alignment padding around the trailing `u64`). Reorder so
  the three `u64`s are grouped to pack to exactly 40B under `repr(C)`:
  ```
  #[repr(C)]
  struct MetaRecord {
      doc_id: u64,        //  0..8
      text_offset: u64,   //  8..16
      title_offset: u64,  // 16..24
      text_len: u32,      // 24..28
      title_len: u32,     // 28..32
      corpus_type: u8,    // 32..33
      _pad: [u8; 7],      // 33..40 (explicit tail pad)
  }
  ```
  Update the builder's field initializers and any `MetaRecord { .. }`
  constructions to match the new field set (order is irrelevant at
  construction sites, only in the `repr(C)` declaration).
- Add `title_from_blob(&self, blob: &[u8]) -> Option<&str>`, returning `None`
  when `title_len == 0`, otherwise the UTF-8 slice `[title_offset,
  title_offset+title_len)`.

### `src/index/header.rs`
- `TURBO_VERSION` 1 → 2.
- Rename `KnownRecordLayout::V1Dim512` → `V2Dim512` (updates `from_header`,
  `record_size`, and all match arms in `mmap_index.rs` / `turbo_searcher.rs`).
  `from_header` accepts `(2, 512)`; `(1, _)` already errors at the version check
  in `from_bytes` with `UnsupportedVersion { version: 1 }`.

### `src/index/static_builder.rs`
- Accumulate a `turbo_static_title` blob alongside `turbo_static_text`.
- For each chunk: read `chunk.metadata.get("title").and_then(Value::as_str)`.
  When present, append its bytes to the title blob and record
  `title_offset/title_len`; when absent/empty, `title_len = 0` (offset may be
  the current blob length).
- Populate the new `MetaRecord` fields.
- `write_static_files` writes `turbo_static_title.bin`.

### `src/index/mmap_index.rs`
- `load`: `mmap` `turbo_static_title.bin` into a new `title_mmap` field. Missing
  file is an error (v2 images always have it).
- Add `title(&self, index: u64) -> Option<&str>` delegating to
  `MetaRecord::title_from_blob(&self.title_mmap)`.
- Add `title_blob(&self) -> &[u8]` for parity with `text_blob` (used in tests).

### `src/query/turbo_searcher.rs`
- Carry the title into `RankedResult` (as `Option<String>`) alongside `text`.
- In the result map, build the citation:
  ```
  citation: title.map(|t| Citation {
      title: Some(t),
      resource_id: doc_id.to_string(),
      source_type: <corpus label for candidate.corpus_type>,
      source_ref: doc_id.to_string(),
      url: None,
  })
  ```
  When no title exists, `citation: None` — identical to today's bare-label
  degradation. Reuse the existing corpus→label mapping (as used by
  `corpus_type_label` in `context_builder.rs`) for `source_type`.

## Data flow

```
static source line { doc_id, text, metadata{title} }
  └─ StaticIndexBuilder
       ├─ text  → turbo_static_text.bin  (text_offset/text_len)
       └─ title → turbo_static_title.bin (title_offset/title_len)   [NEW]
             └─ MetaRecord (40B) in turbo_static_meta.bin
  └─ MmapIndex::load  → title_mmap                                  [NEW]
  └─ turbo_searcher   → SearchResult.citation = Some{title,...}     [NEW]
  └─ filter::strip_metadata  (citation preserved — unchanged)
  └─ ContextBuilder::chunk_block  → "[法规 #1] 民法典" (unchanged)
```

## Error handling & compatibility

- Loading a v1 image under v2 code: `TurboHeader::from_bytes` returns
  `UnsupportedVersion { version: 1 }` → surfaced as `MmapIndexError::Header`.
  Clear, fail-closed. (Acceptance: "旧 image 加载有明确错误".)
- Missing `turbo_static_title.bin` in a directory claiming v2: `mmap_file`
  error, consistent with how a missing `turbo_static_text.bin` is handled today.
- Malformed UTF-8 in the title blob: `title_from_blob` follows the same
  convention as `text_from_blob` (which `expect`s valid UTF-8, guaranteed by the
  builder writing `&str` bytes).

## Testing

- **`turbo_meta_test.rs`**: `MetaRecord` is 40B; `title_from_blob` round-trips a
  title and returns `None` for `title_len == 0`.
- **`static_index_builder_test.rs`**: builder writes `turbo_static_title.bin`;
  chunk with `metadata.title` records nonzero `title_len`; chunk without title
  records `title_len == 0`.
- **`mmap_index_test.rs`**: loads the title blob; `title(i)` returns the built
  title / `None`; loading a synthetic v1 image errors with the version message.
- **`turbo_searcher_test.rs`**: a titled static chunk yields
  `citation.title == Some(...)`; an untitled chunk yields `citation == None`.
- **End-to-end** (`query_flow_test.rs` / `query_build_flow_test.rs`): a static
  chunk's title appears in the assembled context as `[法规 #1] <title>`.
  (Acceptance: title appears in assembled context.)

## Acceptance criteria (from #101)

- [ ] Static chunk's `citation.title` available in search results.
- [ ] `ContextBuilder` renders `[法规 #1] <title>` for static chunks.
- [ ] Index format versioned; old images give a clear error (v1 rejected).
- [ ] E2E test: static chunk title appears in assembled context.

## Rollout

Index-format change → the static index must be fully rebuilt and republished;
old images will not load under v2 code. No in-place migration.
