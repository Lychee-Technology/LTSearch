# Design Document: Serverless Hybrid Search Engine

## Overview

This design document specifies a hybrid search system that combines three retrievers to provide
high-quality retrieval for RAG pipelines and document search workloads:

- **Static path — TurboQuant**: a custom zero-copy mmap index over an authoritative corpus (laws,
  contracts, RFCs). See `docs/TurboQuant.md`.
- **Dynamic path — LanceDB** (vector) **+ Tantivy** (BM25 keyword), fused with Reciprocal Rank
  Fusion (RRF).

All three retrievers run **in-process, in parallel** inside one query binary — there is no
per-retriever Lambda fan-out. The response is returned as two groups, `static_chunks` and
`dynamic_chunks`. Compute is packaged as **Docker container images that run on both AWS Lambda and
AWS Fargate**; storage is S3.

The architecture supports moderate traffic (~20 QPS average, burst to 500 QPS) with sub-300ms
latency SLA, handling datasets up to 10M documents. Updates are processed through near-real-time
batch indexing, using versioned index publishing with an **ETag compare-and-swap** on the
`index/_head` pointer for atomic, zero-downtime updates.

Embeddings are produced locally by the `jina-embeddings-v5-text-nano-retrieval` ONNX model baked
into the image; the model outputs 768-dim raw vectors that the engine Matryoshka-truncates and
L2-renormalizes to **512-dim**, which is the dimension used end-to-end.

> **Alignment note.** Earlier revisions of this document described a Lambda-only fan-out topology,
> a flat `results` response, 768-dim embeddings, and external embedding APIs. This revision matches
> the implementation on `main`; see the divergence summary in [Known Gaps](#known-gaps-and-current-limitations)
> and the deployment design in [`docs/arch.md` §22](arch.md) / [`docs/deployment.md`](deployment.md).

## Architecture

The system follows a layered architecture with clear separation between query execution, storage, and indexing pipelines.

The three deployables are `query`, `write`, and `index_builder` (each a container image running on
Lambda or Fargate). Retrieval happens **in-process** inside the query image — the boxes inside
`QueryRouter` below are parallel threads, not separate functions.

```mermaid
graph TB
    Client[Client Application]

    subgraph "Query image (one process, Lambda or Fargate)"
        Router[QueryRouter]
        Static[Static retriever: TurboQuant mmap]
        VectorR[Vector retriever: LanceDB]
        KeywordR[Keyword retriever: Tantivy]
        Ranker[HybridRanker RRF]
    end

    subgraph "Storage Layer - S3"
        Head[index/_head + versions/manifest.json]
        LanceData[LanceDB dynamic dataset]
        TantivyIndex[Tantivy index]
        StaticIdx[static/ TurboQuant files]
        WAL[Write-Ahead Log]
    end

    subgraph "Ingestion (write + index_builder images)"
        WriteAPI[Write API]
        Queue[SQS Queue]
        IndexBuilder[Index Builder]
    end

    Client -->|Query| Router
    Router --> Static
    Router --> VectorR
    Router --> KeywordR
    VectorR --> Ranker
    KeywordR --> Ranker
    Ranker -->|dynamic_chunks| Router
    Static -->|static_chunks| Router
    Router -->|SearchResponse| Client

    Static -.->|mmap| StaticIdx
    VectorR -.->|Read| LanceData
    KeywordR -.->|Read| TantivyIndex
    Router -.->|active version| Head

    Client -->|Write| WriteAPI
    WriteAPI -->|Append| WAL
    WriteAPI -->|Enqueue| Queue
    Queue -->|Batch| IndexBuilder
    IndexBuilder -->|Publish + ETag CAS| Head
    IndexBuilder -->|Publish| LanceData
    IndexBuilder -->|Publish| TantivyIndex
```

## Main Algorithm/Workflow

All retrieval runs in one process; the three retrievers are parallel threads
(`std::thread::scope`). Window per path is `3 * top_k`. On embedding failure (after 2 retries) the
router falls back to keyword-only.

```mermaid
sequenceDiagram
    participant Client
    participant QueryRouter
    participant StaticR as StaticRetriever (TurboQuant)
    participant VectorR as VectorRetriever (LanceDB)
    participant KeywordR as KeywordRetriever (Tantivy)
    participant HybridRanker

    Client->>QueryRouter: search(SearchRequest)
    QueryRouter->>QueryRouter: load_active_manifest() (index/_head)
    QueryRouter->>QueryRouter: generate_embedding(query) [512-dim, 2 retries]

    par In-process parallel retrieval (window = 3*top_k)
        QueryRouter->>StaticR: search(embedding, window)
        StaticR-->>QueryRouter: static_results
    and
        QueryRouter->>VectorR: search(embedding, window)
        VectorR-->>QueryRouter: vector_results
    and
        QueryRouter->>KeywordR: search(query, window)
        KeywordR-->>QueryRouter: keyword_results
    end

    QueryRouter->>HybridRanker: fuse(vector_results, keyword_results)
    HybridRanker-->>QueryRouter: dynamic_chunks (RRF, source=Hybrid)
    QueryRouter->>QueryRouter: apply_filters + truncate each group to 3*top_k
    QueryRouter-->>Client: SearchResponse { static_chunks, dynamic_chunks, ... }
```

## Components and Interfaces

### Component 1: Query Router

**Purpose**: Orchestrates hybrid search by running three retrievers in parallel **in-process** and
returning two grouped result sets. It is a generic struct over its collaborators (all held as
fields — there is no per-retriever Lambda), so tests can substitute each seam.

**Interface** (`src/query/router.rs`):
```rust
pub struct QueryRouter<M, E, K, V, S = NoopStaticRetriever, W = NoopWarningSink> {
    manifest_store: M,       // ManifestStore
    embedding_generator: E,  // EmbeddingGenerator
    keyword_retriever: K,    // KeywordRetriever
    vector_retriever: V,     // VectorRetriever
    static_retriever: S,     // StaticRetriever
    warning_sink: W,         // WarningSink
    ranker: HybridRanker,    // HybridRanker::new(60.0)
}

impl<M, E, K, V, S, W> QueryRouter<M, E, K, V, S, W> {
    // synchronous; retrieval fan-out uses std::thread::scope
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse, SearchError>;
}
```

**Responsibilities**:
- Load the active index version from `index/_head` (`ManifestStore`)
- Generate the 512-dim query embedding (2 retries; keyword-only fallback on failure)
- Fan out static / vector / keyword retrieval across three threads
- RRF-fuse **vector + keyword** into `dynamic_chunks`; return static results as `static_chunks`
- Apply filters (with iterative window widening), strip metadata when not requested, and truncate
  each group to the `3 * top_k` retrieval window

### Component 2: Vector Retriever (LanceDB)

**Purpose**: Vector similarity search over the **dynamic** user corpus using LanceDB.

**Interface** (`src/query/vector_searcher.rs`, `src/query/router.rs`):
```rust
pub trait VectorRetriever: Send + Sync {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

// Implemented by VectorSearcher<M>, which queries the LanceDB `documents` table
// (embedding: FixedSizeList<Float32>) with DistanceType::Dot, looping over shards.
```

**Responsibilities**:
- Sync/open the LanceDB dataset for the active version (per-shard cache accounting via
  `LocalLanceCache`)
- Execute ANN search (`DistanceType::Dot`) and decode Arrow rows into `SearchResult`
- Validate the query embedding dimension against `manifest.embedding_dim` and the column size

### Component 3: Keyword Retriever (Tantivy)

**Purpose**: BM25 keyword search over the dynamic corpus.

**Interface** (`src/query/keyword_searcher.rs`, `src/query/router.rs`):
```rust
pub trait KeywordRetriever: Send + Sync {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

// Implemented by KeywordSearcher<M>, using Tantivy's default BM25 scorer over
// fields { doc_id, text (indexed+stored), metadata (stored) }. Single shard only.
```

**Responsibilities**:
- Open the Tantivy index for the active version
- Parse and execute BM25 queries; backfill after dedupe by doubling the limit
- Build `Citation` from stored metadata JSON (`Citation::from_metadata`)

### Component 3b: Static Retriever (TurboQuant)

**Purpose**: Brute-force, zero-copy search over the **static** authoritative corpus.

**Interface** (`src/query/turbo_searcher.rs`):
```rust
pub trait StaticRetriever: Send + Sync {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

// Implemented by TurboQuantSearcher { index: &'static MmapIndex }.
// Parallel (rayon) scan of TurboRecord512 records with a bounded top-K heap.
// NoopStaticRetriever (returns empty) is the default when no static index is present.
```

**Responsibilities**:
- Score the query against every compressed record (TurboQuant estimator; see `docs/TurboQuant.md`)
- Attach `Citation` titles from the static index; results become `static_chunks` (never RRF-fused)

### Component 4: Hybrid Ranker

**Purpose**: Merges **vector + keyword** results using Reciprocal Rank Fusion (RRF). Static results
are not passed through the ranker.

**Interface** (`src/query/ranker.rs`):
```rust
pub struct HybridRanker { rrf_k: f32 } // constructed with HybridRanker::new(60.0)

impl HybridRanker {
    pub fn compute_rrf_score(&self, rank: usize) -> f32; // 1.0 / (rrf_k + rank), rank is 1-based
    pub fn fuse(
        &self,
        vector_results: Vec<SearchResult>,
        keyword_results: Vec<SearchResult>,
    ) -> Vec<SearchResult>; // dedup by doc_id, merge metadata/citation, source = Hybrid
}
```

**Responsibilities**:
- Sum RRF contributions per `doc_id`; merge metadata/citation for shared hits
- Sort by descending fused score with a `doc_id` tie-break; set `source = SearchSource::Hybrid`

### Component 5: Index Builder

**Purpose**: Processes document batches and builds versioned indexes for both vector and keyword search.

**Interface**:
```rust
pub struct IndexBuilder {
    s3_client: Arc<S3Client>,
    embedding_generator: Arc<EmbeddingGenerator>,
}

impl IndexBuilder {
    pub async fn build_index(&self, documents: Vec<Document>) -> Result<IndexVersion, IndexError>;
    pub async fn publish_index(&self, version: IndexVersion) -> Result<(), PublishError>;
    pub async fn rollback_index(&self, version: IndexVersion) -> Result<(), PublishError>;
}
```

**Responsibilities**:
- Consume document batches from SQS queue
- Generate embeddings for documents
- Build LanceDB and Tantivy indexes
- Publish versioned indexes to S3
- Update index version pointer atomically

### Component 6: Write API

**Purpose**: Accepts document write requests and enqueues them for batch processing.

**Interface**:
```rust
pub struct WriteAPI {
    sqs_client: Arc<SqsClient>,
    wal: Arc<WriteAheadLog>,
}

impl WriteAPI {
    pub async fn ingest(&self, documents: Vec<Document>) -> Result<IngestResponse, IngestError>;
    pub async fn delete(&self, doc_ids: Vec<String>) -> Result<DeleteResponse, IngestError>;
}
```

**Responsibilities**:
- Validate incoming documents
- Append to write-ahead log (WAL)
- Enqueue documents to SQS for batch processing
- Return acknowledgment to client

## Data Models

### Model 1: SearchRequest

```rust
pub struct SearchRequest {
    pub query: String,
    pub top_k: usize,
    pub filters: Option<HashMap<String, FilterValue>>,
    pub include_metadata: bool,
    #[serde(default)]
    pub corpus_weights: Option<CorpusWeights>, // static_bias / dynamic_bias in [0,1]
}
```

**Validation Rules** (`src/models/search.rs`):
- query must be non-empty (1-1000 characters)
- top_k must be in range [1, 100]
- filter field names must match `[A-Za-z_][A-Za-z0-9_]*`; filter values must be valid
- if present, `corpus_weights.static_bias` / `dynamic_bias` must be in `[0.0, 1.0]`

### Model 2: SearchResult and SearchResponse

```rust
pub struct SearchResult {
    pub doc_id: String,
    pub score: f32,
    pub text: String,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    pub source: SearchSource,               // Vector | Keyword | Hybrid | Static
    #[serde(default)]
    pub chunk_source: ChunkSource,          // Static | Dynamic
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_type: Option<CorpusType>,    // Legal | Contract | Rfc | Other(u8)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citation: Option<Citation>,
}

pub enum SearchSource { Vector, Keyword, Hybrid, Static }
pub enum ChunkSource { Static, Dynamic }   // default Dynamic
pub enum CorpusType { Legal, Contract, Rfc, Other(u8) }

pub struct Citation {
    pub resource_id: String,
    pub source_type: String,
    pub source_ref: String,
    pub title: Option<String>,
    pub url: Option<String>,
}

// The response is two groups, NOT a flat `results` list:
pub struct SearchResponse {
    pub static_chunks: Vec<SearchResult>,
    pub static_count: usize,
    pub dynamic_chunks: Vec<SearchResult>,
    pub dynamic_count: usize,
    pub latency_ms: u64,
    pub index_version: u64,
}
```

**Validation Rules**:
- `doc_id` must be non-empty; `score` must be finite and `>= 0.0`
- each group's `len()` must be `<= its count` and `<= max_chunks_per_path` (the `3 * top_k` window)
- `Citation` is built from static-index titles (static path) or stored metadata
  (`Citation::from_metadata`, dynamic path)

### Model 3: Document

```rust
pub struct Document {
    pub doc_id: String,
    pub text: String,
    pub embedding: Option<Vec<f32>>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub timestamp: i64,
}
```

**Validation Rules**:
- doc_id must be unique, non-empty string (max 256 chars)
- text must be non-empty string (max 100KB)
- **embedding must be 512-dimensional** if present (jina-v5-nano output, Matryoshka-truncated to 512)
- metadata must be valid JSON object (max 10KB)
- timestamp must be Unix epoch milliseconds

### Model 4: ManifestHead + IndexManifest

Version resolution uses two artifacts (`src/storage/head.rs`, `src/models/index.rs`), not a single
`IndexVersion` struct:

```rust
// index/_head — the atomically-swapped active pointer
pub struct ManifestHead {
    pub version_id: u64,
    pub manifest_path: String, // bucket-relative: index/versions/<version_id>/manifest.json
    pub updated_at: i64,       // epoch millis
}

// index/versions/<v>/manifest.json — the per-version manifest
pub struct IndexManifest {
    pub version_id: u64,
    pub created_at: i64,
    pub embedding_dim: usize,   // 512
    pub document_count: usize,
    pub num_shards: usize,
    pub shards: Vec<ShardManifest>,
}

pub struct ShardManifest {
    pub shard_id: usize,
    pub document_count: usize,
    pub lance_path: String,    // s3:// URI (see Known Gaps re: bucket naming)
    pub tantivy_path: String,  // s3:// URI
}
```

**Validation Rules**:
- `version_id` must be `> 0`; `manifest_path` must equal the key derived from `version_id`
- a published version must be strictly greater than the currently active version
- `updated_at` / `created_at` must be plausible epoch-millis
- shard paths validated by `validate_s3_uri`

### Model 5: IndexCache

```rust
pub struct IndexCache {
    pub cache_dir: PathBuf,
    pub max_size_bytes: u64,
    pub current_version: Option<u64>,
}
```

**Validation Rules**:
- cache_dir must be within `/tmp`
- max_size_bytes must not exceed 10GB (Lambda `/tmp` limit)

**Note**: `IndexCache` is a model/validator. On Lambda the query image syncs the `index/`, `lance/`,
and `static/` prefixes from S3 into `LTSEARCH_QUERY_ARTIFACT_ROOT` per invocation and caches the
bootstrapped handler per active `version_id`; the live vector path tracks cache stats via
`LocalLanceCache`. On Fargate, the always-on process keeps the synced artifacts and handler warm.

## Key Functions with Formal Specifications

### Function 1: search()

```rust
// synchronous; retrieval fan-out uses std::thread::scope
pub fn search(
    &self,
    request: &SearchRequest
) -> Result<SearchResponse, SearchError>
```

**Preconditions:**
- `request.query` is non-empty string
- `request.top_k` is in range [1, 100]
- The three retrievers and the manifest store are initialized
- The active index version is resolvable from `index/_head`

**Postconditions:**
- Returns SearchResponse with `static_chunks` and `dynamic_chunks`, each holding
  up to `3 * top_k` candidates (the retrieval window), not truncated to `top_k`
  — see the TurboQuant hybrid spec §5.6. `top_k` is the K base.
- Results within each group are sorted by descending score
- All returned doc_ids are unique within a group
- If successful: `static_chunks.len()`/`dynamic_chunks.len()` each `<= 3 * top_k`
- If error: returns descriptive SearchError
- No mutations to request parameter

**Loop Invariants:** N/A (async parallel execution)

### Function 2: vector_search()

```rust
pub async fn search(
    &self,
    embedding: Vec<f32>,
    top_k: usize
) -> Result<Vec<SearchResult>, SearchError>
```

**Preconditions:**
- `embedding` is 512-dimensional (validated against `manifest.embedding_dim` and the Lance column)
- `top_k` is in range [1, 100]
- LanceDB dataset is loaded in memory or cache
- All embedding values are finite

**Postconditions:**
- Returns at most `top_k` results
- Results are sorted by descending similarity score
- All scores are in range [0.0, 1.0]
- No duplicate doc_ids in results
- Index state remains unchanged (read-only operation)

**Loop Invariants:**
- During ANN search: all visited nodes maintain heap property
- All processed results have valid similarity scores

### Function 3: keyword_search()

```rust
pub async fn search(
    &self,
    query: &str,
    top_k: usize
) -> Result<Vec<SearchResult>, SearchError>
```

**Preconditions:**
- `query` is non-empty string
- `top_k` is in range [1, 100]
- Tantivy index is loaded and valid
- Query can be parsed by Tantivy query parser

**Postconditions:**
- Returns at most `top_k` results
- Results are sorted by descending BM25 score
- All scores are positive finite numbers
- No duplicate doc_ids in results
- Index state remains unchanged (read-only operation)

**Loop Invariants:**
- During BM25 scoring: all document frequencies remain consistent
- All processed results have valid BM25 scores

### Function 4: fuse()

```rust
pub fn fuse(
    &self,
    vector_results: Vec<SearchResult>,
    keyword_results: Vec<SearchResult>
) -> Vec<SearchResult>
```

**Preconditions:**
- `vector_results` is sorted by descending score
- `keyword_results` is sorted by descending score
- All doc_ids in both lists are valid
- `rrf_k` parameter is positive

**Postconditions:**
- Returns merged and deduplicated results
- Results are sorted by descending RRF score
- All doc_ids are unique
- Result count <= vector_results.len() + keyword_results.len()
- No mutations to input parameters

**Loop Invariants:**
- For each result processed: RRF score correctly computed from ranks
- All previously processed results maintain sorted order

### Function 5: build_index()

```rust
pub async fn build_index(
    &self,
    documents: Vec<Document>
) -> Result<IndexVersion, IndexError>
```

**Preconditions:**
- `documents` is non-empty vector
- All documents have valid doc_ids and text
- Embeddings are generated or provided
- S3 client is initialized and has write permissions

**Postconditions:**
- Returns new IndexVersion with incremented version_id
- LanceDB dataset is created and uploaded to S3
- Tantivy index is created and uploaded to S3
- All documents are indexed in both systems
- If error: no partial index is published
- Original documents vector is consumed (moved)

**Loop Invariants:**
- For each document processed: embedding dimension is 512
- All indexed documents maintain referential integrity
- Index build progress is monotonically increasing

### Function 6: publish_index()

```rust
pub async fn publish_index(
    &self,
    version: IndexVersion
) -> Result<(), PublishError>
```

**Preconditions:**
- `version` references valid index files in S3
- Index files are complete and valid
- _head pointer exists in S3

**Postconditions:**
- _head pointer is atomically updated to new version
- New version becomes active for all subsequent queries
- Previous version remains available for rollback
- Operation is atomic (either fully succeeds or fully fails)

`_head.manifest_path` stores the canonical bucket-relative manifest object key, not a full S3 URI. Its value must match `index/versions/<version_id>/manifest.json` for the active version.

**Loop Invariants:** N/A (atomic operation)

## Algorithmic Pseudocode

### Main Query Processing Algorithm

```rust
// Algorithm: Hybrid Search Query Processing (in-process, three retrievers)
// Input: request: &SearchRequest
// Output: SearchResponse with two result groups

fn process_query(request: &SearchRequest) -> Result<SearchResponse, SearchError> {
    // Precondition: request is validated
    request.validate()?;

    // Step 0: Resolve the active index version (index/_head)
    let active = manifest_store.load_active_manifest()?;
    let window = retrieval_window(request.top_k); // 3 * top_k, capped at 100

    // Step 1: Generate query embedding (2 retries; None => keyword-only fallback)
    let embedding = generate_embedding_with_retry(&request.query);
    if let Ok(ref e) = embedding { assert!(e.len() == 512); }

    // Step 2: In-process parallel retrieval (std::thread::scope), each to `window`
    //         static + vector + keyword. On embedding failure, static/vector are skipped.
    let grouped = search_grouped(&active, &request.query, &embedding, window)?;
    // grouped.static_chunks  = static_results (TurboQuant)
    // grouped.dynamic_chunks = RRF fuse(vector_results, keyword_results)

    // Step 3: Filter (with iterative window widening when filters are present),
    //         then truncate EACH group to the window (not to top_k), strip metadata if unwanted.
    let mut static_chunks  = apply_filters(grouped.static_chunks,  request.filters.as_ref());
    let mut dynamic_chunks = apply_filters(grouped.dynamic_chunks, request.filters.as_ref());
    static_chunks.truncate(window);
    dynamic_chunks.truncate(window);

    let response = SearchResponse {
        static_count: static_chunks.len(),
        dynamic_count: dynamic_chunks.len(),
        static_chunks,
        dynamic_chunks,
        latency_ms: measure_latency(),
        index_version: active.head.version_id,
    };
    response.validate(window)?;
    Ok(response)
}
```

**Preconditions:**
- request is validated and well-formed
- The active manifest (and its indexes) can be loaded
- Embedding generator is available (or keyword-only fallback applies)

**Postconditions:**
- Returns two groups, each with at most `3 * top_k` results
- Within each group, doc_ids are unique and sorted by descending score
- Latency and `index_version` are included

**Loop Invariants:**
- Result sets maintain sorted order throughout processing
- All doc_ids remain unique after each transformation

### Reciprocal Rank Fusion Algorithm

```rust
// Algorithm: Reciprocal Rank Fusion (RRF)
// Input: vector_results, keyword_results (both sorted by score)
// Output: merged results sorted by RRF score

fn reciprocal_rank_fusion(
    vector_results: Vec<SearchResult>,
    keyword_results: Vec<SearchResult>,
    k: f32
) -> Vec<SearchResult> {
    // Precondition: Both inputs are sorted by descending score
    assert!(is_sorted_descending(&vector_results));
    assert!(is_sorted_descending(&keyword_results));
    
    let mut rrf_scores: HashMap<String, f32> = HashMap::new();
    let mut doc_map: HashMap<String, SearchResult> = HashMap::new();
    
    // Step 1: Compute RRF scores from vector results
    for (rank, result) in vector_results.into_iter().enumerate() {
        let rrf_score = 1.0 / (k + (rank as f32 + 1.0));
        *rrf_scores.entry(result.doc_id.clone()).or_insert(0.0) += rrf_score;
        doc_map.insert(result.doc_id.clone(), result);
    }
    
    // Invariant: All processed vector results have RRF contribution
    assert!(rrf_scores.len() == doc_map.len());
    
    // Step 2: Add RRF scores from keyword results
    for (rank, result) in keyword_results.into_iter().enumerate() {
        let rrf_score = 1.0 / (k + (rank as f32 + 1.0));
        *rrf_scores.entry(result.doc_id.clone()).or_insert(0.0) += rrf_score;
        doc_map.entry(result.doc_id.clone()).or_insert(result);
    }
    
    // Step 3: Sort by RRF score descending
    let mut merged: Vec<(String, f32)> = rrf_scores.into_iter().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    
    // Step 4: Build final result list
    let final_results: Vec<SearchResult> = merged
        .into_iter()
        .map(|(doc_id, rrf_score)| {
            let mut result = doc_map.remove(&doc_id).unwrap();
            result.score = rrf_score;
            result.source = SearchSource::Hybrid;
            result
        })
        .collect();
    
    // Postcondition: All doc_ids are unique and sorted by RRF score
    assert!(all_unique_doc_ids(&final_results));
    assert!(is_sorted_descending(&final_results));
    
    final_results
}
```

**Preconditions:**
- vector_results and keyword_results are sorted by descending score
- k parameter is positive (typically 60.0)
- All doc_ids are valid strings

**Postconditions:**
- Returns merged results sorted by descending RRF score
- All doc_ids are unique (deduplicated)
- Result count <= vector_results.len() + keyword_results.len()

**Loop Invariants:**
- All processed results have valid RRF score contribution
- doc_map maintains one-to-one mapping with doc_ids

### Index Building Algorithm

```rust
// Algorithm: Batch Index Building
// Input: documents (batch of documents to index)
// Output: IndexVersion with new version metadata

async fn build_index(documents: Vec<Document>) -> Result<IndexVersion, IndexError> {
    // Precondition: Documents are validated
    assert!(!documents.is_empty());
    assert!(all_valid_documents(&documents));
    
    let version_id = get_next_version_id().await?;
    let lance_path = format!("s3://bucket/lance/v{}", version_id);
    let tantivy_path = format!("s3://bucket/index/v{}", version_id);
    
    // Step 1: Generate embeddings for documents without them
    let mut enriched_docs = Vec::with_capacity(documents.len());
    for doc in documents {
        let embedding = if let Some(emb) = doc.embedding {
            emb
        } else {
            generate_embedding(&doc.text).await?
        };
        
        // Invariant: All embeddings are 512-dimensional
        assert!(embedding.len() == 512);
        
        enriched_docs.push(Document {
            embedding: Some(embedding),
            ..doc
        });
    }
    
    // Step 2: Build LanceDB dataset
    let lance_builder = LanceDatasetBuilder::new();
    for doc in &enriched_docs {
        lance_builder.add_row(
            &doc.doc_id,
            doc.embedding.as_ref().unwrap(),
            &doc.text,
            &doc.metadata
        )?;
    }
    let lance_dataset = lance_builder.build()?;
    
    // Step 3: Build Tantivy index
    let tantivy_builder = TantivyIndexBuilder::new();
    for doc in &enriched_docs {
        tantivy_builder.add_document(
            &doc.doc_id,
            &doc.text,
            &doc.metadata
        )?;
    }
    let tantivy_index = tantivy_builder.build()?;
    
    // Invariant: Both indexes contain same document count
    assert!(lance_dataset.count() == tantivy_index.count());
    assert!(lance_dataset.count() == enriched_docs.len());
    
    // Step 4: Upload to S3
    upload_to_s3(&lance_dataset, &lance_path).await?;
    upload_to_s3(&tantivy_index, &tantivy_path).await?;
    
    // Step 5: Create version metadata
    let index_version = IndexVersion {
        version_id,
        lance_path,
        tantivy_path,
        document_count: enriched_docs.len(),
        created_at: current_timestamp(),
    };
    
    // Postcondition: Valid index version created
    assert!(index_version.document_count == enriched_docs.len());
    
    Ok(index_version)
}
```

**Preconditions:**
- documents is non-empty vector
- All documents have valid doc_ids and text
- S3 client has write permissions

**Postconditions:**
- New IndexVersion is created with incremented version_id
- Both LanceDB and Tantivy indexes are built and uploaded
- All documents are indexed in both systems
- Document counts match across both indexes

**Loop Invariants:**
- All processed documents have 512-dimensional embeddings
- Both indexes maintain consistent document counts

### Index Cache Management Algorithm

```rust
// Algorithm: Lambda Index Cache Management
// Input: version (target index version to load)
// Output: Loaded index in /tmp cache

async fn load_index_with_cache(version: IndexVersion) -> Result<(), IndexError> {
    let cache_dir = PathBuf::from("/tmp/search_index");
    let version_marker = cache_dir.join("version.txt");
    
    // Step 1: Check if correct version is already cached
    if cache_dir.exists() && version_marker.exists() {
        let cached_version = read_version_marker(&version_marker)?;
        
        if cached_version == version.version_id {
            // Cache hit - reuse existing index
            return Ok(());
        }
    }
    
    // Step 2: Cache miss - clear old cache
    if cache_dir.exists() {
        remove_dir_all(&cache_dir)?;
    }
    create_dir_all(&cache_dir)?;
    
    // Step 3: Download index from S3
    let lance_cache = cache_dir.join("lance");
    let tantivy_cache = cache_dir.join("tantivy");
    
    download_from_s3(&version.lance_path, &lance_cache).await?;
    download_from_s3(&version.tantivy_path, &tantivy_cache).await?;
    
    // Step 4: Verify cache size constraint
    let cache_size = calculate_dir_size(&cache_dir)?;
    assert!(cache_size <= 10 * 1024 * 1024 * 1024); // 10 GB limit
    
    // Step 5: Write version marker
    write_version_marker(&version_marker, version.version_id)?;
    
    // Postcondition: Cache contains correct version
    assert!(cache_dir.exists());
    assert!(read_version_marker(&version_marker)? == version.version_id);
    
    Ok(())
}
```

**Preconditions:**
- version references valid index files in S3
- /tmp directory is writable
- Sufficient space available in /tmp (up to 10 GB)

**Postconditions:**
- Index is loaded in /tmp cache directory
- Version marker file indicates cached version
- Cache size does not exceed 10 GB limit
- Old cache is cleared if version mismatch

**Loop Invariants:** N/A (sequential operations)

## Example Usage

```rust
// Example 1: Basic hybrid search query
// (QueryRouter is constructed from its collaborators; see src/query_lambda.rs bootstrap.)
use ltsearch::models::SearchRequest;

fn run(router: &Router, request: &SearchRequest) {
    let response = router.search(request).expect("search");

    // Two groups: authoritative (static) and user corpus (dynamic, RRF-fused).
    for group in [&response.static_chunks, &response.dynamic_chunks] {
        for result in group {
            println!("Doc: {} | Score: {:.4} | Source: {:?} | Chunk: {:?}",
                result.doc_id, result.score, result.source, result.chunk_source);
        }
    }

    let request = SearchRequest {
        query: "serverless vector database".to_string(),
        top_k: 10,
        filters: None,
        include_metadata: true,
        corpus_weights: None,
    };
    let _ = request;
}
```

// Example 2: Document ingestion
use serverless_search::{WriteAPI, Document};
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let write_api = WriteAPI::new().await?;
    
    let documents = vec![
        Document {
            doc_id: "doc_001".to_string(),
            text: "Serverless computing with AWS Lambda".to_string(),
            embedding: None, // Will be generated
            metadata: HashMap::from([
                ("category".to_string(), json!("technology")),
                ("author".to_string(), json!("Alice")),
            ]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        },
        Document {
            doc_id: "doc_002".to_string(),
            text: "Vector databases for machine learning".to_string(),
            embedding: None,
            metadata: HashMap::from([
                ("category".to_string(), json!("ml")),
                ("author".to_string(), json!("Bob")),
            ]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        },
    ];
    
    let response = write_api.ingest(documents).await?;
    println!("Ingested {} documents", response.count);
    
    Ok(())
}

// Example 3: Index building and publishing
use serverless_search::{IndexBuilder, Document};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let builder = IndexBuilder::new().await?;
    
    // Consume batch from SQS
    let documents = consume_document_batch().await?;
    
    // Build new index version
    let version = builder.build_index(documents).await?;
    println!("Built index version: {}", version.version_id);
    
    // Publish atomically
    builder.publish_index(version).await?;
    println!("Index published successfully");
    
    Ok(())
}

// Example 4: Hybrid ranking with custom parameters
use serverless_search::HybridRanker;

fn main() {
    let ranker = HybridRanker { rrf_k: 60.0 };
    
    let vector_results = vec![
        SearchResult { doc_id: "doc_1".into(), score: 0.95, ..Default::default() },
        SearchResult { doc_id: "doc_2".into(), score: 0.87, ..Default::default() },
        SearchResult { doc_id: "doc_3".into(), score: 0.82, ..Default::default() },
    ];
    
    let keyword_results = vec![
        SearchResult { doc_id: "doc_2".into(), score: 12.5, ..Default::default() },
        SearchResult { doc_id: "doc_4".into(), score: 10.3, ..Default::default() },
        SearchResult { doc_id: "doc_1".into(), score: 8.7, ..Default::default() },
    ];
    
    let merged = ranker.fuse(vector_results, keyword_results);
    
    for result in merged {
        println!("Doc: {} | RRF Score: {:.4}", result.doc_id, result.score);
    }
}
```

## Correctness Properties

The following properties must hold for all valid inputs and system states:

### Property 1: Search Result Uniqueness (per group)
```rust
// Within each group, doc_ids must be unique
∀ response: SearchResponse,
  ∀ group in [response.static_chunks, response.dynamic_chunks],
    group.iter().map(|r| &r.doc_id).collect::<HashSet<_>>().len() == group.len()
```

### Property 2: Result Ordering (per group)
```rust
// Each group must be sorted by descending score
∀ response: SearchResponse,
  ∀ group in [response.static_chunks, response.dynamic_chunks],
    ∀ i, j where 0 <= i < j < group.len(),
      group[i].score >= group[j].score
```

### Property 3: Retrieval-Window Constraint
```rust
// Each group holds at most the retrieval window (3 * top_k), NOT top_k,
// and never more than its declared count.
∀ request: SearchRequest, response: SearchResponse,
  let window = min(3 * max(request.top_k, 1), 100);
  response.static_chunks.len()  <= min(window, response.static_count) &&
  response.dynamic_chunks.len() <= min(window, response.dynamic_count)
```

### Property 4: RRF Score Correctness
```rust
// RRF score must be computed correctly from ranks
∀ doc_id in merged_results,
  let vector_rank = find_rank(doc_id, vector_results);
  let keyword_rank = find_rank(doc_id, keyword_results);
  let expected_score = 
    (if vector_rank.is_some() { 1.0 / (k + vector_rank.unwrap() + 1.0) } else { 0.0 })
    + (if keyword_rank.is_some() { 1.0 / (k + keyword_rank.unwrap() + 1.0) } else { 0.0 });
  
  merged_results[doc_id].score == expected_score
```

### Property 5: Index Version Consistency
```rust
// All queries within same Lambda execution use same index version
∀ lambda_execution,
  ∀ query1, query2 in lambda_execution,
    query1.index_version == query2.index_version
```

### Property 6: Cache Size Constraint
```rust
// Lambda cache must not exceed 10 GB
∀ cache_state: IndexCache,
  calculate_dir_size(&cache_state.cache_dir) <= 10 * 1024 * 1024 * 1024
```

### Property 7: Embedding Dimension Consistency
```rust
// All embeddings must be 512-dimensional (jina-v5-nano truncated to 512)
∀ document: Document where document.embedding.is_some(),
  document.embedding.unwrap().len() == 512
```

### Property 8: Atomic Index Publishing
```rust
// Index publish is atomic - either fully succeeds or fully fails
∀ publish_operation,
  (lance_published ∧ tantivy_published ∧ head_updated) 
  ∨ (¬lance_published ∧ ¬tantivy_published ∧ ¬head_updated)
```

### Property 9: Document Count Consistency
```rust
// Both indexes must contain same document count
∀ index_version: IndexVersion,
  lance_dataset.count() == tantivy_index.count() 
  == index_version.document_count
```

### Property 10: Latency SLA
```rust
// Query latency must not exceed 300ms (excluding cold starts)
∀ query with warm_cache,
  query.latency_ms <= 300
```

## Error Handling

### Error Scenario 1: Index Cache Miss (Cold Start)

**Condition**: Lambda container starts without cached index or version mismatch
**Response**: 
- Download index from S3 to /tmp cache
- Verify index integrity
- Update version marker
- Proceed with query execution

**Recovery**: 
- Subsequent requests in same container use cached index
- Expected cold start latency: 2-5 seconds
- Warm request latency: 50-150ms

### Error Scenario 2: S3 Download Failure

**Condition**: Network error or S3 unavailability during index download
**Response**:
- Retry with exponential backoff (3 attempts)
- Return 503 Service Unavailable to client
- Log error with request ID for debugging

**Recovery**:
- Client retries request
- May hit different Lambda with cached index
- CloudWatch alarm triggers if error rate exceeds threshold

### Error Scenario 3: Cache Size Exceeded

**Condition**: Index size exceeds 10 GB Lambda /tmp limit
**Response**:
- Fail index load operation
- Return 500 Internal Server Error
- Alert operations team via CloudWatch alarm

**Recovery**:
- Implement index sharding to reduce per-shard size
- Increase shard count to distribute index
- Consider index compression techniques

### Error Scenario 4: Invalid Query Input

**Condition**: Client sends malformed query (empty string, invalid top_k, etc.)
**Response**:
- Validate request parameters
- Return 400 Bad Request with descriptive error message
- Do not invoke search operations

**Recovery**:
- Client corrects request and retries
- No system state changes

### Error Scenario 5: Embedding Generation Failure

**Condition**: Embedding model unavailable or returns error
**Response**:
- Retry embedding generation (2 attempts)
- If still failing, fall back to keyword-only search
- Log warning with request details

**Recovery**:
- Return keyword search results only
- Monitor embedding service health
- Alert if failure rate exceeds threshold

### Error Scenario 6: Index Build Failure

**Condition**: Index builder fails during batch processing (OOM, invalid data, etc.)
**Response**:
- Rollback partial index build
- Move failed batch to dead-letter queue (DLQ)
- Do not publish incomplete index
- Log error with batch details

**Recovery**:
- Investigate failed documents in DLQ
- Fix data issues and reprocess
- Subsequent batches continue processing normally

### Error Scenario 7: Index Publish Race Condition

**Condition**: Multiple index builders attempt to publish simultaneously
**Response**:
- Use S3 conditional writes for _head pointer
- Only one publish succeeds atomically
- Failed publishers detect conflict and abort

**Recovery**:
- Failed publisher logs warning
- Next batch will publish successfully
- No data loss or corruption

### Error Scenario 8: Lambda Timeout

**Condition**: Query execution exceeds Lambda timeout (30 seconds)
**Response**:
- Lambda terminates execution
- Return 504 Gateway Timeout to client
- Log timeout event with query details

**Recovery**:
- Client retries with exponential backoff
- Investigate slow queries (complex filters, large result sets)
- Consider query optimization or timeout increase

## Testing Strategy

### Unit Testing Approach

Unit tests focus on individual components and functions in isolation.

**Key Test Cases**:
- RRF score computation with various rank combinations
- Search result deduplication and sorting
- Index cache management (hit/miss scenarios)
- Request validation (boundary conditions, invalid inputs)
- Embedding dimension validation

**Coverage Goals**:
- 90% code coverage for core algorithms
- 100% coverage for RRF fusion logic
- All error paths tested

**Testing Framework**: Rust's built-in test framework with `cargo test`

### Property-Based Testing Approach

Property-based tests verify correctness properties hold for all valid inputs.

**Property Test Library**: proptest (Rust property testing framework)

**Key Properties to Test**:
1. Search result uniqueness (no duplicate doc_ids)
2. Result ordering (descending score)
3. Top-K constraint (result count <= top_k)
4. RRF score correctness (matches formula)
5. Cache size constraint (never exceeds 10 GB)
6. Embedding dimension consistency (always 512)

**Example Property Test**:
```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_rrf_produces_unique_sorted_results(
        vector_results in prop::collection::vec(arbitrary_search_result(), 1..20),
        keyword_results in prop::collection::vec(arbitrary_search_result(), 1..20),
    ) {
        let ranker = HybridRanker { rrf_k: 60.0 };
        let merged = ranker.fuse(vector_results, keyword_results);
        
        // Property 1: All doc_ids are unique
        let doc_ids: HashSet<_> = merged.iter().map(|r| &r.doc_id).collect();
        prop_assert_eq!(doc_ids.len(), merged.len());
        
        // Property 2: Results are sorted by descending score
        for i in 0..merged.len().saturating_sub(1) {
            prop_assert!(merged[i].score >= merged[i + 1].score);
        }
    }
}
```

### Integration Testing Approach

Integration tests verify end-to-end workflows with real AWS services (using Moto for local testing).

**Key Integration Tests**:
- Complete query pipeline (embedding → vector search → keyword search → fusion)
- Index build and publish workflow
- Cache management across Lambda invocations
- S3 index versioning and atomic updates
- SQS batch processing

**Test Environment**:
- Moto for S3 and SQS simulation
- In-memory LanceDB and Tantivy indexes
- Mock embedding service

**Test Data**:
- Synthetic document corpus (1000-10000 documents)
- Realistic query patterns
- Edge cases (empty results, large batches, etc.)

## Performance Considerations

### Query Latency Breakdown

Target latency budget (300ms SLA):
- Embedding generation: 20-40ms (13%)
- Vector search (LanceDB): 40-80ms (27%)
- Keyword search (Tantivy): 10-30ms (10%)
- RRF fusion: <5ms (2%)
- Network overhead: 20-40ms (13%)
- Buffer: 100-150ms (35%)

### Cold Start Optimization

**Problem**: Lambda cold starts add 2-5 seconds latency

**Mitigation Strategies**:
- Provisioned concurrency for baseline traffic (2-5 instances)
- Lazy index loading (load on first query, not on init)
- Index compression to reduce download time
- CloudFront caching for frequently accessed index versions

### Index Sharding Strategy

**Problem**: Large indexes exceed 10 GB cache limit

**Solution**: Partition the dynamic index into shards (recorded in the manifest)
- Shard assignment: `shard_id = hash(doc_id) % num_shards`
- The vector retriever **iterates shards in-process** (not a Lambda-per-shard fan-out) and merges
- Keyword search currently requires a single shard

**Trade-offs**:
- Lower per-container memory per shard
- Sequential shard iteration trades some latency for simplicity
- Larger corpora are better served by the always-on Fargate path (no repeated cold sync)

### Memory Management

**Lambda Memory Configuration**: 3-10 GB
- Base runtime: ~500 MB
- LanceDB dataset: 2-8 GB (depends on shard size)
- Tantivy index: 1-3 GB (depends on shard size)
- Query processing: ~100-500 MB

**Optimization Techniques**:
- Memory-mapped file I/O for indexes
- Streaming result processing
- Aggressive deallocation after query completion

### Throughput Scaling

**Baseline Traffic (20 QPS)**:
- 2-5 warm Lambda instances
- Average concurrency: 4-6
- Cost: ~$15-20/month

**Burst Traffic (500 QPS)**:
- Auto-scale to 100-150 Lambda instances
- Concurrency limit: 1000 (AWS default)
- Cost during burst: ~$0.50-1.00 per hour

**Scaling Characteristics**:
- Linear scaling up to 500 QPS
- Bottleneck: S3 request rate (5500 req/sec per prefix)
- Mitigation: Use multiple S3 prefixes for shards

## Security Considerations

### Authentication and Authorization

**API Gateway Integration**:
- AWS IAM authentication for service-to-service
- API keys for external clients
- JWT tokens for user authentication

**Lambda Execution Role**:
- Least privilege IAM policy
- Read-only access to S3 index buckets
- Write access to CloudWatch Logs
- No cross-account access

### Data Encryption

**At Rest**:
- S3 server-side encryption (SSE-KMS)
- Customer-managed KMS keys
- Automatic key rotation enabled

**In Transit**:
- TLS 1.3 for all API communications
- VPC endpoints for S3 access (no internet routing)
- Encrypted Lambda environment variables

### Tenant Isolation

**Multi-Tenant Strategy**:
- S3 prefix per tenant: `s3://bucket/{tenant_id}/index/`
- Lambda environment variable for tenant context
- Query-time filtering by tenant_id
- No cross-tenant data leakage

**Access Control**:
- IAM policies scoped to tenant prefixes
- CloudWatch Logs separated by tenant
- Audit trail for all data access

### Secrets Management

**Sensitive Configuration**:
- AWS Secrets Manager for API keys
- KMS encryption for secrets
- Automatic secret rotation
- No secrets in Lambda code or environment variables

### Network Security

**VPC Configuration**:
- Lambda functions in private subnets
- VPC endpoints for S3 and SQS (no NAT gateway needed)
- Security groups restrict outbound traffic
- No public IP addresses

**DDoS Protection**:
- API Gateway throttling (burst: 5000, steady: 10000 req/sec)
- AWS WAF rules for common attack patterns
- CloudFront rate limiting

## Dependencies

### Core Runtime Dependencies

**Rust Crates** (actual, per `Cargo.toml`):
- `tokio` (1.35): async runtime
- `aws-config`, `aws-sdk-s3`, `aws-sdk-sqs` (1.x): AWS clients
- `lambda_runtime` (0.13): AWS Lambda runtime for Rust
- `serde` / `serde_json` (1.0): serialization
- `rayon` (1.10): parallel static-index scan; `memmap2` (0.9): mmap of the TurboQuant index

**Search Libraries** (pinned):
- `lancedb` (0.26.2) + `lance` (=2.0.0): vector search (dynamic corpus)
- `tantivy` (=0.24.2): BM25 keyword search
- `arrow-array` / `arrow-schema` (57.2): columnar decode

**Embedding Generation** (local, no external API):
- `ltembed` (git dependency, `optional`, behind the `ltembed` feature): wraps the LTEmbed ONNX
  engine (`jina-embeddings-v5-text-nano-retrieval`, 512-dim output)
- `ort` (2.0.0-rc, `load-dynamic`): ONNX Runtime bindings — the `libonnxruntime.so` ships in the
  baked ort bundle, so the compiled binary is architecture-portable (arm64) across Lambda/Fargate
- Default build uses `--no-default-features`; the `fixed` deterministic stub provider replaces the
  model in CI and unit tests (vendored `ltembed-stub` crate satisfies the optional git dep)

### AWS Services

**Compute**:
- Container images (`PackageType: Image`, arm64) run on **AWS Lambda** (custom-runtime base) and
  **AWS Fargate/ECS** (plain al2023 base + Lambda Web Adapter). See `docs/deployment.md`.
- Lambda function memory: 1-10 GB; Fargate task sized per corpus
- Lambda timeout: 30 seconds (query), up to 15 minutes (indexing); Fargate has no such limit

**Storage**:
- Amazon S3 (Standard storage class)
- S3 bucket versioning enabled
- S3 lifecycle policies for old index cleanup

**Messaging**:
- Amazon SQS (Standard queue)
- Message retention: 4 days
- Dead-letter queue for failed batches

**Monitoring**:
- Amazon CloudWatch Logs
- Amazon CloudWatch Metrics
- AWS X-Ray for distributed tracing

**Security**:
- AWS IAM for authentication/authorization
- AWS KMS for encryption key management
- AWS Secrets Manager for API keys

### External Services (Optional)

**Embedding Generation**: none required at runtime — the model is baked into the image and run
locally via ONNX Runtime. (`minimal-ort-builder` releases supply the pinned ort bundle at build
time only.)

**Reranking (Optional)**: not implemented; would be an external GPU inference endpoint if added
(see `docs/arch.md` §9).

### Development and Testing

**Build Tools**:
- Rust toolchain (1.94.0, per `rust-toolchain.toml`)
- Docker + BuildKit for the multi-stage image build (`sam/builder.Dockerfile`)
- AWS SAM CLI for local end-to-end runs against Moto

**Testing**:
- `proptest` (1.4+): Property-based testing
- `mockall` (0.12+): Mocking framework
- Moto (3.0+): Local AWS service emulation

**CI/CD**:
- GitHub Actions (self-hosted ARM64 runners): fast checks → Moto integration → SAM local e2e
- `scripts/verify-fast.sh` (build all bins + non-Moto tests + fmt + clippy), `scripts/verify-moto.sh`

## Known Gaps and Current Limitations

This document describes the implemented system; the following are known gaps where behavior is
incomplete or intentionally deferred. They are recorded so the spec does not overstate a closed loop.

1. **SQS → build is not auto-wired end-to-end.** `BuildFunction` has no SQS `EventSource` in
   `template.sam-e2e.yaml` (it is invoke-only), and the enqueued
   `QueueBatch { batch_id, wal_key, accepted_count, wal_event_ids }` does not carry the
   `version_id` / `embedding_dim` that `BuildRequest` requires. Version allocation and payload
   translation are an external/missing orchestration step.
2. **`ContextBuilder` is not invoked in the query path.** `src/query/context_builder.rs` (the
   `6 * top_k` LLM-context assembler, token budgeting, corpus-weight system prompt) is fully
   implemented and unit-tested but `QueryRouter::search` returns the raw grouped chunks; context
   assembly is left to an upstream caller. `SearchRequest.corpus_weights` is likewise a reserved
   interface with no consumer in the live query path.
3. **Deferred fake-bucket manifest migration.** `ShardManifest.{lance_path, tantivy_path}` still
   carry `s3://local-artifacts/…` placeholder URIs that read sides strip; migrating them to
   bucket-relative keys is a separate, backward-compatibility-sensitive change (see
   `docs/architecture-review-2026-07-05.md`). Note `ManifestHead.manifest_path` is *already*
   bucket-relative and correct.
4. **Deployment is Lambda container images today.** The unified Fargate + Lambda image (HTTP server
   + Lambda Web Adapter) described in `docs/arch.md` §22 and `docs/deployment.md` is the target;
   it is not yet implemented.
