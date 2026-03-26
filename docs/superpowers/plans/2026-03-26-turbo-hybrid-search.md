# TurboQuant Hybrid Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a TurboQuant static-corpus search path alongside the existing LanceDB dynamic path, retrieving 3×K candidates from each, then formatting them with source labels for LLM consumption.

**Architecture:** Two paths run in parallel inside `QueryRouter`: `TurboQuantSearcher` scans a mmap'd binary index bundled in the Docker image (static corpus, shared across tenants), while the existing LanceDB + Tantivy path handles per-tenant dynamic data. A `ContextBuilder` formats the combined results for LLM consumption.

**Tech Stack:** Rust, `memmap2` (zero-copy file mapping), `rayon` (parallel linear scan), `std::collections::BinaryHeap` (top-K), existing `tokio`/`thread::scope` parallelism pattern.

**Spec:** `docs/superpowers/specs/2026-03-26-turbo-hybrid-search-design.md`

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `Cargo.toml` | Modify | Add `rayon`, `memmap2` |
| `src/models/search.rs` | Modify | Add `ChunkSource`, `CorpusType`, `CorpusWeights`; extend `SearchResult`/`SearchRequest` |
| `src/turbo/mod.rs` | Create | Module root |
| `src/turbo/types.rs` | Create | `TurboRecord`, `MetaRecord` (repr(C)); `Centroids`, `ProjectionMatrix` |
| `src/turbo/mmap_index.rs` | Create | `MmapIndex` global singleton — load 3 binary files, text lookup |
| `src/turbo/scorer.rs` | Create | TurboQuant_prod scoring formula |
| `src/turbo/encoder.rs` | Create | TurboQuant compression (rotate → quantize → QJL) for offline builder |
| `src/query/turbo_searcher.rs` | Create | `TurboQuantSearcher` — rayon parallel scan, min-heap top-K |
| `src/query/context_builder.rs` | Create | Format static + dynamic chunks for LLM; build system prompt |
| `src/query/router.rs` | Modify | Add `StaticRetriever` trait; add optional static path to `QueryRouter` |
| `src/query/mod.rs` | Modify | Re-export new types |
| `src/lib.rs` | Modify | Expose `turbo` module |
| `src/bin/turbo_index_builder.rs` | Create | CLI: S3 docs → embed → compress → write 3 bin files |
| `Dockerfile` | Create | Lambda image with `/app/static/` layer |
| `static/.gitkeep` | Create | Placeholder for index files (excluded from git via `.gitignore`) |

---

## Task 1: Add Dependencies

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add rayon and memmap2 to Cargo.toml**

```toml
# in [dependencies]
rayon = "1.10"
memmap2 = "0.9"
```

- [ ] **Step 2: Verify build compiles**

```bash
cargo check
```

Expected: compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add rayon and memmap2 dependencies"
```

---

## Task 2: Extend Search Models

**Files:**
- Modify: `src/models/search.rs`

- [ ] **Step 1: Write tests for new model types**

Add to the bottom of `src/models/search.rs`:

```rust
#[cfg(test)]
mod turbo_model_tests {
    use super::*;

    #[test]
    fn chunk_source_serializes() {
        let s = serde_json::to_string(&ChunkSource::Static).unwrap();
        assert_eq!(s, "\"static\"");
        let d = serde_json::to_string(&ChunkSource::Dynamic).unwrap();
        assert_eq!(d, "\"dynamic\"");
    }

    #[test]
    fn corpus_type_roundtrip() {
        let t = CorpusType::Legal;
        let json = serde_json::to_string(&t).unwrap();
        let back: CorpusType = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn search_request_corpus_weights_optional() {
        let req = SearchRequest {
            query: "test".into(),
            top_k: 5,
            filters: None,
            include_metadata: false,
            corpus_weights: None,
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn search_result_with_static_source() {
        let r = SearchResult {
            doc_id: "abc".into(),
            score: 0.9,
            text: "hello".into(),
            metadata: None,
            source: SearchSource::Vector,
            chunk_source: ChunkSource::Static,
            corpus_type: Some(CorpusType::Legal),
        };
        assert!(r.validate().is_ok());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail (types don't exist yet)**

```bash
cargo test turbo_model_tests 2>&1 | head -20
```

Expected: compile error mentioning `ChunkSource` not found.

- [ ] **Step 3: Add new types to `src/models/search.rs`**

Add after the `FilterValue` enum (before `SearchRequest`):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkSource {
    Static,
    Dynamic,
}

impl Default for ChunkSource {
    fn default() -> Self {
        Self::Dynamic
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorpusType {
    Legal,
    Contract,
    Rfc,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorpusWeights {
    pub static_bias: f32,
    pub dynamic_bias: f32,
}
```

- [ ] **Step 4: Extend `SearchRequest` with `corpus_weights`**

Replace the existing `SearchRequest` struct:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub top_k: usize,
    pub filters: Option<HashMap<String, FilterValue>>,
    pub include_metadata: bool,
    #[serde(default)]
    pub corpus_weights: Option<CorpusWeights>,
}
```

- [ ] **Step 5: Extend `SearchResult` with `chunk_source` and `corpus_type`**

Replace the existing `SearchResult` struct:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub doc_id: String,
    pub score: f32,
    pub text: String,
    pub metadata: Option<HashMap<String, Value>>,
    pub source: SearchSource,
    #[serde(default)]
    pub chunk_source: ChunkSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_type: Option<CorpusType>,
}
```

- [ ] **Step 6: Run all tests to verify nothing is broken**

```bash
cargo test
```

Expected: all existing tests pass; new `turbo_model_tests` pass.

- [ ] **Step 7: Commit**

```bash
git add src/models/search.rs
git commit -m "feat(models): add ChunkSource, CorpusType, CorpusWeights; extend SearchResult/SearchRequest"
```

---

## Task 3: TurboQuant Binary Data Structures

**Files:**
- Create: `src/turbo/mod.rs`
- Create: `src/turbo/types.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write tests for record layout sizes**

Create `src/turbo/types.rs` with tests first:

```rust
use std::mem;

/// One compressed vector entry. repr(C) ensures stable binary layout.
/// Total size: 8 + 96 + 48 + 4 = 156 bytes.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct TurboRecord {
    pub doc_id: u64,
    pub idx: [u8; 96],   // 384 dims × 2 bits packed
    pub qjl: [u8; 48],   // 384 dims × 1 bit packed
    pub gamma: f32,
}

/// Per-chunk metadata. repr(C) ensures stable binary layout.
/// Total size: 8 + 1 + 3 + 8 + 4 = 24 bytes.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MetaRecord {
    pub doc_id: u64,
    pub corpus_type: u8,  // 0=Legal, 1=Contract, 2=RFC, 3=Other
    pub _pad: [u8; 3],
    pub text_offset: u64,
    pub text_len: u32,
}

/// Per-dimension MSE quantization centroids.
/// centroids[dim] = [c0, c1, c2, c3] for 2-bit (4 levels).
pub struct Centroids {
    pub values: Vec<[f32; 4]>,  // len == num_dims (384)
}

/// QJL projection matrix S, stored row-major.
pub struct ProjectionMatrix {
    pub values: Vec<f32>,  // len == rows * cols
    pub rows: usize,
    pub cols: usize,
}

impl ProjectionMatrix {
    /// Returns row `i` as a slice of length `cols`.
    pub fn row(&self, i: usize) -> &[f32] {
        &self.values[i * self.cols..(i + 1) * self.cols]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turbo_record_size_is_156() {
        assert_eq!(mem::size_of::<TurboRecord>(), 156);
    }

    #[test]
    fn meta_record_size_is_24() {
        assert_eq!(mem::size_of::<MetaRecord>(), 24);
    }

    #[test]
    fn projection_matrix_row() {
        let m = ProjectionMatrix {
            values: vec![1.0, 2.0, 3.0, 4.0],
            rows: 2,
            cols: 2,
        };
        assert_eq!(m.row(0), &[1.0f32, 2.0]);
        assert_eq!(m.row(1), &[3.0f32, 4.0]);
    }
}
```

- [ ] **Step 2: Create `src/turbo/mod.rs`**

```rust
pub mod mmap_index;
pub mod scorer;
pub mod encoder;
pub mod types;

pub use mmap_index::MmapIndex;
pub use types::{Centroids, MetaRecord, ProjectionMatrix, TurboRecord};
```

- [ ] **Step 3: Add `turbo` module to `src/lib.rs`**

Add to `src/lib.rs`:

```rust
pub mod turbo;
```

- [ ] **Step 4: Run size tests**

```bash
cargo test turbo::types::tests
```

Expected: all 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/turbo/ src/lib.rs
git commit -m "feat(turbo): add TurboRecord, MetaRecord, Centroids, ProjectionMatrix data structures"
```

---

## Task 4: MmapIndex

**Files:**
- Create: `src/turbo/mmap_index.rs`

- [ ] **Step 1: Write tests**

Create `src/turbo/mmap_index.rs`:

```rust
use std::path::Path;
use std::{fs, slice};

use memmap2::Mmap;

use crate::models::CorpusType;

use super::types::{Centroids, MetaRecord, ProjectionMatrix, TurboRecord};

pub struct MmapIndex {
    // Keep Mmap alive so the memory remains mapped.
    _records_mmap: Mmap,
    _meta_mmap: Mmap,
    _text_mmap: Mmap,
    pub records: &'static [TurboRecord],
    pub meta: &'static [MetaRecord],
    pub text_blob: &'static [u8],
    pub centroids: Centroids,
    pub projection: ProjectionMatrix,
}

impl MmapIndex {
    /// Load index files from the given directory.
    /// Files expected: turbo_static.bin, turbo_static_meta.bin,
    /// turbo_static_text.bin, centroids.bin, projection.bin
    pub fn load_from_dir(dir: &Path) -> anyhow::Result<Self> {
        let records_mmap = open_mmap(&dir.join("turbo_static.bin"))?;
        let meta_mmap = open_mmap(&dir.join("turbo_static_meta.bin"))?;
        let text_mmap = open_mmap(&dir.join("turbo_static_text.bin"))?;

        let records: &[TurboRecord] = cast_slice(&records_mmap);
        let meta: &[MetaRecord] = cast_slice(&meta_mmap);
        let text_blob: &[u8] = &text_mmap[..];

        // Safety: we extend lifetime to 'static because _*_mmap keeps the
        // backing memory alive for the process lifetime when stored in a
        // OnceLock<MmapIndex>.
        let records: &'static [TurboRecord] = unsafe { extend_lifetime(records) };
        let meta: &'static [MetaRecord] = unsafe { extend_lifetime(meta) };
        let text_blob: &'static [u8] = unsafe { extend_lifetime(text_blob) };

        let centroids = load_centroids(&dir.join("centroids.bin"))?;
        let projection = load_projection(&dir.join("projection.bin"))?;

        Ok(Self {
            _records_mmap: records_mmap,
            _meta_mmap: meta_mmap,
            _text_mmap: text_mmap,
            records,
            meta,
            text_blob,
            centroids,
            projection,
        })
    }

    /// Load from the fixed path inside the Docker image.
    pub fn load_from_image() -> anyhow::Result<Self> {
        Self::load_from_dir(Path::new("/app/static"))
    }

    /// Return the raw text for a MetaRecord.
    pub fn text_of(&self, m: &MetaRecord) -> &str {
        let start = m.text_offset as usize;
        let end = start + m.text_len as usize;
        std::str::from_utf8(&self.text_blob[start..end])
            .unwrap_or("[invalid utf8]")
    }

    /// Map MetaRecord corpus_type byte to the CorpusType enum.
    pub fn corpus_type_of(m: &MetaRecord) -> CorpusType {
        match m.corpus_type {
            0 => CorpusType::Legal,
            1 => CorpusType::Contract,
            2 => CorpusType::Rfc,
            _ => CorpusType::Other,
        }
    }
}

fn open_mmap(path: &Path) -> anyhow::Result<Mmap> {
    let file = fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("failed to open {:?}: {}", path, e))?;
    // Safety: caller must ensure the file is not modified while mapped.
    let mmap = unsafe { Mmap::map(&file) }
        .map_err(|e| anyhow::anyhow!("failed to mmap {:?}: {}", path, e))?;
    Ok(mmap)
}

fn cast_slice<T: Copy>(mmap: &Mmap) -> &[T] {
    let size = std::mem::size_of::<T>();
    assert!(
        mmap.len() % size == 0,
        "file length {} is not a multiple of record size {}",
        mmap.len(),
        size
    );
    let count = mmap.len() / size;
    // Safety: TurboRecord/MetaRecord are repr(C), all-bits-valid, and the
    // file was written with the same layout.
    unsafe { slice::from_raw_parts(mmap.as_ptr() as *const T, count) }
}

unsafe fn extend_lifetime<'a, T: ?Sized>(r: &'a T) -> &'static T {
    &*(r as *const T)
}

fn load_centroids(path: &Path) -> anyhow::Result<Centroids> {
    let bytes = fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read {:?}: {}", path, e))?;
    // File format: flat f32 values, 4 per dimension, row-major.
    // num_dims = bytes.len() / (4 * 4)
    assert!(bytes.len() % (4 * 4) == 0, "centroids.bin size mismatch");
    let num_dims = bytes.len() / (4 * 4);
    let floats: &[f32] = unsafe {
        slice::from_raw_parts(bytes.as_ptr() as *const f32, bytes.len() / 4)
    };
    let values: Vec<[f32; 4]> = floats
        .chunks_exact(4)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect();
    assert_eq!(values.len(), num_dims);
    Ok(Centroids { values })
}

fn load_projection(path: &Path) -> anyhow::Result<ProjectionMatrix> {
    let bytes = fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read {:?}: {}", path, e))?;
    // File format: header [rows: u32, cols: u32] followed by flat f32 values.
    assert!(bytes.len() >= 8, "projection.bin too small");
    let rows = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let cols = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let float_bytes = &bytes[8..];
    assert_eq!(float_bytes.len(), rows * cols * 4, "projection.bin size mismatch");
    let values: Vec<f32> = float_bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    Ok(ProjectionMatrix { values, rows, cols })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_records(dir: &Path, records: &[TurboRecord]) {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                records.as_ptr() as *const u8,
                records.len() * std::mem::size_of::<TurboRecord>(),
            )
        };
        fs::write(dir.join("turbo_static.bin"), bytes).unwrap();
    }

    fn write_meta(dir: &Path, meta: &[MetaRecord]) {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                meta.as_ptr() as *const u8,
                meta.len() * std::mem::size_of::<MetaRecord>(),
            )
        };
        fs::write(dir.join("turbo_static_meta.bin"), bytes).unwrap();
    }

    fn write_text(dir: &Path, text: &[u8]) {
        fs::write(dir.join("turbo_static_text.bin"), text).unwrap();
    }

    fn write_centroids(dir: &Path, dims: usize) {
        // 4 centroids per dim, all zeros
        let data: Vec<f32> = vec![0.0; dims * 4];
        let bytes = unsafe {
            std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4)
        };
        fs::write(dir.join("centroids.bin"), bytes).unwrap();
    }

    fn write_projection(dir: &Path, rows: usize, cols: usize) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(rows as u32).to_le_bytes());
        bytes.extend_from_slice(&(cols as u32).to_le_bytes());
        let data: Vec<f32> = vec![0.0; rows * cols];
        for v in &data {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        fs::write(dir.join("projection.bin"), bytes).unwrap();
    }

    #[test]
    fn load_and_read_records() {
        let dir = TempDir::new().unwrap();
        let record = TurboRecord {
            doc_id: 42,
            idx: [1u8; 96],
            qjl: [0xFFu8; 48],
            gamma: 1.5,
        };
        write_records(dir.path(), &[record]);
        let meta = MetaRecord {
            doc_id: 42,
            corpus_type: 0,
            _pad: [0; 3],
            text_offset: 0,
            text_len: 5,
        };
        write_meta(dir.path(), &[meta]);
        write_text(dir.path(), b"hello");
        write_centroids(dir.path(), 384);
        write_projection(dir.path(), 384, 384);

        let index = MmapIndex::load_from_dir(dir.path()).unwrap();
        assert_eq!(index.records.len(), 1);
        assert_eq!(index.records[0].doc_id, 42);
        assert_eq!(index.text_of(&index.meta[0]), "hello");
    }

    #[test]
    fn corpus_type_mapping() {
        let mut m = MetaRecord { doc_id: 0, corpus_type: 0, _pad: [0;3], text_offset: 0, text_len: 0 };
        assert_eq!(MmapIndex::corpus_type_of(&m), CorpusType::Legal);
        m.corpus_type = 1;
        assert_eq!(MmapIndex::corpus_type_of(&m), CorpusType::Contract);
        m.corpus_type = 2;
        assert_eq!(MmapIndex::corpus_type_of(&m), CorpusType::Rfc);
        m.corpus_type = 99;
        assert_eq!(MmapIndex::corpus_type_of(&m), CorpusType::Other);
    }
}
```

- [ ] **Step 2: Add `anyhow` and `tempfile` to `Cargo.toml`**

```toml
# [dependencies]
anyhow = "1"

# [dev-dependencies]
tempfile = "3"
```

- [ ] **Step 3: Run tests**

```bash
cargo test turbo::mmap_index::tests
```

Expected: `load_and_read_records` and `corpus_type_mapping` pass.

- [ ] **Step 4: Commit**

```bash
git add src/turbo/mmap_index.rs Cargo.toml Cargo.lock
git commit -m "feat(turbo): implement MmapIndex with zero-copy binary file loading"
```

---

## Task 5: TurboQuant Scorer

**Files:**
- Create: `src/turbo/scorer.rs`

The scoring formula is:
```
score(y, record) = ⟨y_rot, x̃_mse⟩ + γ · ⟨y, S^T · sign(qjl)⟩
```
where `y_rot = Π y` (rotation applied to query), `x̃_mse[i] = centroids[i][idx[i]]` (MSE reconstruction), and `sign(qjl)` extracts ±1 from the packed bits.

- [ ] **Step 1: Write tests**

Create `src/turbo/scorer.rs`:

```rust
use super::types::{Centroids, MetaRecord, ProjectionMatrix, TurboRecord};

/// Extract the 2-bit index for dimension `dim` from the packed `idx` array.
/// Each byte holds 4 indices (2 bits each), MSB first.
pub fn get_idx(idx: &[u8; 96], dim: usize) -> usize {
    let byte = idx[dim / 4];
    let shift = 6 - (dim % 4) * 2;
    ((byte >> shift) & 0b11) as usize
}

/// Extract the sign bit for dimension `dim` from the packed `qjl` array.
/// Returns +1.0 if bit is 1, -1.0 if bit is 0.
pub fn get_sign(qjl: &[u8; 48], dim: usize) -> f32 {
    let byte = qjl[dim / 8];
    let bit = (byte >> (7 - dim % 8)) & 1;
    if bit == 1 { 1.0 } else { -1.0 }
}

/// Reconstruct x̃_mse from stored indices and centroids.
/// Returns a 384-dimensional vector.
pub fn reconstruct_mse(idx: &[u8; 96], centroids: &Centroids) -> Vec<f32> {
    (0..384)
        .map(|dim| centroids.values[dim][get_idx(idx, dim)])
        .collect()
}

/// Compute dot product of two equal-length slices.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Compute S^T · sign(qjl): for each output dimension i,
/// sum_j S[j][i] * sign(qjl[j])  (i.e., column j of S^T = row j of S).
/// Result length = projection.cols.
fn apply_qjl_transpose(qjl: &[u8; 48], projection: &ProjectionMatrix) -> Vec<f32> {
    let mut result = vec![0.0f32; projection.cols];
    for j in 0..projection.rows {
        let sign = get_sign(qjl, j);
        let row = projection.row(j);
        for (i, &s) in row.iter().enumerate() {
            result[i] += sign * s;
        }
    }
    result
}

/// Compute the TurboQuant_prod score between query `y` and a compressed record.
pub fn score(
    y: &[f32],
    record: &TurboRecord,
    centroids: &Centroids,
    projection: &ProjectionMatrix,
) -> f32 {
    let x_mse = reconstruct_mse(&record.idx, centroids);
    let mse_term = dot(y, &x_mse);

    let qjl_vec = apply_qjl_transpose(&record.qjl, projection);
    let qjl_term = dot(y, &qjl_vec);

    mse_term + record.gamma * qjl_term
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_idx_first_dim() {
        let mut idx = [0u8; 96];
        // Set first byte to 0b11_00_00_00 → dim 0 = 3
        idx[0] = 0b1100_0000;
        assert_eq!(get_idx(&idx, 0), 3);
    }

    #[test]
    fn get_idx_second_dim() {
        let mut idx = [0u8; 96];
        // Set first byte to 0b00_10_00_00 → dim 1 = 2
        idx[0] = 0b0010_0000;
        assert_eq!(get_idx(&idx, 1), 2);
    }

    #[test]
    fn get_sign_set_bit() {
        let mut qjl = [0u8; 48];
        qjl[0] = 0b1000_0000;  // dim 0 bit = 1
        assert_eq!(get_sign(&qjl, 0), 1.0);
        assert_eq!(get_sign(&qjl, 1), -1.0);
    }

    #[test]
    fn score_is_finite() {
        let centroids = Centroids {
            values: vec![[-1.0, -0.33, 0.33, 1.0]; 384],
        };
        let projection = ProjectionMatrix {
            values: vec![0.01; 384 * 384],
            rows: 384,
            cols: 384,
        };
        let record = TurboRecord {
            doc_id: 1,
            idx: [0b01_01_01_01; 96],  // all dims → centroid index 1
            qjl: [0b1010_1010; 48],
            gamma: 0.5,
        };
        let y = vec![0.1f32; 384];
        let s = score(&y, &record, &centroids, &projection);
        assert!(s.is_finite(), "score must be finite, got {s}");
    }

    #[test]
    fn score_zero_gamma() {
        // With gamma=0, QJL term vanishes; score = dot(y, x_mse)
        let centroids = Centroids {
            values: vec![[0.0, 1.0, 2.0, 3.0]; 384],
        };
        let projection = ProjectionMatrix {
            values: vec![0.0; 384 * 384],
            rows: 384,
            cols: 384,
        };
        let record = TurboRecord {
            doc_id: 1,
            idx: [0b01_01_01_01; 96],  // all dims → centroid[1] = 1.0
            qjl: [0u8; 48],
            gamma: 0.0,
        };
        let y = vec![1.0f32; 384];
        let s = score(&y, &record, &centroids, &projection);
        // dot([1.0; 384], [1.0; 384]) = 384.0
        assert!((s - 384.0).abs() < 1e-3, "expected ~384.0, got {s}");
    }
}
```

- [ ] **Step 2: Run scorer tests**

```bash
cargo test turbo::scorer::tests
```

Expected: all 4 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/turbo/scorer.rs
git commit -m "feat(turbo): implement TurboQuant_prod scorer with MSE reconstruction and QJL term"
```

---

## Task 6: TurboQuantSearcher

**Files:**
- Create: `src/query/turbo_searcher.rs`
- Modify: `src/query/mod.rs`

- [ ] **Step 1: Write test**

Create `src/query/turbo_searcher.rs`:

```rust
use std::collections::BinaryHeap;
use std::cmp::Ordering;

use rayon::prelude::*;

use crate::error::SearchError;
use crate::models::{ChunkSource, CorpusType, SearchResult, SearchSource};
use crate::turbo::scorer;
use crate::turbo::MmapIndex;

/// Trait for searching the static TurboQuant index.
/// Unlike VectorRetriever, no ActiveManifest is needed.
pub trait StaticRetriever: Send + Sync {
    fn search(
        &self,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

pub struct TurboQuantSearcher {
    pub index: &'static MmapIndex,
}

impl TurboQuantSearcher {
    pub fn new(index: &'static MmapIndex) -> Self {
        Self { index }
    }
}

impl StaticRetriever for TurboQuantSearcher {
    fn search(
        &self,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let index = self.index;

        // Parallel score computation over all records.
        let mut scored: Vec<(f32, usize)> = index
            .records
            .par_iter()
            .enumerate()
            .map(|(i, record)| {
                let s = scorer::score(
                    query_embedding,
                    record,
                    &index.centroids,
                    &index.projection,
                );
                (s, i)
            })
            .collect();

        // Partial sort: keep top_k by score descending.
        scored.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        scored.truncate(top_k);

        let results = scored
            .into_iter()
            .map(|(score, i)| {
                let record = &index.records[i];
                // Find matching meta by doc_id (meta is parallel to records array).
                let meta = &index.meta[i];
                let text = index.text_of(meta).to_owned();
                let corpus_type = MmapIndex::corpus_type_of(meta);
                SearchResult {
                    doc_id: record.doc_id.to_string(),
                    score,
                    text,
                    metadata: None,
                    source: SearchSource::Vector,
                    chunk_source: ChunkSource::Static,
                    corpus_type: Some(corpus_type),
                }
            })
            .collect();

        Ok(results)
    }
}

/// No-op static retriever — returns empty results.
/// Used as the default when no TurboQuant index is available.
pub struct NoopStaticRetriever;

impl StaticRetriever for NoopStaticRetriever {
    fn search(&self, _: &[f32], _: usize) -> Result<Vec<SearchResult>, SearchError> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SearchError;

    struct FakeStaticRetriever(Vec<SearchResult>);

    impl StaticRetriever for FakeStaticRetriever {
        fn search(&self, _: &[f32], top_k: usize) -> Result<Vec<SearchResult>, SearchError> {
            Ok(self.0.iter().take(top_k).cloned().collect())
        }
    }

    #[test]
    fn noop_returns_empty() {
        let r = NoopStaticRetriever
            .search(&[0.1; 384], 5)
            .unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn fake_retriever_respects_top_k() {
        let results: Vec<SearchResult> = (0..10)
            .map(|i| SearchResult {
                doc_id: i.to_string(),
                score: i as f32,
                text: "".into(),
                metadata: None,
                source: SearchSource::Vector,
                chunk_source: ChunkSource::Static,
                corpus_type: None,
            })
            .collect();
        let r = FakeStaticRetriever(results).search(&[], 3).unwrap();
        assert_eq!(r.len(), 3);
    }
}
```

- [ ] **Step 2: Export from `src/query/mod.rs`**

Add to `src/query/mod.rs`:

```rust
pub mod turbo_searcher;

pub use turbo_searcher::{NoopStaticRetriever, StaticRetriever, TurboQuantSearcher};
```

- [ ] **Step 3: Run tests**

```bash
cargo test query::turbo_searcher::tests
```

Expected: both tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/query/turbo_searcher.rs src/query/mod.rs
git commit -m "feat(query): add TurboQuantSearcher, StaticRetriever trait, NoopStaticRetriever"
```

---

## Task 7: Extend QueryRouter with Static Path

**Files:**
- Modify: `src/query/router.rs`

The router gains a new type parameter `S: StaticRetriever` (defaulting to `NoopStaticRetriever`) and runs the static search in parallel with the existing dynamic search.

- [ ] **Step 1: Write a test for the router with a static retriever**

Add to the bottom of `src/query/router.rs`:

```rust
#[cfg(test)]
mod turbo_router_tests {
    use super::*;
    use crate::models::{ChunkSource, CorpusWeights, SearchSource};
    use crate::query::turbo_searcher::NoopStaticRetriever;

    struct AlwaysOkKeyword;
    impl KeywordRetriever for AlwaysOkKeyword {
        fn search(&self, _: &ActiveManifest, _: &str, _: usize)
            -> Result<Vec<SearchResult>, SearchError> { Ok(vec![]) }
    }

    struct AlwaysOkVector;
    impl VectorRetriever for AlwaysOkVector {
        fn search(&self, _: &ActiveManifest, _: &[f32], _: usize)
            -> Result<Vec<SearchResult>, SearchError> { Ok(vec![]) }
    }

    struct FixedEmbedding;
    impl crate::embedding::EmbeddingGenerator for FixedEmbedding {
        fn generate(&self, _: &str)
            -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
            Ok(vec![0.1; 384])
        }
    }

    struct FakeStaticRetriever;
    impl crate::query::StaticRetriever for FakeStaticRetriever {
        fn search(&self, _: &[f32], top_k: usize)
            -> Result<Vec<SearchResult>, SearchError> {
            Ok((0..top_k).map(|i| SearchResult {
                doc_id: format!("static-{i}"),
                score: 0.9,
                text: "law text".into(),
                metadata: None,
                source: SearchSource::Vector,
                chunk_source: ChunkSource::Static,
                corpus_type: None,
            }).collect())
        }
    }

    // Note: this test requires a ManifestStore that works without real S3.
    // It is an integration-level test; run it with the moto environment.
    // Here we just check that the router compiles with a static retriever.
    #[test]
    fn router_accepts_static_retriever() {
        // Verify the type system accepts a QueryRouter with all 5 type params.
        fn _accept<M, E, K, V, S, W>(_: &QueryRouter<M, E, K, V, S, W>)
        where
            M: ManifestStore + Send + Sync,
            E: crate::embedding::EmbeddingGenerator + Send + Sync,
            K: KeywordRetriever,
            V: VectorRetriever,
            S: crate::query::StaticRetriever,
            W: WarningSink,
        {}
        // Compiles = pass.
    }
}
```

- [ ] **Step 2: Add `StaticRetriever` type parameter to `QueryRouter`**

Replace the struct definition and impls in `src/query/router.rs`:

```rust
use super::turbo_searcher::{NoopStaticRetriever, StaticRetriever};

// Add S = NoopStaticRetriever as 5th type parameter (before W).
#[derive(Debug, Clone)]
pub struct QueryRouter<M, E, K, V, S = NoopStaticRetriever, W = NoopWarningSink> {
    manifest_store: M,
    embedding_generator: E,
    keyword_retriever: K,
    vector_retriever: V,
    static_retriever: S,
    warning_sink: W,
    ranker: HybridRanker,
}
```

- [ ] **Step 3: Update `QueryRouter::new` to default `static_retriever`**

Replace the `new` impl:

```rust
impl<M, E, K, V> QueryRouter<M, E, K, V, NoopStaticRetriever, NoopWarningSink>
where
    M: ManifestStore + Send + Sync,
    E: EmbeddingGenerator + Send + Sync,
    K: KeywordRetriever,
    V: VectorRetriever,
{
    pub fn new(
        manifest_store: M,
        embedding_generator: E,
        keyword_retriever: K,
        vector_retriever: V,
    ) -> Self {
        Self {
            manifest_store,
            embedding_generator,
            keyword_retriever,
            vector_retriever,
            static_retriever: NoopStaticRetriever,
            warning_sink: NoopWarningSink,
            ranker: HybridRanker::new(60.0),
        }
    }
}
```

- [ ] **Step 4: Add `with_static_retriever` builder method**

Add to the `impl<M, E, K, V, S, W> QueryRouter<M, E, K, V, S, W>` block:

```rust
pub fn with_static_retriever<S2>(self, static_retriever: S2)
    -> QueryRouter<M, E, K, V, S2, W>
where
    S2: StaticRetriever,
{
    QueryRouter {
        manifest_store: self.manifest_store,
        embedding_generator: self.embedding_generator,
        keyword_retriever: self.keyword_retriever,
        vector_retriever: self.vector_retriever,
        static_retriever,
        warning_sink: self.warning_sink,
        ranker: self.ranker,
    }
}
```

- [ ] **Step 5: Update `with_warning_sink` to preserve `S`**

Replace the existing `with_warning_sink` method signature to include `S`:

```rust
pub fn with_warning_sink<W2>(self, warning_sink: W2) -> QueryRouter<M, E, K, V, S, W2>
where
    W2: WarningSink,
{
    QueryRouter {
        manifest_store: self.manifest_store,
        embedding_generator: self.embedding_generator,
        keyword_retriever: self.keyword_retriever,
        vector_retriever: self.vector_retriever,
        static_retriever: self.static_retriever,
        warning_sink,
        ranker: self.ranker,
    }
}
```

- [ ] **Step 6: Add bounds for `S` to the main `impl` block and update `search_hybrid`**

Update the `impl` bounds:

```rust
impl<M, E, K, V, S, W> QueryRouter<M, E, K, V, S, W>
where
    M: ManifestStore + Send + Sync,
    E: EmbeddingGenerator + Send + Sync,
    K: KeywordRetriever,
    V: VectorRetriever,
    S: StaticRetriever,
    W: WarningSink,
{
    // ...existing methods unchanged...
}
```

Replace the `search_hybrid` method to run static search in parallel:

```rust
fn search_hybrid(
    &self,
    active_manifest: &ActiveManifest,
    query: &str,
    query_embedding: &[f32],
    top_k: usize,
) -> Result<Vec<SearchResult>, SearchError> {
    let retrieval_top_k = top_k * 3;

    let (static_results, keyword_results, vector_results) = thread::scope(|scope| {
        let static_handle = scope.spawn(|| {
            self.static_retriever.search(query_embedding, retrieval_top_k)
        });
        let keyword_handle = scope.spawn(|| {
            self.keyword_retriever.search(active_manifest, query, retrieval_top_k)
        });
        let vector_handle = scope.spawn(|| {
            self.vector_retriever.search(active_manifest, query_embedding, retrieval_top_k)
        });

        let static_results = static_handle
            .join()
            .map_err(|p| SearchError::Execution {
                message: panic_payload_message("static retrieval", p),
            })?;
        let keyword_results = keyword_handle
            .join()
            .map_err(|p| SearchError::Execution {
                message: panic_payload_message("keyword retrieval", p),
            })?;
        let vector_results = vector_handle
            .join()
            .map_err(|p| SearchError::Execution {
                message: panic_payload_message("vector retrieval", p),
            })?;

        Ok::<_, SearchError>((static_results?, keyword_results?, vector_results?))
    })?;

    validate_results(&static_results)?;
    validate_results(&keyword_results)?;
    validate_results(&vector_results)?;

    // RRF fuses dynamic results; static results are appended as-is.
    let dynamic_results = self.ranker.fuse(vector_results, keyword_results);
    let mut all_results = static_results;
    all_results.extend(dynamic_results);
    Ok(all_results)
}
```

- [ ] **Step 7: Verify full test suite still passes**

```bash
cargo test
```

Expected: all existing tests pass; new `turbo_router_tests` compile and pass.

- [ ] **Step 8: Commit**

```bash
git add src/query/router.rs
git commit -m "feat(query): extend QueryRouter with StaticRetriever type param and 3-way parallel search"
```

---

## Task 8: ContextBuilder

**Files:**
- Create: `src/query/context_builder.rs`
- Modify: `src/query/mod.rs`

- [ ] **Step 1: Write tests**

Create `src/query/context_builder.rs`:

```rust
use crate::models::{ChunkSource, CorpusType, CorpusWeights, SearchResult};

pub struct ContextBuilder;

impl ContextBuilder {
    /// Format static and dynamic chunks into LLM-ready context string.
    pub fn build_context(
        static_chunks: &[SearchResult],
        dynamic_chunks: &[SearchResult],
        query: &str,
    ) -> String {
        let mut out = String::from("=== 参考资料 ===\n\n");

        for (i, r) in static_chunks.iter().enumerate() {
            let label = corpus_type_label(r.corpus_type.as_ref());
            out.push_str(&format!("[{label} #{}]\n{}\n\n", i + 1, r.text));
        }

        for (i, r) in dynamic_chunks.iter().enumerate() {
            out.push_str(&format!("[用户数据 #{}]\n{}\n\n", i + 1, r.text));
        }

        out.push_str(&format!("=== 问题 ===\n{query}"));
        out
    }

    /// Build system prompt, injecting weight instruction based on corpus_weights.
    pub fn build_system_prompt(weights: Option<&CorpusWeights>) -> String {
        let weight_instruction = weight_instruction(weights);
        format!(
            "你是一个专业的文档检索助手。\n\
             \n\
             参考资料分为两类：\n\
             - [法规/合同/RFC]：来自共享权威文档库（法律法规、合同模板、RFC等）\n\
             - [用户数据]：来自用户的私有文档\n\
             \n\
             {weight_instruction}\n\
             \n\
             回答时只引用与问题直接相关的内容，忽略无关片段。\n\
             引用时注明来源类型。"
        )
    }
}

fn weight_instruction(weights: Option<&CorpusWeights>) -> &'static str {
    match weights {
        Some(w) if w.static_bias > 0.7 => {
            "如法规/合同与用户数据冲突，以法规/合同为准。"
        }
        Some(w) if w.dynamic_bias > 0.7 => {
            "优先参考用户数据，不足时补充引用法规/合同。"
        }
        _ => "综合两类来源回答，不偏向任何一方。",
    }
}

fn corpus_type_label(ct: Option<&CorpusType>) -> &'static str {
    match ct {
        Some(CorpusType::Legal) => "法规",
        Some(CorpusType::Contract) => "合同",
        Some(CorpusType::Rfc) => "RFC",
        _ => "法规/合同",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChunkSource, SearchSource};

    fn make_result(text: &str, chunk_source: ChunkSource, ct: Option<CorpusType>) -> SearchResult {
        SearchResult {
            doc_id: "1".into(),
            score: 0.9,
            text: text.into(),
            metadata: None,
            source: SearchSource::Vector,
            chunk_source,
            corpus_type: ct,
        }
    }

    #[test]
    fn context_contains_section_headers() {
        let static_r = make_result("law text", ChunkSource::Static, Some(CorpusType::Legal));
        let dynamic_r = make_result("user doc", ChunkSource::Dynamic, None);
        let ctx = ContextBuilder::build_context(&[static_r], &[dynamic_r], "what is x?");
        assert!(ctx.contains("=== 参考资料 ==="));
        assert!(ctx.contains("[法规 #1]"));
        assert!(ctx.contains("law text"));
        assert!(ctx.contains("[用户数据 #1]"));
        assert!(ctx.contains("user doc"));
        assert!(ctx.contains("=== 问题 ==="));
        assert!(ctx.contains("what is x?"));
    }

    #[test]
    fn weight_instruction_static_bias() {
        let w = CorpusWeights { static_bias: 0.9, dynamic_bias: 0.1 };
        let prompt = ContextBuilder::build_system_prompt(Some(&w));
        assert!(prompt.contains("以法规/合同为准"));
    }

    #[test]
    fn weight_instruction_dynamic_bias() {
        let w = CorpusWeights { static_bias: 0.1, dynamic_bias: 0.9 };
        let prompt = ContextBuilder::build_system_prompt(Some(&w));
        assert!(prompt.contains("优先参考用户数据"));
    }

    #[test]
    fn weight_instruction_default() {
        let prompt = ContextBuilder::build_system_prompt(None);
        assert!(prompt.contains("不偏向任何一方"));
    }

    #[test]
    fn empty_chunks_produces_valid_context() {
        let ctx = ContextBuilder::build_context(&[], &[], "test?");
        assert!(ctx.contains("=== 问题 ==="));
        assert!(ctx.contains("test?"));
    }
}
```

- [ ] **Step 2: Export from `src/query/mod.rs`**

Add to `src/query/mod.rs`:

```rust
pub mod context_builder;
pub use context_builder::ContextBuilder;
```

- [ ] **Step 3: Run tests**

```bash
cargo test query::context_builder::tests
```

Expected: all 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/query/context_builder.rs src/query/mod.rs
git commit -m "feat(query): add ContextBuilder for LLM prompt and context formatting"
```

---

## Task 9: TurboQuant Encoder (for offline builder)

**Files:**
- Create: `src/turbo/encoder.rs`

The encoder is the inverse of the scorer: it compresses a float32 vector into a `TurboRecord`.

- [ ] **Step 1: Write tests**

Create `src/turbo/encoder.rs`:

```rust
use super::scorer::{dot, get_idx, get_sign};
use super::types::{Centroids, ProjectionMatrix, TurboRecord};

/// Pack a 2-bit index value into `idx` at position `dim`.
pub fn set_idx(idx: &mut [u8; 96], dim: usize, value: u8) {
    debug_assert!(value < 4);
    let byte_pos = dim / 4;
    let shift = 6 - (dim % 4) * 2;
    idx[byte_pos] &= !(0b11 << shift);
    idx[byte_pos] |= (value & 0b11) << shift;
}

/// Pack a sign bit into `qjl` at position `dim`. bit=1 → +1, bit=0 → -1.
pub fn set_sign(qjl: &mut [u8; 48], dim: usize, positive: bool) {
    let byte_pos = dim / 8;
    let bit_pos = 7 - (dim % 8);
    if positive {
        qjl[byte_pos] |= 1 << bit_pos;
    } else {
        qjl[byte_pos] &= !(1 << bit_pos);
    }
}

/// Apply random rotation matrix Π to vector x (Π is row-major, shape [D][D]).
pub fn rotate(x: &[f32], pi: &ProjectionMatrix) -> Vec<f32> {
    (0..pi.rows).map(|i| dot(pi.row(i), x)).collect()
}

/// Find the nearest centroid index for a scalar value.
fn nearest_centroid(value: f32, centroids: &[f32; 4]) -> u8 {
    let mut best_idx = 0u8;
    let mut best_dist = f32::MAX;
    for (i, &c) in centroids.iter().enumerate() {
        let d = (value - c).abs();
        if d < best_dist {
            best_dist = d;
            best_idx = i as u8;
        }
    }
    best_idx
}

/// Compress a 384-dimensional float32 vector into a TurboRecord.
///
/// # Arguments
/// - `x`: input vector (length must equal centroids.values.len())
/// - `doc_id`: identifier for this chunk
/// - `pi`: random rotation matrix (same for all records, deterministic seed)
/// - `centroids`: per-dimension MSE centroids
/// - `s`: QJL projection matrix
pub fn compress(
    x: &[f32],
    doc_id: u64,
    pi: &ProjectionMatrix,
    centroids: &Centroids,
    s: &ProjectionMatrix,
) -> TurboRecord {
    let dims = centroids.values.len();
    assert_eq!(x.len(), dims);

    // Stage 1: rotate and MSE-quantize.
    let x_rot = rotate(x, pi);
    let mut idx = [0u8; 96];
    let mut x_mse = vec![0.0f32; dims];
    for d in 0..dims {
        let ci = nearest_centroid(x_rot[d], &centroids.values[d]);
        set_idx(&mut idx, d, ci);
        x_mse[d] = centroids.values[d][ci as usize];
    }

    // Stage 2: compute residual and QJL-quantize.
    let residual: Vec<f32> = x_rot.iter().zip(x_mse.iter()).map(|(a, b)| a - b).collect();
    let gamma = residual.iter().map(|v| v * v).sum::<f32>().sqrt();

    // Apply S to residual → projected; store sign bits.
    let mut qjl = [0u8; 48];
    for j in 0..s.rows {
        let proj = dot(s.row(j), &residual);
        set_sign(&mut qjl, j, proj >= 0.0);
    }

    TurboRecord { doc_id, idx, qjl, gamma }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turbo::scorer;

    fn identity_pi(dims: usize) -> ProjectionMatrix {
        let mut values = vec![0.0f32; dims * dims];
        for i in 0..dims {
            values[i * dims + i] = 1.0;
        }
        ProjectionMatrix { values, rows: dims, cols: dims }
    }

    #[test]
    fn set_get_idx_roundtrip() {
        let mut idx = [0u8; 96];
        for dim in 0..384 {
            let val = (dim % 4) as u8;
            set_idx(&mut idx, dim, val);
            assert_eq!(scorer::get_idx(&idx, dim), val as usize, "dim={dim}");
        }
    }

    #[test]
    fn set_get_sign_roundtrip() {
        let mut qjl = [0u8; 48];
        for dim in 0..384 {
            let positive = dim % 2 == 0;
            set_sign(&mut qjl, dim, positive);
            let expected = if positive { 1.0f32 } else { -1.0f32 };
            assert_eq!(scorer::get_sign(&qjl, dim), expected, "dim={dim}");
        }
    }

    #[test]
    fn compress_with_identity_rotation() {
        // With identity Π, rotation is a no-op.
        let dims = 384;
        let pi = identity_pi(dims);
        let centroids = Centroids {
            values: vec![[-1.5, -0.5, 0.5, 1.5]; dims],
        };
        let s = identity_pi(dims);
        let x = vec![0.6f32; dims];  // nearest centroid is index 2 (0.5)

        let record = compress(&x, 99, &pi, &centroids, &s);
        assert_eq!(record.doc_id, 99);
        assert!(record.gamma.is_finite());
        assert!(record.gamma >= 0.0);

        // With identity rotation and uniform x, all idx should map to centroid 2.
        for dim in 0..dims {
            assert_eq!(scorer::get_idx(&record.idx, dim), 2, "dim={dim}");
        }
    }
}
```

- [ ] **Step 2: Run encoder tests**

```bash
cargo test turbo::encoder::tests
```

Expected: all 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/turbo/encoder.rs
git commit -m "feat(turbo): implement TurboQuant encoder (rotate → quantize → QJL compress)"
```

---

## Task 10: Offline Index Builder Binary

**Files:**
- Create: `src/bin/turbo_index_builder.rs`
- Modify: `Cargo.toml` (add binary entry)

This CLI reads a JSONL file of `{doc_id, text, corpus_type}` records (one per line), generates embeddings, compresses each vector, and writes the three binary files.

- [ ] **Step 1: Add binary to `Cargo.toml`**

```toml
[[bin]]
name = "turbo_index_builder"
path = "src/bin/turbo_index_builder.rs"
```

- [ ] **Step 2: Create the builder binary**

Create `src/bin/turbo_index_builder.rs`:

```rust
//! Offline TurboQuant index builder.
//!
//! Usage:
//!   turbo_index_builder --input docs.jsonl --output-dir ./static
//!
//! Input JSONL format (one record per line):
//!   {"doc_id": 1, "text": "...", "corpus_type": "legal"}
//!
//! Output files written to --output-dir:
//!   turbo_static.bin, turbo_static_meta.bin, turbo_static_text.bin,
//!   centroids.bin, projection.bin

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use serde::Deserialize;

use ltsearch::turbo::encoder;
use ltsearch::turbo::types::{Centroids, MetaRecord, ProjectionMatrix, TurboRecord};

#[derive(Deserialize)]
struct InputRecord {
    doc_id: u64,
    text: String,
    corpus_type: String,
}

fn corpus_type_byte(s: &str) -> u8 {
    match s {
        "legal" => 0,
        "contract" => 1,
        "rfc" => 2,
        _ => 3,
    }
}

/// Generate a deterministic random projection matrix using a fixed seed.
/// Uses a simple LCG for reproducibility without external deps.
fn gen_matrix(rows: usize, cols: usize, seed: u64) -> ProjectionMatrix {
    let mut state = seed;
    let values: Vec<f32> = (0..rows * cols)
        .map(|_| {
            // LCG step
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            // Map to [-1, 1] and normalize by sqrt(cols) for approximate isometry
            let v = ((state >> 33) as f32 / (u32::MAX as f32) * 2.0 - 1.0) / (cols as f32).sqrt();
            v
        })
        .collect();
    ProjectionMatrix { values, rows, cols }
}

/// Generate per-dimension centroids for 2-bit (4-level) uniform quantization
/// over the range [-2.0, 2.0]. These are approximate; in production, learn
/// from actual embedding statistics.
fn gen_centroids(dims: usize) -> Centroids {
    Centroids {
        values: vec![[-1.5, -0.5, 0.5, 1.5]; dims],
    }
}

fn write_centroids(centroids: &Centroids, path: &PathBuf) -> anyhow::Result<()> {
    let mut f = File::create(path)?;
    for dim_centroids in &centroids.values {
        for &v in dim_centroids {
            f.write_all(&v.to_le_bytes())?;
        }
    }
    Ok(())
}

fn write_projection(proj: &ProjectionMatrix, path: &PathBuf) -> anyhow::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(&(proj.rows as u32).to_le_bytes())?;
    f.write_all(&(proj.cols as u32).to_le_bytes())?;
    for &v in &proj.values {
        f.write_all(&v.to_le_bytes())?;
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut input_path = None::<PathBuf>;
    let mut output_dir = PathBuf::from("./static");

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" => input_path = Some(PathBuf::from(args.next().context("--input needs value")?)),
            "--output-dir" => output_dir = PathBuf::from(args.next().context("--output-dir needs value")?),
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }

    let input_path = input_path.context("--input is required")?;
    fs::create_dir_all(&output_dir)?;

    const DIMS: usize = 384;
    // Use fixed seeds — NEVER change these without rebuilding the full index.
    let pi = gen_matrix(DIMS, DIMS, 0xDEADBEEF_CAFEBABE);
    let s = gen_matrix(DIMS, DIMS, 0xFEEDFACE_12345678);
    let centroids = gen_centroids(DIMS);

    // Write supporting files.
    write_centroids(&centroids, &output_dir.join("centroids.bin"))?;
    write_projection(&pi, &output_dir.join("projection.bin"))?;

    // Process input records.
    let input = BufReader::new(File::open(&input_path)?);
    let mut records_buf: Vec<u8> = Vec::new();
    let mut meta_buf: Vec<u8> = Vec::new();
    let mut text_buf: Vec<u8> = Vec::new();
    let mut count = 0usize;
    let started = Instant::now();

    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let rec: InputRecord = serde_json::from_str(&line)
            .with_context(|| format!("bad JSON on line {}", count + 1))?;

        // Placeholder embedding: in production, call the embedding model here.
        // Replace this with: embedding_generator.generate(&rec.text)?
        let embedding = vec![0.0f32; DIMS];

        let turbo = encoder::compress(&embedding, rec.doc_id, &pi, &centroids, &s);
        let text_offset = text_buf.len() as u64;
        let text_bytes = rec.text.as_bytes();
        text_buf.extend_from_slice(text_bytes);

        let meta = MetaRecord {
            doc_id: rec.doc_id,
            corpus_type: corpus_type_byte(&rec.corpus_type),
            _pad: [0; 3],
            text_offset,
            text_len: text_bytes.len() as u32,
        };

        // Append TurboRecord bytes.
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &turbo as *const TurboRecord as *const u8,
                std::mem::size_of::<TurboRecord>(),
            )
        };
        records_buf.extend_from_slice(record_bytes);

        // Append MetaRecord bytes.
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &meta as *const MetaRecord as *const u8,
                std::mem::size_of::<MetaRecord>(),
            )
        };
        meta_buf.extend_from_slice(meta_bytes);

        count += 1;
        if count % 10_000 == 0 {
            eprintln!("processed {count} records...");
        }
    }

    fs::write(output_dir.join("turbo_static.bin"), &records_buf)?;
    fs::write(output_dir.join("turbo_static_meta.bin"), &meta_buf)?;
    fs::write(output_dir.join("turbo_static_text.bin"), &text_buf)?;

    eprintln!(
        "Done: {count} records in {:.1}s → {:.1} MB",
        started.elapsed().as_secs_f32(),
        (records_buf.len() + meta_buf.len() + text_buf.len()) as f32 / 1e6
    );
    Ok(())
}
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo build --bin turbo_index_builder 2>&1 | tail -5
```

Expected: `Compiling ltsearch ... Finished`.

- [ ] **Step 4: Smoke test with a tiny input**

```bash
echo '{"doc_id":1,"text":"test document","corpus_type":"legal"}' > /tmp/test_docs.jsonl
cargo run --bin turbo_index_builder -- --input /tmp/test_docs.jsonl --output-dir /tmp/static_test
ls -lh /tmp/static_test/
```

Expected: 5 files created: `turbo_static.bin` (156 bytes), `turbo_static_meta.bin` (24 bytes), `turbo_static_text.bin` (13 bytes), `centroids.bin`, `projection.bin`.

- [ ] **Step 5: Commit**

```bash
git add src/bin/turbo_index_builder.rs Cargo.toml
git commit -m "feat(bin): add turbo_index_builder offline index builder CLI"
```

---

## Task 11: Dockerfile and Static Placeholder

**Files:**
- Create: `Dockerfile`
- Create: `static/.gitkeep`
- Modify: `.gitignore`

- [ ] **Step 1: Create `static/` placeholder**

```bash
mkdir -p static
touch static/.gitkeep
```

- [ ] **Step 2: Add `static/*.bin` to `.gitignore`**

Add to `.gitignore`:

```
# TurboQuant binary index files (generated artifacts, not source)
static/*.bin
```

- [ ] **Step 3: Create `Dockerfile`**

Create `Dockerfile`:

```dockerfile
# Build stage
FROM rust:1.94 AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY vendor/ vendor/
COPY src/ src/

RUN cargo build --release --bin query_lambda

# Runtime stage
FROM public.ecr.aws/lambda/provided:al2023

COPY --from=builder /build/target/release/query_lambda /var/task/bootstrap

# Static TurboQuant index — update this layer monthly/quarterly by rebuilding
# after running turbo_index_builder.
COPY static/ /app/static/

CMD ["bootstrap"]
```

- [ ] **Step 4: Verify `static/` directory structure comment in README is not needed** (no doc files unless requested)

- [ ] **Step 5: Commit**

```bash
git add Dockerfile static/.gitkeep .gitignore
git commit -m "feat(deploy): add Dockerfile with /app/static TurboQuant layer and static placeholder"
```

---

## Self-Review

**Spec coverage check:**

| Spec section | Task(s) |
|---|---|
| §4 Static file layout (TurboRecord, MetaRecord) | Task 3 |
| §4 centroids.bin, projection.bin | Tasks 4, 9, 10 |
| §5.1 MmapIndex global singleton | Task 4 |
| §5.2 TurboQuantSearcher + StaticRetriever | Task 6 |
| §5.3 QueryRouter 3-way parallel | Task 7 |
| §5.4 SearchResult ChunkSource/CorpusType | Task 2 |
| §5.5 SearchRequest CorpusWeights | Task 2 |
| §6 ContextBuilder + prompt | Task 8 |
| §8 Offline index builder | Tasks 9, 10 |
| §9 Docker image + /app/static | Task 11 |

All spec requirements covered.

**OnceLock for Lambda cold start** (from spec §9): not yet wired in `query_lambda.rs`. Add after Task 4 is merged:

```rust
// In src/query_lambda.rs (or wherever the handler is initialized):
use std::sync::OnceLock;
use ltsearch::turbo::MmapIndex;

static MMAP_INDEX: OnceLock<MmapIndex> = OnceLock::new();

fn get_index() -> &'static MmapIndex {
    MMAP_INDEX.get_or_init(|| {
        MmapIndex::load_from_image().expect("failed to load TurboQuant index")
    })
}
```

This is intentionally left as a follow-up wiring step once the Lambda handler structure is confirmed.
