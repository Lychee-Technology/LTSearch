## **Serverless Hybrid Search Engine Architecture**

---

# **1\. Overview**

This document describes the architecture of a **serverless hybrid search system** built using:

* **Rust runtime**  
* **AWS Lambda (compute)**  
* **Amazon S3 (storage)**  
* **LanceDB (vector search)**  
* **Tantivy (BM25 keyword search**

The system is designed for **RAG retrieval and document search workloads** with moderate traffic and burst elasticity.

The architecture emphasizes:

* **low infrastructure cost**  
* **serverless scalability**  
* **simplified operational management**

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

High-level architecture:

               \+--------------------+  
                |        Client      |  
                \+---------+----------+  
                          |  
                     Query API  
                          |  
                  \+-------v--------+  
                  | Query Router   |  
                  | Rust Lambda    |  
                  \+-------+--------+  
                          |  
        \+-----------------+-----------------+  
        |                                   |  
   Vector Search Lambda              Keyword Search Lambda  
       (LanceDB)                         (Tantivy)  
        |                                   |  
        \+-----------------+-----------------+  
                          |  
                    Hybrid Ranker  
                          |  
                       Response

Storage layer:

               \+------------------+  
                |        S3        |  
                \+------------------+  
                 |              |  
            Lance dataset   Tantivy index  
---

# **5\. Storage Layout (S3)**

All persistent data resides in **Amazon S3**.

Example layout:

s3://search-system/

  index/  
      \_head  
      v42/  
      v43/

  lance/  
      v42/

  wal/  
      000001.log  
      000002.log

  docs/  
      documents.parquet

Components:

| Path | Purpose |
| ----- | ----- |
| index | Tantivy keyword index |
| lance | vector dataset |
| wal | ingestion log |
| docs | metadata storage |

---

# **6\. Query Execution Pipeline**

Search pipeline:

client query  
   |  
embedding generation  
   |  
parallel retrieval  
   |  
vector results  
keyword results  
   |  
fusion ranking  
   |  
response  
---

# **7\. Hybrid Retrieval**

The system performs two retrieval steps.

---

## **Vector Search**

Implemented using **LanceDB**.

Dataset schema:

doc\_id  
embedding  
text  
metadata

Vector search returns:

top\_k\_vector\_results  
---

## **Keyword Search**

Implemented using **Tantivy**.

Uses BM25 scoring.

Example query:

"vector database lambda"

Returns:

top\_k\_keyword\_results  
---

# **8\. Hybrid Ranking**

Results are merged using **Reciprocal Rank Fusion (RRF)**.

Formula:

score \= Σ 1 / (k \+ rank)

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
