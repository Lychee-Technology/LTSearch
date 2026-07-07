# Design: TurboQuant + LanceDB Hybrid Search Architecture

**Date:** 2026-03-26
**Status:** Approved

---

## 1. Overview

This document describes a two-tier hybrid search architecture that separates static corpora (laws, contracts, RFCs) from dynamic per-tenant user data, using different storage and retrieval strategies optimized for each tier's access patterns.

**Core idea:** retrieve `3*K` candidates from each tier in parallel, pass all candidates with source labels to the LLM, and let the LLM filter to the final answer.

---

## 2. Design Constraints

| Constraint | Value |
|---|---|
| Static corpus update frequency | Monthly / quarterly |
| Static corpus scale | 10万 ~ 100万 chunks (~21–208 MB compressed) |
| Static corpus tenancy | Shared across all tenants |
| Dynamic data tenancy | Per-tenant |
| Result presentation | Blended (single unified answer) |
| BM25 keyword search | Dynamic data only |
| TurboQuant index location | Bundled in Lambda Docker image |
| Latency SLA | ≤ 300 ms |

---

## 3. Architecture

```
                        Query
                          │
          ┌───────────────┴───────────────┐
          │                               │
   [Static Path]                   [Dynamic Path]
TurboQuantSearcher              VectorSearcher (LanceDB)
  (image-bundled)              + KeywordSearcher (Tantivy)
          │                               │
      top-3K                          RRF fusion
   source=Static                          │
                                      top-3K
                                   source=Dynamic
          │                               │
          └───────────────┬───────────────┘
                          │
               6K chunks with source labels
                          │
              LLM (system prompt + corpus_weights)
                          │
                      Final answer
```

Both paths execute **fully in parallel**. There is no cross-path score normalization — the LLM is the sole fusion layer.

---

## 4. Static Index: File Layout

Five memory-mapped files, all bundled in the Lambda Docker image:

```
/app/static/
  turbo_static.bin       # 32-byte header + compressed vectors, 208 bytes per record
  turbo_static_meta.bin  # Fixed-size metadata, one 32-byte record per chunk
  turbo_static_text.bin  # Concatenated raw text, variable length
  centroids.bin          # 1D MSE centroid table (4 centroids per dimension)
  projection.bin         # QJL projection matrix S (512 × 512)
```

### `turbo_static.bin` — TurboHeader (32 bytes) + typed records

The file opens with a 32-byte header, validated on every load:

| Field | Type | Size | Description |
|---|---|---|---|
| magic | `[u8; 4]` | 4 B | `TQNT` |
| version | `u32` | 4 B | Currently 1 |
| dim | `u32` | 4 B | Embedding dimension, currently 512 |
| record_count | `u64` | 8 B | Number of records that follow |
| (padding) | — | 12 B | Zero-filled |

`(version, dim)` dispatches to a `KnownRecordLayout` variant: `(1, 512)` →
`V1Dim512`; any other combination fails loading with `UnsupportedLayout`.
Future layouts are additive — existing images keep loading unchanged. Total
file size must equal exactly `32 + record_count × 208`.

### Record: `TurboRecord512` (208 bytes, repr(C), layout `V1Dim512`)

| Field | Type | Size | Description |
|---|---|---|---|
| `doc_id` | `u64` | 8 B | FNV-1a 64-bit hash of the string chunk id |
| `idx` | `[u8; 128]` | 128 B | Stage 1: 2-bit MSE quantization indices (512 dims × 2 bits) |
| `qjl` | `[u8; 64]` | 64 B | Stage 2: QJL residual sign bits (512 dims × 1 bit) |
| `gamma` | `f32` | 4 B | L2 norm of residual vector ‖r‖₂ |
| `_reserved` | `[u8; 4]` | 4 B | Zero-filled; keeps the record size 8-byte aligned |

### `turbo_static_meta.bin` — MetaRecord (32 bytes, repr(C))

| Field | Type | Size | Description |
|---|---|---|---|
| `doc_id` | `u64` | 8 B | Links to TurboRecord512 |
| `corpus_type` | `u8` | 1 B | Enum: Legal=0, Contract=1, RFC=2, ... |
| `_pad` | `[u8; 3]` | 3 B | Alignment padding |
| `text_offset` | `u64` | 8 B | Byte offset into turbo_static_text.bin (4 B alignment gap before this field) |
| `text_len` | `u32` | 4 B | Byte length of text (4 B trailing padding after this field) |

Total record size is 32 bytes including the two alignment gaps. File length
must be a multiple of 32 and match the header's `record_count`.

### `turbo_static_text.bin`

Concatenated UTF-8 text for all chunks. Accessed via `text_offset` + `text_len` from MetaRecord. Single memory address lookup — no S3 dependency.

### `centroids.bin` / `projection.bin`

Both share an 8-byte asset header (two `u32` dimensions) followed by
little-endian `f32` values:

- `centroids.bin`: header `(dim, centroids_per_dim = 4)`, then `dim × 4`
  values — the per-dimension 1D MSE centroid table (8,200 bytes at 512 d).
  Generated with fixed seed 7.
- `projection.bin`: header `(input_dim, output_dim)`, then the row-major
  `512 × 512` QJL projection matrix S (~1 MB at 512 d). Generated with fixed
  seed 11.

The fixed seeds make builds reproducible. Loading validates both files'
dimensions against the header's `dim`.

All five files are `mmap`-mounted once at Lambda startup and reused for the container's lifetime.

---

## 5. Component Design

### 5.1 New: `MmapIndex` (global singleton)

```rust
// src/index/mmap_index.rs
pub struct MmapIndex {
    header: TurboHeader,          // parsed from turbo_static.bin
    layout: KnownRecordLayout,    // V1Dim512
    bin_mmap: Mmap,
    meta_mmap: Mmap,
    text_mmap: Mmap,
    centroids: CentroidTable,     // 1D MSE centroids, loaded at startup
    projection: ProjectionMatrix, // QJL matrix S, loaded at startup
}

impl MmapIndex {
    pub fn load(dir: &Path) -> Result<Self, MmapIndexError>;
    /// OnceLock-backed singleton; called once in Lambda handler init.
    pub fn global_from_image() -> Result<&'static Self, MmapIndexError>;
    /// Typed zero-copy view over bin_mmap, dispatched by layout.
    pub fn records(&self) -> TurboRecordSlice<'_>;  // ::V1Dim512(&[TurboRecord512])
}
```

`centroids` and `projection` are precomputed offline and bundled alongside the `.bin` files. All five files are size/dimension-validated during `load`; warm instances reuse the singleton without re-mmapping.

### 5.2 New: `TurboQuantSearcher`

```rust
// src/query/turbo_searcher.rs
pub struct TurboQuantSearcher {
    index: &'static MmapIndex,
}

impl StaticRetriever for TurboQuantSearcher {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}
```

Implements the `StaticRetriever` trait consumed by the QueryRouter's static
branch (synchronous — parallelism comes from `rayon`, not async). Brute-force
linear scan with per-thread bounded heaps; ties broken by ascending `doc_id`
for deterministic ordering. Score formula (TurboQuant_prod estimator):

```
score = ⟨y, x̃_mse⟩ + γ · ⟨y, S^T · sign(qjl)⟩
```

Returns `SearchResult` with `source: ChunkSource::Static`.

### 5.3 Modified: `QueryRouter` (`src/query/router.rs`)

Adds a third parallel branch for the static path:

```
Before: vector_search ──┬→ RRF → results
        keyword_search ─┘

After:  turbo_search ─────────────────────→ static_chunks
        vector_search ──┬→ RRF → dynamic_chunks
        keyword_search ─┘
```

Both chunk groups are passed downstream with their source labels. No cross-group RRF.

### 5.4 Modified: `SearchResult` (`src/models/search.rs`)

```rust
pub enum ChunkSource {
    Static,   // from TurboQuant
    Dynamic,  // from LanceDB + BM25
}

pub struct SearchResult {
    // existing fields unchanged
    pub doc_id: String,
    pub text: String,
    pub score: f32,
    // new
    pub source: ChunkSource,
    pub corpus_type: Option<CorpusType>,  // Legal, Contract, RFC, etc.
}
```

### 5.5 Modified: `SearchRequest` (`src/models/search.rs`)

```rust
pub struct CorpusWeights {
    pub static_bias: f32,   // 0.0–1.0, bias toward static source in LLM prompt
    pub dynamic_bias: f32,
}

pub struct SearchRequest {
    // existing fields unchanged
    pub query: String,
    pub top_k: usize,           // default: 5
    // new
    pub corpus_weights: Option<CorpusWeights>,  // None = equal weight
}
```

### 5.6 Response Contract: candidates per path

`top_k` is the **K base**. Each path retrieves `3*K` candidates internally
(`retrieval_window(top_k) = 3*top_k`, capped at 100), and `SearchResponse`
returns those candidates **in full — up to `3*K` per path**, not truncated to
`K`. So a `top_k = 5` request yields up to 15 static + 15 dynamic chunks (the
designed 6*K), and the upstream caller assembles the LLM context from them.
(`QueryRouter::search` truncates each group to `retrieval_window(top_k)`;
`SearchResponse::validate` caps each group at the same value.)

`citation` (title, source ref, url) is a first-class provenance field and is
**preserved even when `include_metadata=false`** — only the freeform `metadata`
map is dropped. This lets an upstream caller request a lean response yet still
render source labels (`[法规 #1] 民法典`) from `citation.title`.

---

## 6. LLM Integration

### Integration Ownership

The LLM turn lives **outside LTSearch**. LTSearch is a pure retrieval service:
its query Lambda returns `SearchResponse` (JSON) and never calls an LLM.
`ContextBuilder` (`src/query/context_builder.rs`) is a **library** the upstream
caller (e.g. `ltbase.api`, which owns the LLM interaction) depends on:

- The caller receives `SearchResponse` (up to `3*K` per path, with `text`,
  `corpus_type`, and `citation.title` per §5.6).
- It calls `ContextBuilder::build_context_bounded(static_chunks, dynamic_chunks,
  query, max_tokens)` to assemble the 6*K context, and
  `build_system_prompt(corpus_weights)` for the weight instruction.
- **`max_tokens` (the token budget) is owned by the caller** — it knows the
  target LLM's context window; LTSearch does not hold this budget.

LTSearch's public surface (`SearchResponse` fields + the `ContextBuilder`
export) is sufficient for the caller to construct context without any
LTSearch-side LLM code. Format below is produced by `ContextBuilder`.

### Context Format

```
=== Reference Materials ===

[Legal/Contract #1] <title>
<text>

[Legal/Contract #2] ...
... (up to 3*K static chunks)

[User Data #1] <title>
<text>

[User Data #2] ...
... (up to 3*K dynamic chunks)

=== Question ===
{query}
```

### System Prompt Template

```
You are a professional document retrieval assistant.

References are grouped into two categories:
- [Legal/Contract]: from the shared authoritative document library (laws, contracts, RFCs)
- [User Data]: from the user's private documents

{weight_instruction}

Only cite content directly relevant to the question. Ignore unrelated passages.
Always indicate the source type when citing.
```

### `weight_instruction` Selection

| Condition | Instruction |
|---|---|
| `static_bias > 0.7` | "If Legal/Contract and User Data conflict, defer to Legal/Contract." |
| `dynamic_bias > 0.7` | "Prioritize User Data; supplement with Legal/Contract only when needed." |
| default (equal / unset) | "Draw from both sources equally without preference." |

---

## 7. K Value Guidelines

| Use Case | K | Total chunks to LLM | Est. tokens |
|---|---|---|---|
| Default | 5 | 30 | ~9,000 |
| Detailed analysis | 10 | 60 | ~18,000 |
| Compliance review | 15 | 90 | ~27,000 |

Default `top_k = 5`. Callers increase as needed.

---

## 8. Offline Index Build Pipeline

Independent from the existing `index_builder_lambda`. Triggered manually or on a scheduled basis (monthly/quarterly).

```
Static documents in S3
        │
   Text chunking
        │
   Embedding generation (same model as dynamic path)
        │
   Centroid/projection generation (fixed seeds 7/11)
                          → centroids.bin, projection.bin
   TurboQuant compression → turbo_static.bin (header + records)
   Metadata extraction   → turbo_static_meta.bin
   Text concatenation    → turbo_static_text.bin
        │
   Docker image rebuild → ECR push → Lambda function update
```

The five output files are committed as build artifacts into the Docker image layer, not fetched at runtime. The builder (`StaticIndexBuilder`, CLI wrapper `src/bin/turbo_index_builder.rs`) only accepts 512-dimensional embeddings and stages all files in a sibling directory before an atomic directory swap.

---

## 9. Deployment

```
Lambda Docker image contains:
  /app/static/turbo_static.bin        (read-only, mmap at startup)
  /app/static/turbo_static_meta.bin   (read-only, mmap at startup)
  /app/static/turbo_static_text.bin   (read-only, mmap at startup)
  /app/static/centroids.bin           (MSE centroids)
  /app/static/projection.bin          (QJL matrix S)

Dynamic data (per-tenant):
  S3 → /tmp cache → LanceDB + Tantivy  (existing mechanism, unchanged)
```

Static index update flow:
1. Run offline build pipeline → produce 5 `.bin` files
2. `docker build` with new files → `docker push` to ECR
3. `aws lambda update-function-code` → Lambda picks up new image

---

## 10. What Is Not Changed

- BM25 keyword search (Tantivy) — dynamic data only, existing implementation unchanged
- Per-tenant LanceDB vector search — existing implementation unchanged
- RRF fusion within the dynamic path — unchanged
- WAL + SQS + IndexBuilder for dynamic data — unchanged
- S3 path conventions for dynamic indexes — unchanged
