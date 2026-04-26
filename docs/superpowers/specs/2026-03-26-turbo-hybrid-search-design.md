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
| Static corpus scale | 10万 ~ 100万 chunks (~15–156 MB compressed) |
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

Three memory-mapped binary files, all bundled in the Lambda Docker image:

```
/app/static/
  turbo_static.bin       # Compressed vectors, 156 bytes per record
  turbo_static_meta.bin  # Fixed-size metadata, one record per chunk
  turbo_static_text.bin  # Concatenated raw text, variable length
```

### `turbo_static.bin` — TurboRecord (156 bytes, repr(C))

| Field | Type | Size | Description |
|---|---|---|---|
| `doc_id` | `u64` | 8 B | Unique chunk identifier |
| `idx` | `[u8; 96]` | 96 B | Stage 1: 2-bit MSE quantization indices (384 dims × 2 bits) |
| `qjl` | `[u8; 48]` | 48 B | Stage 2: QJL residual sign bits (384 dims × 1 bit) |
| `gamma` | `f32` | 4 B | L2 norm of residual vector ‖r‖₂ |

### `turbo_static_meta.bin` — MetaRecord (fixed-size, repr(C))

| Field | Type | Size | Description |
|---|---|---|---|
| `doc_id` | `u64` | 8 B | Links to TurboRecord |
| `corpus_type` | `u8` | 1 B | Enum: Legal=0, Contract=1, RFC=2, ... |
| `_pad` | `[u8; 3]` | 3 B | Alignment padding |
| `text_offset` | `u64` | 8 B | Byte offset into turbo_static_text.bin |
| `text_len` | `u32` | 4 B | Byte length of text |

### `turbo_static_text.bin`

Concatenated UTF-8 text for all chunks. Accessed via `text_offset` + `text_len` from MetaRecord. Single memory address lookup — no S3 dependency.

All three files are `mmap`-mounted once at Lambda startup and reused for the container's lifetime.

---

## 5. Component Design

### 5.1 New: `MmapIndex` (global singleton)

```rust
// src/index/mmap_index.rs
pub struct MmapIndex {
    records: &'static [TurboRecord],
    meta: &'static [MetaRecord],
    text_blob: &'static [u8],
    centroids: Centroids,        // 1D MSE centroids, loaded at startup
    projection: ProjectionMatrix, // QJL matrix S, loaded at startup
}

impl MmapIndex {
    pub fn load_from_image() -> Self;  // called once in Lambda handler init
}
```

`centroids` and `projection` are precomputed offline and bundled alongside the `.bin` files.

### 5.2 New: `TurboQuantSearcher`

```rust
// src/query/turbo_searcher.rs
pub struct TurboQuantSearcher {
    index: &'static MmapIndex,
}

impl TurboQuantSearcher {
    pub async fn search(&self, query_embedding: &[f32], top_k: usize) -> Vec<SearchResult>;
}
```

Implements the same search trait as `VectorSearcher`. Uses `rayon` parallel iterator for brute-force linear scan. Score formula (TurboQuant_prod estimator):

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

---

## 6. LLM Integration

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
   TurboQuant compression → turbo_static.bin
   Metadata extraction   → turbo_static_meta.bin
   Text concatenation    → turbo_static_text.bin
        │
   Docker image rebuild → ECR push → Lambda function update
```

The three output files are committed as build artifacts into the Docker image layer, not fetched at runtime.

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
1. Run offline build pipeline → produce 3 `.bin` files
2. `docker build` with new files → `docker push` to ECR
3. `aws lambda update-function-code` → Lambda picks up new image

---

## 10. What Is Not Changed

- BM25 keyword search (Tantivy) — dynamic data only, existing implementation unchanged
- Per-tenant LanceDB vector search — existing implementation unchanged
- RRF fusion within the dynamic path — unchanged
- WAL + SQS + IndexBuilder for dynamic data — unchanged
- S3 path conventions for dynamic indexes — unchanged
