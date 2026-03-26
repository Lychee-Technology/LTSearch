# System Design Document: Zero-Copy Mmap Vector Search Engine with TurboQuant Compression

## 1. Introduction
### 1.1 Purpose
This document details the architecture for a highly optimized, pure in-memory vector search engine designed for read-heavy, low-frequency update environments (e.g., enterprise policies, legal documents). The system leverages the `e5-small` embedding model and the state-of-the-art **TurboQuant** extreme compression algorithm, bypassing traditional Vector Databases in favor of a zero-copy memory-mapped (`mmap`) architecture in Rust.

### 1.2 Background & Motivation
Traditional vector databases incur significant memory overhead and serialization/deserialization costs. By utilizing TurboQuant's `TurboQuant_prod` algorithm , which is optimized for unbiased inner product estimation , we can compress high-dimensional vectors to a minimal footprint (e.g., 3 bits per dimension). Storing these fixed-size compressed representations in a flat binary file allows the OS Page Cache to manage memory via `mmap`, achieving near-instant startup times and zero-copy deserialization.

### 1.3 Goals
* **Extreme Compression:** Compress 384-dimensional `e5-small` vectors to ~156 bytes per document using TurboQuant's two-stage algorithm.
* **Zero-Copy Retrieval:** Utilize memory-mapped files to eliminate memory allocation and parsing overhead during search.
* **High Performance:** Execute brute-force parallel exact search in memory using CPU caches and SIMD instructions.
* **Minimal Dependency:** Operate without external database daemon processes.

---

## 2. System Architecture
The architecture is divided into two primary subsystems: the **Offline Indexer** (Write Path) and the **Online Search Server** (Read Path).



### 2.1 Offline Indexer (Write Path)
This pipeline is triggered when new documents are added or updated. It follows an append-only processing flow:
1.  **Text Processing:** Documents are chunked and prefixed with `passage: ` (specific to the `e5-small` requirement).
2.  **Embedding Generation:** The Rust `candle` framework executes the `e5-small` model to generate a 384-dimensional dense float32 vector ($x$).
3.  **TurboQuant Compression:** The vector $x$ passes through the `TurboQuant_prod` pipeline.
    * *Stage 1 (MSE Quantization):* The vector is randomly rotated to induce a Beta distribution on its coordinates , then quantized using $b-1$ bits.
    * *Stage 2 (QJL Transform):* The residual error vector is quantized using the Quantized Johnson-Lindenstrauss (QJL) transform down to 1 single sign bit per coordinate.
4.  **Binary Serialization:** The resulting compressed data structure is appended to a flat, continuous `.bin` file on disk.

### 2.2 Online Search Server (Read Path)
This pipeline is optimized for ultra-low latency:
1.  **Memory Mapping:** Upon startup, the service memory-maps the `.bin` file into a continuous byte slice, casting it directly to an array of strictly defined structs.
2.  **Query Embedding:** The user's query is prefixed with `query: ` and passed through the `candle` `e5-small` model to generate an uncompressed float32 vector ($y$).
3.  **Parallel Scoring:** The system uses parallel iterators (e.g., via `rayon`) to scan the mapped memory array. For each compressed vector, it calculates the unbiased inner product score using the TurboQuant estimation formula.
4.  **Top-K Aggregation:** The highest-scoring Document IDs are collected using a min-heap and returned to the application layer.

---

## 3. Data Structure & Memory Layout
The cornerstone of this zero-copy architecture is the fixed-size, strictly aligned C-representation (`repr(C)`) layout of the compressed vector. 

Assuming the target bit-width $b=3$  for $d=384$ (`e5-small`), the structure per document is designed as follows:

| Field Name | Description | Size (Bytes) | Derivation |
| :--- | :--- | :--- | :--- |
| `doc_id` | Unique identifier for the document chunk. | 8 bytes | 64-bit unsigned integer |
| `idx` | Phase 1 MSE quantization pointers. | 96 bytes | 384 dims $\times$ 2 bits ($b-1$)  |
| `qjl` | Phase 2 residual sign bits (+1 or -1). | 48 bytes | 384 dims $\times$ 1 bit  |
| `gamma` | The L2 norm ($\|\|r\|\|_2$) of the residual vector. | 4 bytes | 32-bit floating point |

**Total Memory Footprint:** **156 Bytes per vector.**
*A knowledge base of 1,000,000 documents will result in a single `~156 MB` binary file.*

---

## 4. Component Design Details

### 4.1 TurboQuant Mathematics Integration
To search against the `mmap` struct, the inner product between the high-precision query vector $y$ and the compressed representation must be calculated dynamically. 

The custom distance metric implements the `TurboQuant_prod` estimator:

$$
{Score} = \langle y, \tilde{x}_{mse} \rangle + \gamma \cdot \langle y, Q^{-1}_{qjl}(qjl) \rangle
$$

Where:
* $\tilde{x}_{mse}$ is reconstructed on-the-fly using the pre-computed 1D centroids and the stored `idx`.
* $Q^{-1}_{qjl}(qjl)$ is reconstructed using the stored `qjl` sign bits and the pre-computed global random projection matrix $S$.



### 4.2 Handling Mutability (Updates & Deletions)
Because `.bin` files mapped via `mmap` are inherently hostile to mid-file insertions and deletions, the system utilizes a **LSM-tree (Log-Structured Merge-tree) inspired approach**:
1.  **Main Segment:** The immutable `mmap` binary file containing the bulk of the data.
2.  **Delta Segment (In-Memory):** A standard Rust `HashMap` containing newly added or updated vectors.
3.  **Tombstone Bitmap:** A bit-array indicating which `doc_id`s in the Main Segment have been deleted or superseded.
4.  **Compaction:** A background asynchronous task that periodically (e.g., nightly) merges the Delta Segment and Main Segment, purges tombstones, and writes a new continuous `.bin` file to disk, seamlessly hot-swapping the `mmap` pointer.

---

## 5. Performance Characteristics & Resource Analysis

### 5.1 Memory Efficiency
By eliminating vector database indexing structures (like HNSW graphs or IVF inverted lists) and storing data in a highly compressed format, memory usage is delegated entirely to the OS Page Cache. User-space memory allocation is virtually zero during retrieval.

### 5.2 Computational Efficiency
Because the vectors are compressed independently (Data-oblivious) , the index building process is dramatically sped up. During retrieval, the linear scan is highly predictable for the CPU branch predictor and cache prefetcher. The dot products between the query vector $y$ and the $qjl$ bitmasks can be heavily optimized using SIMD (Single Instruction, Multiple Data) instructions.

### 5.3 System Limitations
* **Query-Time Compute:** Brute-force scanning scales linearly $O(N)$. While SIMD and parallelization make this exceptionally fast for datasets under 10 million records, it will eventually bottleneck on CPU cycles for billion-scale datasets compared to Approximate Nearest Neighbor (ANN) graph traversals.
* **Static Matrices:** The global rotation matrix $\Pi$ and projection matrix $S$ must remain consistent. Changing them requires a full re-indexing of the `.bin` file.
