## **Serverless Hybrid Search Engine Architecture**

---

# **1\. Overview**

This document describes the architecture of a **hybrid search system** built using:

* **Rust runtime**
* **Docker container images** deployable to **AWS Lambda and AWS Fargate** (compute)
* **Amazon S3 (storage)**
* **TurboQuant** — a custom zero-copy mmap index for the **static** authoritative corpus (see `docs/TurboQuant.md`)
* **LanceDB** — vector search for the **dynamic** user corpus
* **Tantivy** — BM25 keyword search
* **jina-embeddings-v5-text-nano-retrieval** — a local ONNX embedding model (512-dim), baked into the image

The system is designed for **RAG retrieval and document search workloads** with moderate traffic and burst elasticity.

The architecture emphasizes:

* **low infrastructure cost**
* **elastic scalability** (event-driven on Lambda, always-on on Fargate)
* **simplified operational management**

> **Note on "serverless".** The original design targeted a Lambda-only, fan-out-per-retriever
> topology. The implemented system is a single-process engine that runs all retrieval in-process,
> packaged as a **container image that runs unchanged on both Lambda and Fargate**. Sections below
> reflect the implemented system; the deployment topology is covered in
> [§22 Deployment Topology](#22-deployment-topology-docker-fargate--lambda).

---

# **2\. System Scope and Constraints**

This architecture intentionally limits its design scope to **S3 storage \+ Lambda compute**.

These choices introduce clear operational constraints.

---

## **Supported Workload Profile**

| Metric | Target |
| ----- | ----- |
| Average QPS | \~20 |
| Burst QPS | up to 500 |
| Latency SLA | ≤300 ms |
| Dataset size | ≤10M documents |
| Index size | ≤20–40 GB |
| Update latency | 1–5 minutes (NRT) |

---

## **Intended Workloads**

This architecture is suitable for:

* RAG retrieval pipelines  
* enterprise knowledge search  
* AI agent memory stores  
* internal document search  
* long-tail query workloads

Characteristics:

* low baseline traffic  
* occasional bursts  
* read-heavy workload

---

# **3\. Architectural Principles**

The system follows three design principles.

---

## **3.1 Compute–Storage Separation**

Data stored in **S3 object storage**.

Compute provided by **stateless Lambda functions**.

Storage → persistent  
Compute → ephemeral

Advantages:

* minimal baseline cost  
* elastic scaling  
* operational simplicity

---

## **3.2 Serverless Execution Model**

All search operations execute on demand.

request → Lambda invocation

No always-running infrastructure.

---

## **3.3 Near-Real-Time Indexing**

Updates are applied using **micro-batch indexing**, not real-time updates.

write → queue → batch index build

Expected update latency:

1–5 minutes  
---

# **4\. System Architecture**

High-level architecture. Retrieval is **not** a per-retriever Lambda fan-out; a single
query binary runs three retrievers in parallel **in-process** (`std::thread::scope`,
`src/query/router.rs`) and returns two result groups.

```
                +--------------------+
                |        Client      |
                +---------+----------+
                          |
                     Query API / HTTP
                          |
                  +-------v--------+
                  |  QueryRouter   |   one process (query image)
                  |  (Rust)        |   runs on Lambda OR Fargate
                  +-------+--------+
                          |
     +--------------------+--------------------+
     |                    |                    |   in-process parallel
 Static path         Vector path          Keyword path
 TurboQuant mmap      LanceDB              Tantivy (BM25)
 (authoritative)      (user corpus)        (user corpus)
     |                    |                    |
     |                    +---------+----------+
     |                              |
     |                        RRF fusion (rrf_k=60)
     |                              |
 static_chunks                 dynamic_chunks
     +--------------+---------------+
                    |
        SearchResponse { static_chunks, dynamic_chunks, ... }
```

The static path is returned separately; **RRF fuses only the vector + keyword (dynamic)
results**. Both groups return up to the retrieval window (`3 * top_k`) per path so an
upstream caller can assemble the designed `6 * top_k` LLM context.

Storage layer (Amazon S3):

```
index/_head                         # active version pointer (ETag CAS)
index/versions/<v>/manifest.json    # per-version manifest
lance/<v>/...                       # LanceDB dynamic dataset (per shard)
static/...                          # TurboQuant static index files
wal/YYYY/MM/DD/*.jsonl              # write-ahead log
```
---

# **5\. Storage Layout (S3)**

All persistent data resides in **Amazon S3**.

Example layout:

```
s3://search-system/
  index/
      _head                          # active version pointer (JSON)
      versions/
          42/manifest.json           # per-version manifest
          43/manifest.json
  lance/
      42/shard_0/ ...                # LanceDB dynamic dataset per shard
  static/
      turbo_static.bin               # TurboRecord512 records (mmap)
      turbo_static_meta.bin
      turbo_static_text.bin
      turbo_static_title.bin
      centroids.bin                  # quantization centroids
      projection.bin                 # JL projection matrix
  wal/
      2026/07/08/<segment>.jsonl     # write-ahead log (JSONL)
```

Components:

| Path | Purpose | Code |
| ----- | ----- | ----- |
| `index/_head` | Active index version pointer; updated via **ETag compare-and-swap** | `src/storage/head.rs`, `src/indexing/publisher.rs` |
| `index/versions/<v>/manifest.json` | Per-version manifest (`embedding_dim`, `document_count`, shards) | `src/models/index.rs` |
| `lance/<v>/` | LanceDB dynamic vector dataset (per shard) | `src/query/vector_searcher.rs` |
| `static/` | TurboQuant static index (mmap-loaded) | `src/index/mmap_index.rs`, `docs/TurboQuant.md` |
| `wal/` | Write-ahead log (JSONL, date-partitioned) | `src/write/wal.rs` |

The `_head` document (`ManifestHead`) holds `{ version_id, manifest_path, updated_at }`, where
`manifest_path` is the bucket-relative key `index/versions/<version_id>/manifest.json`, derived
from `version_id` and validated on both read and write.

---

# **6\. Query Execution Pipeline**

Search pipeline (`src/query/router.rs`):

```
client query
   |
embedding generation (512-dim; 2 retries, keyword-only fallback on failure)
   |
parallel retrieval (in-process, 3 threads)
   |-- static:  TurboQuant mmap scan  -> static results
   |-- vector:  LanceDB ANN           -> vector results  --\
   |-- keyword: Tantivy BM25          -> keyword results --+-- RRF fuse
   |                                                          |
   |                                                     dynamic_chunks
static_chunks <---------------------------------------------/
   |
optional filtering + metadata strip; truncate each group to 3*top_k
   |
SearchResponse { static_chunks, dynamic_chunks, counts, latency_ms, index_version }
```

When filters are present, the router **iteratively widens** the retrieval window (doubling
`top_k` up to 100) until enough post-filter dynamic results exist.
---

# **7\. Hybrid Retrieval**

The system runs **three** retrievers in parallel and returns two result groups: a **static**
group (authoritative corpus) and a **dynamic** group (user corpus, RRF-fused).

---

## **Static Search (TurboQuant)**

Implemented as a custom **zero-copy memory-mapped** index (`src/query/turbo_searcher.rs`),
not a database. Fixed-size `TurboRecord512` records (512-dim, quantized to ~208 bytes each) are
`mmap`-ed and brute-force scanned in parallel (`rayon`) with a bounded top-K heap. See
[`docs/TurboQuant.md`](TurboQuant.md) for the compression and scoring math.

Returns `static_chunks` (with `Citation` titles from the index).

---

## **Vector Search (LanceDB)**

Implemented using **LanceDB** over the dynamic user corpus (`src/query/vector_searcher.rs`).

Dataset table `documents` schema: `doc_id`, `embedding` (`FixedSizeList<Float32>`), `text`,
`metadata`. ANN query uses `DistanceType::Dot`. Returns `top_k` vector results.

---

## **Keyword Search (Tantivy)**

Implemented using **Tantivy** with default **BM25** scoring (`src/query/keyword_searcher.rs`).
Fields: `doc_id`, `text` (indexed + stored), `metadata` (stored). Returns `top_k` keyword results.
Requires a single shard.

---

# **8\. Hybrid Ranking**

The **vector** and **keyword** results are merged using **Reciprocal Rank Fusion (RRF)** into
`dynamic_chunks` (`src/query/ranker.rs`, `rrf_k = 60`). The **static** TurboQuant results are
**not** RRF-fused — they are returned as `static_chunks` unchanged.

Formula:

score \= Σ 1 / (k \+ rank)     // rank is 1-based; k = 60

Advantages:

* robust ranking

* simple implementation

* avoids score normalization

---

# **9\. Optional Reranking**

For higher retrieval accuracy, a reranker may be used.

However:

**Reranking is not performed inside Lambda.**

Instead:

Lambda → GPU inference endpoint

Example services:

* SageMaker Serverless Inference

* dedicated GPU inference service

Pipeline:

retrieve top 50  
   |  
send to reranker  
   |  
return top 10

Expected latency:

100–200 ms  
---

# **10\. Index Sharding**

To support larger datasets, the index is partitioned.

Shard rule:

shard\_id \= hash(doc\_id) % N

Typical configuration:

N \= 8–16 shards

Shard layout:

index/  
  v42/  
    shard\_0  
    shard\_1

This avoids excessive Lambda fan-out.

---

# **11\. Lambda Index Cache**

Lambda uses /tmp storage for index caching.

Limit:

10 GB

Cache layout:

/tmp/index/  
/tmp/lance/

Cold start behavior:

download index from S3

Warm container:

reuse cached index

Expected warm latency:

50–150 ms  
---

# **12\. Near-Real-Time Indexing**

The ingestion pipeline uses **batch indexing**.

Pipeline:

client  
   |  
write API  
   |  
SQS queue  
   |  
Index Builder  
   |  
build new index  
   |  
publish version

Batch window:

1–5 minutes  
---

# **13\. Versioned Index Publishing**

Indexes are versioned.

Structure:

index/  
   \_head  
   v42  
   v43

Publishing process:

upload new version  
update \_head

Advantages:

* atomic index switch

* rollback capability

* zero downtime

---

# **14\. Consistency Model**

The system provides **near-real-time consistency**.

Guarantee:

writes become searchable after next index publish

Optional improvement:

search index  
\+  
scan recent WAL

Provides read-after-write for very recent documents.

---

# **15\. Performance Expectations**

Typical query latency:

| Stage | Latency |
| ----- | ----- |
| embedding generation | 20–40 ms |
| vector search | 40–80 ms |
| BM25 search | 10–30 ms |
| fusion | \<5 ms |

Total:

100–200 ms typical  
≤300 ms SLA  
---

# **16\. Cost Model**

Example workload:

10M documents  
20 QPS average

Monthly cost estimate:

| Service | Cost |
| ----- | ----- |
| S3 storage | $1–3 |
| Lambda compute | $10–25 |
| SQS | \<$1 |
| CloudWatch | $2 |

Total estimated cost:

$15–30 / month  
---

# **17\. Operational Constraints**

This architecture has several hard constraints.

---

## **Lambda Storage Limit**

Maximum /tmp:

10 GB

Implication:

index must fit within cache limit  
---

## **Lambda Concurrency**

Default AWS account limit:

1000 concurrent executions

Large shard fan-out may hit this limit.

---

## **S3 Request Latency**

Typical S3 read latency:

5–20 ms

Frequent small reads should be minimized.

---

## **Index Update Cost**

Frequent index rebuilds increase:

S3 PUT cost  
Lambda execution time

Therefore batching is required.

---

# **18\. Monitoring**

Key metrics:

query latency  
cold start rate  
cache hit ratio  
S3 request count  
index build duration

Tools:

* CloudWatch  
* AWS X-Ray  
* OpenTelemetry

---

# **19\. Security**

Security model:

| Layer | Mechanism |
| ----- | ----- |
| authentication | IAM |
| storage encryption | S3 SSE-KMS |
| network | VPC endpoints |
| tenant isolation | S3 prefix |

---

# **20\. Summary**

This architecture implements a **serverless hybrid search system** using:

S3 storage  
\+  
Lambda compute  
\+  
LanceDB vector search  
\+  
Tantivy keyword search

Key properties:

serverless  
low-cost  
near-real-time indexing  
hybrid retrieval

The system is optimized for:

* moderate QPS workloads  
* burst traffic  
* AI retrieval pipelines

while maintaining **very low operational overhead and infrastructure cost**.

---

# **21\. Embedding Layer**

The system supports two embedding providers, selected per deployment via environment variable.

---

## **Providers**

| Provider | Env value | Description |
| -------- | --------- | ----------- |
| Fixed | `fixed` | Deterministic stub vector; all documents and queries share the same vector. Used in CI and unit tests. |
| LTEmbed | `ltembed` | Real model inference: `jinaai/jina-embeddings-v5-text-nano-retrieval`, **512-dim** (768-dim raw, Matryoshka-truncated and L2-re-normalized to 512 by the LTEmbed ONNX engine; last-token pooling; `Query: ` / `Document: ` prefixes applied by the engine per input kind — build side embeds Documents, query side embeds Queries). |

The provider is configured independently for the build pipeline (`LTSEARCH_BUILD_EMBEDDING_PROVIDER`) and the query path (`LTSEARCH_QUERY_EMBEDDING_PROVIDER`). Both must use the same provider and dimension for a given index version. The static TurboQuant path is pinned to 512-dim (`TurboRecord512`), matching the LTEmbed output.

LTEmbed configuration per side is two env vars: `LTSEARCH_{BUILD,QUERY}_LTEMBED_BUNDLE_DIR` (directory holding `tokenizer.json` + `build-info.json`, and optionally `libonnxruntime.so`) and `LTSEARCH_{BUILD,QUERY}_LTEMBED_MODEL_PATH` (the `model.ort` weights). Pooling, prefixes, and output dimension are owned by the engine and its bundle metadata — there are no pooling/prefix env vars.

---

## **LTEmbed Asset Delivery**

Model files are too large for Lambda Layers and impractical to download at cold-start. Instead, an **ort bundle** — a public `minimal-ort-builder` release asset (q4f16 `model.ort` for jina-embeddings-v5-text-nano-retrieval with a matching minimal-build `libonnxruntime.so`) — is **baked into the Lambda container image** at build time. `sam/builder.Dockerfile` pins the exact bundle version via the `LTEMBED_BUNDLE_URL` build arg (default: `minimal-ort-builder` v1.0.9); bumping the model is a one-line change to that default.

Build flow:

```
docker build --build-arg LTEMBED_MODE=real \
             --build-arg LTEMBED_BUNDLE_URL=<ort-bundle tarball> \
             sam/builder.Dockerfile
  → downloads and unpacks the bundle into /ltembed-assets/
  → compiles Rust binaries with --features ltembed
    (against the LTEmbed checkout staged at .sam-local-deps/LTEmbed)

sam build
  → index_builder_lambda.Dockerfile: COPY --from=builder /ltembed-assets /ltembed-assets
  → query_lambda.Dockerfile:         COPY --from=builder /ltembed-assets /ltembed-assets
```

The default `LTEMBED_MODE=stub` skips the download and satisfies the ltembed git dependency with the vendored stub crate; binaries are built without the `ltembed` feature and use the `fixed` provider. This is the CI default. `LTEMBED_MODE=real` downloads the pinned default `LTEMBED_BUNDLE_URL` (overridable to bump the model version) and fails the build loudly only if that URL is explicitly emptied or unreachable.

Bundle files inside Lambda containers:

| File | Container path |
| ---- | -------------- |
| `model.ort` | `/ltembed-assets/model.ort` |
| `tokenizer.json` | `/ltembed-assets/tokenizer.json` |
| `build-info.json` | `/ltembed-assets/build-info.json` |
| `libonnxruntime.so` | `/ltembed-assets/libonnxruntime.so` (resolved automatically by the engine; `ort` is built with `load-dynamic`) |

---

## **Dimension Validation**

The build event carries an `embedding_dim` field. The index builder validates that the configured embedding dimension matches before writing the LanceDB dataset. This prevents silent dimension mismatches when switching providers.

---

## **Performance**

With `ltembed`, embedding generation adds latency versus the fixed stub:

| Stage | Fixed | LTEmbed |
| ----- | ----- | ------- |
| Embedding generation | ~0 ms | 20–40 ms |
| Total query latency | 50–150 ms (warm) | 70–190 ms (warm) |

The model is loaded once per container lifetime (warm path reuse). On Fargate this is once per
task; on Lambda, once per warm container.

---

# **22\. Deployment Topology (Docker: Fargate + Lambda)**

> **Status: target/planned.** The serving path already ships as a Lambda **container image**.
> This section documents the intended unified topology so one image per component runs unchanged
> on **both AWS Fargate and AWS Lambda**. The concrete runbook lives in
> [`docs/deployment.md`](deployment.md); the code/Dockerfile/template changes are follow-up work.

## Why Docker for both

The embedding model assets (~140 MB: `model.ort` ~118 MB + `tokenizer.json` ~16 MB +
`libonnxruntime.so` ~4.6 MB) exceed the Lambda **Layer** limit, so they are baked into a
**container image** (already the case). Running the same image on **Fargate** additionally gives
an always-on service that loads the model once (no per-invoke cold start) and sidesteps Lambda's
15-minute / 10 GB `/tmp` limits for the index builder.

## One image, two runtimes — via the Lambda Web Adapter

Each component's binary becomes a plain **HTTP server** on `0.0.0.0:8080`
(query → `POST /query` + `GET /health`; write → `POST /write`, `POST /delete`;
index_builder → `POST /build` + health). The transport-agnostic cores already exist
(`handle_search_request`, `handle_write_request`, `handle_build_request`), so this is a thin
transport swap.

The [**AWS Lambda Web Adapter**](https://github.com/awslabs/aws-lambda-web-adapter) is baked in as
a Lambda extension:

```dockerfile
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.x /lambda-adapter /opt/extensions/lambda-adapter
```

* On **Lambda**, the extension boots the app and bridges API-Gateway / SQS events to
  `http://localhost:8080`.
* On **Fargate/ECS**, the extension file is inert; the container simply runs the HTTP server.

Relevant env knobs: `AWS_LWA_PORT=8080`, `AWS_LWA_READINESS_CHECK_PATH=/health`, plus event
pass-through config for the SQS-driven builder.

## Image base and unification

Move each runtime image from `public.ecr.aws/lambda/provided:al2023-arm64` to a plain
`public.ecr.aws/amazonlinux/amazonlinux:2023` (arm64) base, reusing the existing multi-stage
`sam/builder.Dockerfile` compile stage. The divergent top-level `Dockerfile` (x86, bakes `static/`
+ `CMD [bootstrap]`) is folded into this arm64 lineage so the static-index baking
(`/app/static`, `LTSEARCH_QUERY_STATIC_DIR`) has a single source of truth.

## Platform mapping (all three components)

| Component | Fargate | Lambda |
| --- | --- | --- |
| query | ECS service (always-on, behind ALB) | Function behind API Gateway |
| write | ECS service | Function behind API Gateway |
| index_builder | ECS task (queue-driven) | Function with SQS EventSource |

## Architecture portability caveat

`ort` is built with `load-dynamic`, so the compiled binary + `libonnxruntime.so` are decoupled and
portable **as long as the CPU architecture matches**. The pinned bundle is `linux-arm64`, so both
Fargate and Lambda must run on **arm64 (Graviton)**. Targeting x86_64 Fargate would require an
x86_64 `minimal-ort-builder` bundle and a matching `LTEMBED_BUNDLE_URL`.
