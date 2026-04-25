# TurboQuant Searcher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close issue `#73` by tightening the synchronous typed-record `TurboQuantSearcher` path with score-breakdown correctness coverage, deterministic recall evidence, and a typed 512d benchmark harness.

**Architecture:** Keep `TurboQuantSearcher` synchronous and typed-record-first. Add a small typed scoring breakdown helper in `src/index/turbo_codec.rs` so tests can validate centroid and residual terms independently, then strengthen searcher tests and add deterministic recall and benchmark coverage around the existing `MmapIndex` + `TurboRecord512` path.

**Tech Stack:** Rust, `cargo test`, `rayon`, `memmap2`, integration tests under `tests/`

---

## File Map

- `src/index/turbo_codec.rs`
  Owns TurboQuant encoding and typed scoring. This is the right place for a small score-breakdown helper that keeps the production scorer readable while exposing testable formula pieces.
- `src/index/mod.rs`
  Re-exports any new public typed scoring helper added in `turbo_codec.rs`.
- `src/query/turbo_searcher.rs`
  Owns the synchronous typed-record scan. Keep the public API unchanged; only extract small internal helpers if they make the hot path clearer.
- `tests/turbo_codec_test.rs`
  Verifies packed encoding, typed scoring, and the centroid/QJL/gamma terms independently.
- `tests/turbo_searcher_test.rs`
  Verifies `TurboQuantSearcher` result ordering, validation errors, top-K limits, and stable static result materialization.
- `tests/turbo_searcher_recall_test.rs`
  Adds deterministic recall coverage against exact float32 dot-product ranking.
- `tests/turbo_searcher_benchmark_test.rs`
  Replaces the stale 4d smoke setup with an ignored typed 512d benchmark harness.

### Task 1: Add Typed Score Breakdown Coverage

**Files:**
- Modify: `src/index/turbo_codec.rs`
- Modify: `src/index/mod.rs`
- Test: `tests/turbo_codec_test.rs`

- [ ] **Step 1: Write the failing codec-breakdown tests**

Add these tests to `tests/turbo_codec_test.rs` alongside the existing typed scorer coverage:

```rust
use ltsearch::index::{
    encode_vector, score_query_against_record_512, score_query_terms_against_record_512,
    CentroidTable, ProjectionMatrix, TurboRecord512,
};

#[test]
fn typed_score_breakdown_separates_centroid_qjl_and_gamma_terms() {
    let dim = 512;
    let mut centroid_values = Vec::with_capacity(dim as usize * 4);
    for _ in 0..dim {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(dim, 4, &centroid_values);
    let projection = identity_projection(dim as usize);

    let mut query = vec![0.0; dim as usize];
    query[0] = 2.0;
    query[1] = -1.0;
    query[2] = 0.5;
    let encoded_query = encode_vector(&query, &centroids, &projection).unwrap();

    let mut idx = [0u8; 128];
    idx[0] = 0b00_01_10;
    let mut qjl = [0u8; 64];
    qjl[0] = 0b0000_0101;
    let record = TurboRecord512 {
        doc_id: 7,
        idx,
        qjl,
        gamma: 2.0,
        _reserved: [0; 4],
    };

    let terms = score_query_terms_against_record_512(
        &query,
        &encoded_query,
        &record,
        &centroids,
        &projection,
    )
    .unwrap();

    assert!((terms.centroid_score - 3.0).abs() < 1e-6);
    assert!((terms.qjl_score - 3.5).abs() < 1e-6);
    assert!((terms.total() - 10.0).abs() < 1e-6);
}

#[test]
fn typed_score_matches_legacy_byte_score_for_the_same_record() {
    let dim = 512;
    let mut centroid_values = Vec::with_capacity(dim as usize * 4);
    for _ in 0..dim {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(dim, 4, &centroid_values);
    let projection = identity_projection(dim as usize);
    let header = TurboHeader::new(dim, 1);
    let query = vec![0.25; dim as usize];
    let encoded_query = encode_vector(&query, &centroids, &projection).unwrap();

    let record = TurboRecord512 {
        doc_id: 11,
        idx: [0b01; 128],
        qjl: [0b1010_1010; 64],
        gamma: 0.75,
        _reserved: [0; 4],
    };
    let raw_record = record_bytes(&header, &record.idx, &record.qjl, record.gamma);

    let typed = score_query_against_record_512(
        &query,
        &encoded_query,
        &record,
        &centroids,
        &projection,
    )
    .unwrap();
    let legacy = score_query_against_record(
        &query,
        &encoded_query,
        &raw_record,
        &header,
        &centroids,
        &projection,
    )
    .unwrap();

    assert!((typed - legacy).abs() < 1e-6);
}
```

- [ ] **Step 2: Run the codec tests to verify the new API is missing**

Run: `cargo test turbo_codec -- --nocapture`

Expected: FAIL with a compile error because `score_query_terms_against_record_512` does not exist yet.

- [ ] **Step 3: Add the minimal typed score-breakdown implementation**

In `src/index/turbo_codec.rs`, add a tiny typed result struct and delegate the existing typed scorer through it:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurboScoreBreakdown {
    pub centroid_score: f32,
    pub qjl_score: f32,
    pub gamma: f32,
}

impl TurboScoreBreakdown {
    pub fn total(self) -> f32 {
        self.centroid_score + self.gamma * self.qjl_score
    }
}

pub fn score_query_terms_against_record_512(
    query: &[f32],
    encoded: &EncodedTurboVector,
    record: &TurboRecord512,
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) -> Result<TurboScoreBreakdown, AssetError> {
    validate_codec_inputs(query.len(), centroids, projection)?;
    validate_encoded_vector(encoded, query.len())?;

    let centroid_score = (0..query.len())
        .map(|dim| query[dim] * centroid_value(centroids, dim, read_idx(&record.idx, dim) as usize))
        .sum::<f32>();

    let projected_query = projection.project_checked(query)?;
    let qjl_score = projected_query
        .iter()
        .enumerate()
        .map(|(dim, value)| value * if read_sign_bit(&record.qjl, dim) { 1.0 } else { -1.0 })
        .sum::<f32>();

    Ok(TurboScoreBreakdown {
        centroid_score,
        qjl_score,
        gamma: record.gamma,
    })
}

pub fn score_query_against_record_512(
    query: &[f32],
    encoded: &EncodedTurboVector,
    record: &TurboRecord512,
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) -> Result<f32, AssetError> {
    Ok(score_query_terms_against_record_512(query, encoded, record, centroids, projection)?.total())
}
```

Update `src/index/mod.rs` to re-export the new helper:

```rust
pub use turbo_codec::{
    encode_vector, score_query_against_record, score_query_against_record_512,
    score_query_terms_against_record_512, EncodedTurboVector, TurboScoreBreakdown,
};
```

- [ ] **Step 4: Run codec tests until they pass**

Run: `cargo test turbo_codec -- --nocapture`

Expected: PASS. The new tests should prove the centroid term, signed-projection term, `gamma` multiplier, and legacy/typed score parity.

- [ ] **Step 5: Commit the codec work**

```bash
git add src/index/turbo_codec.rs src/index/mod.rs tests/turbo_codec_test.rs
git commit -m "test(turbo): verify typed scoring terms"
```

### Task 2: Tighten TurboQuantSearcher Behavior Coverage

**Files:**
- Modify: `src/query/turbo_searcher.rs`
- Test: `tests/turbo_searcher_test.rs`

- [ ] **Step 1: Add searcher behavior tests for score stability and top-k validation**

Extend `tests/turbo_searcher_test.rs` with these tests:

```rust
#[test]
fn turbo_searcher_rejects_top_k_out_of_range() {
    let dir = temp_dir("top-k-range");
    write_test_index(
        &dir,
        512,
        &[FixtureDoc {
            doc_id: 1,
            corpus_type: 0,
            text: "legal one",
            embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        }],
    );

    let searcher = load_searcher(&dir);
    let error = searcher.search(&padded_embedding(&[1.2, -1.4, 0.3, 0.9]), 0).unwrap_err();

    assert!(matches!(
        error,
        SearchError::Validation(ValidationError::RangeOutOfRange {
            field: "top_k",
            min: 1,
            max: 100,
        })
    ));
}

#[test]
fn turbo_searcher_returns_the_exact_score_for_a_single_document_fixture() {
    let dir = temp_dir("single-doc-score");
    let query = padded_embedding(&[1.2, -1.4, 0.3, 0.9]);
    write_test_index(
        &dir,
        512,
        &[FixtureDoc {
            doc_id: 42,
            corpus_type: 1,
            text: "contract forty-two",
            embedding: query.clone(),
        }],
    );

    let searcher = load_searcher(&dir);
    let results = searcher.search(&query, 1).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].doc_id, "42");
    assert_eq!(results[0].source, SearchSource::Static);
    assert!(results[0].score.is_finite());
    assert!(results[0].score > 0.0);
}
```

- [ ] **Step 2: Run the searcher tests**

Run: `cargo test turbo_searcher -- --nocapture`

Expected: Either FAIL on one of the new assertions or PASS immediately because the current searcher already satisfies the stronger contract. Keep the tests either way.

- [ ] **Step 3: If needed, make the typed hot path explicit with a small helper**

If `src/query/turbo_searcher.rs` still feels too dense after adding the new tests, extract the typed scan into a focused helper without changing the public API:

```rust
use crate::index::{
    encode_vector, score_query_against_record_512, EncodedTurboVector, MmapIndex, TurboRecord512,
    TurboRecordSlice,
};

fn search_records_512(
    &self,
    records: &[TurboRecord512],
    query_embedding: &[f32],
    encoded_query: &EncodedTurboVector,
    top_k: usize,
) -> Result<BinaryHeap<RankedResult>, SearchError> {
    records
        .par_iter()
        .enumerate()
        .try_fold(BinaryHeap::new, |mut heap, (record_index, record)| {
            let score = score_query_against_record_512(
                query_embedding,
                encoded_query,
                record,
                self.index.centroids(),
                self.index.projection(),
            )
            .map_err(|source| SearchError::Execution {
                message: format!("failed to score turbo record {record_index}: {source}"),
            })?;

            let meta = self.index.meta(record_index as u64);
            push_bounded(
                &mut heap,
                RankedResult {
                    score,
                    doc_id: meta.doc_id,
                    text: self.index.text(record_index as u64).to_string(),
                    corpus_type: CorpusType::from_id(meta.corpus_type),
                },
                top_k,
            );
            Ok::<_, SearchError>(heap)
        })
        .try_reduce(BinaryHeap::new, |mut left, right| {
            for candidate in right.into_sorted_vec() {
                push_bounded(&mut left, candidate, top_k);
            }
            Ok::<_, SearchError>(left)
        })
}
```

Only keep this refactor if it makes the file clearer. Do not change `TurboQuantSearcher::search(...)` or add new public API.

- [ ] **Step 4: Re-run the searcher test suite**

Run: `cargo test turbo_searcher -- --nocapture`

Expected: PASS. Confirm stable ordering, corpus mapping, top-K truncation, dimension validation, and `top_k` validation all still hold.

- [ ] **Step 5: Commit the searcher behavior work**

```bash
git add src/query/turbo_searcher.rs tests/turbo_searcher_test.rs
git commit -m "test(turbo): strengthen searcher result coverage"
```

### Task 3: Add Deterministic Recall Regression Coverage

**Files:**
- Create: `tests/turbo_searcher_recall_test.rs`
- Test: `tests/turbo_searcher_recall_test.rs`

- [ ] **Step 1: Write the failing recall regression test**

Create `tests/turbo_searcher_recall_test.rs` with a deterministic corpus/query generator and a single recall assertion:

```rust
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader,
    TurboRecord512, META_RECORD_SIZE,
};
use ltsearch::query::TurboQuantSearcher;

const DIM: usize = 512;
const DOC_COUNT: usize = 256;
const QUERY_COUNT: usize = 8;
const TOP_K: usize = 10;
const RECALL_TARGET: f32 = 0.90;

#[test]
fn turbo_searcher_recall_stays_above_ninety_percent_against_exact_dot_product() {
    let dir = temp_dir("recall");
    let docs = generate_embeddings(0xC0FFEE, DOC_COUNT);
    write_test_index(&dir, &docs);

    let index = Box::new(MmapIndex::load(&dir).unwrap());
    let searcher = TurboQuantSearcher::new(Box::leak(index));

    let queries = generate_embeddings(0xFACEFEED, QUERY_COUNT);
    let mut total_hits = 0usize;
    let mut total_expected = 0usize;

    for query in &queries {
        let turbo = searcher.search(query, TOP_K).unwrap();
        let exact = exact_top_k(&docs, query, TOP_K);

        let turbo_ids: HashSet<_> = turbo.iter().map(|result| result.doc_id.as_str()).collect();
        total_hits += exact
            .iter()
            .filter(|doc_id| turbo_ids.contains(doc_id.as_str()))
            .count();
        total_expected += exact.len();
    }

    let recall = total_hits as f32 / total_expected as f32;
    assert!(
        recall > RECALL_TARGET,
        "expected recall > {RECALL_TARGET}, got {recall}"
    );
}
```

- [ ] **Step 2: Run the new recall test**

Run: `cargo test turbo_searcher_recall -- --nocapture`

Expected: FAIL to compile because the deterministic helpers (`temp_dir`, `generate_embeddings`, `write_test_index`, `exact_top_k`) do not exist yet.

- [ ] **Step 3: Fill in the deterministic recall helpers in the same file**

Add the smallest local helpers needed by the test:

```rust
fn generate_embeddings(seed: u64, count: usize) -> Vec<Vec<f32>> {
    let mut state = seed;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let mut embedding = vec![0.0; DIM];
        for value in &mut embedding {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let bits = ((state >> 32) as u32) as f32 / u32::MAX as f32;
            *value = bits * 2.0 - 1.0;
        }
        out.push(embedding);
    }
    out
}

fn exact_top_k(docs: &[Vec<f32>], query: &[f32], top_k: usize) -> Vec<String> {
    let mut scored = docs
        .iter()
        .enumerate()
        .map(|(index, doc)| {
            let score = doc.iter().zip(query).map(|(left, right)| left * right).sum::<f32>();
            ((index + 1).to_string(), score)
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.truncate(top_k);
    scored.into_iter().map(|(doc_id, _)| doc_id).collect()
}

fn write_test_index(dir: &Path, docs: &[Vec<f32>]) {
    let mut centroid_values = Vec::with_capacity(DIM * 4);
    for _ in 0..DIM {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(DIM as u32, 4, &centroid_values);
    let projection = identity_projection(DIM);
    let header = TurboHeader::new(DIM as u32, docs.len() as u64);

    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();

    for (index, embedding) in docs.iter().enumerate() {
        let encoded = encode_vector(embedding, &centroids, &projection).unwrap();
        let record = TurboRecord512 {
            doc_id: (index + 1) as u64,
            idx: encoded.idx.clone().try_into().unwrap(),
            qjl: encoded.qjl.clone().try_into().unwrap(),
            gamma: encoded.gamma,
            _reserved: [0; 4],
        };
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &record as *const TurboRecord512 as *const u8,
                std::mem::size_of::<TurboRecord512>(),
            )
        };
        bin_data.extend_from_slice(record_bytes);

        let text = format!("document {index}");
        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(text.as_bytes());
        let meta = MetaRecord {
            doc_id: (index + 1) as u64,
            corpus_type: (index % 3) as u8,
            _pad: [0; 3],
            text_offset,
            text_len: text.len() as u32,
        };
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&meta as *const MetaRecord as *const u8, META_RECORD_SIZE)
        };
        meta_data.extend_from_slice(meta_bytes);
    }

    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();
    fs::write(dir.join("turbo_static_meta.bin"), &meta_data).unwrap();
    fs::write(dir.join("turbo_static_text.bin"), &text_blob).unwrap();
    fs::write(dir.join("centroids.bin"), centroids.to_bytes()).unwrap();
    fs::write(dir.join("projection.bin"), projection.to_bytes()).unwrap();
}
```

- [ ] **Step 4: Run the recall test until it passes**

Run: `cargo test turbo_searcher_recall -- --nocapture`

Expected: PASS with printed or debuggable recall above `0.90` on the fixed dataset.

- [ ] **Step 5: Commit the recall regression**

```bash
git add tests/turbo_searcher_recall_test.rs
git commit -m "test(turbo): add recall regression coverage"
```

### Task 4: Replace the Legacy Benchmark Smoke With a Typed 512d Harness

**Files:**
- Modify: `tests/turbo_searcher_benchmark_test.rs`
- Test: `tests/turbo_searcher_benchmark_test.rs`

- [ ] **Step 1: Rewrite the benchmark fixture to use the typed 512d layout**

Update `tests/turbo_searcher_benchmark_test.rs` so it no longer builds a 4d byte-layout record. Replace the fixture builder with the same 512d typed pattern used elsewhere:

```rust
use std::time::Instant;

use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader,
    TurboRecord512, META_RECORD_SIZE,
};

const DIM: usize = 512;

fn padded_embedding(doc_id: u64) -> Vec<f32> {
    let mut embedding = vec![0.0; DIM];
    embedding[0] = 1.0;
    embedding[1] = 0.5;
    embedding[2] = (doc_id % 7) as f32 * 0.1;
    embedding[3] = -0.25;
    embedding
}

fn write_benchmark_index(dir: &Path, doc_count: u64) {
    let mut centroid_values = Vec::with_capacity(DIM * 4);
    for _ in 0..DIM {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(DIM as u32, 4, &centroid_values);
    let projection = identity_projection(DIM);
    let header = TurboHeader::new(DIM as u32, doc_count);
    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();

    for doc_id in 0..doc_count {
        let embedding = padded_embedding(doc_id);
        let encoded = encode_vector(&embedding, &centroids, &projection).unwrap();
        let record = TurboRecord512 {
            doc_id: doc_id + 1,
            idx: encoded.idx.clone().try_into().unwrap(),
            qjl: encoded.qjl.clone().try_into().unwrap(),
            gamma: encoded.gamma,
            _reserved: [0; 4],
        };
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &record as *const TurboRecord512 as *const u8,
                std::mem::size_of::<TurboRecord512>(),
            )
        };
        bin_data.extend_from_slice(record_bytes);

        let text = format!("document {doc_id}");
        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(text.as_bytes());
        let meta = MetaRecord {
            doc_id: doc_id + 1,
            corpus_type: (doc_id % 3) as u8,
            _pad: [0; 3],
            text_offset,
            text_len: text.len() as u32,
        };
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&meta as *const MetaRecord as *const u8, META_RECORD_SIZE)
        };
        meta_data.extend_from_slice(meta_bytes);
    }

    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();
    fs::write(dir.join("turbo_static_meta.bin"), &meta_data).unwrap();
    fs::write(dir.join("turbo_static_text.bin"), &text_blob).unwrap();
    fs::write(dir.join("centroids.bin"), centroids.to_bytes()).unwrap();
    fs::write(dir.join("projection.bin"), projection.to_bytes()).unwrap();
}
```

- [ ] **Step 2: Time the search call inside the ignored benchmark test**

Replace the old test body with:

```rust
#[test]
#[ignore = "benchmark-style smoke test"]
fn turbo_searcher_benchmark_smoke_compiles() {
    let dir = temp_dir("smoke");
    write_benchmark_index(&dir, 100_000);

    let index = Box::new(MmapIndex::load(&dir).unwrap());
    let searcher = TurboQuantSearcher::new(Box::leak(index));
    let query = padded_embedding(0);

    let started_at = Instant::now();
    let results = searcher.search(&query, 10).unwrap();
    let elapsed = started_at.elapsed();

    assert_eq!(results.len(), 10);
    eprintln!("typed turbo benchmark: docs=100000 elapsed_ms={}", elapsed.as_millis());
}
```

- [ ] **Step 3: Run the ignored benchmark harness**

Run: `cargo test turbo_searcher_benchmark_smoke_compiles -- --ignored --nocapture`

Expected: PASS. The test should load the typed 512d fixture and print an elapsed time instead of failing on the obsolete 4d layout.

- [ ] **Step 4: Run the full turbo-focused regression slice**

Run: `cargo test turbo_ -- --nocapture`

Expected: PASS for the non-ignored turbo tests. Then run the benchmark again with `--ignored` to confirm the typed harness still works.

- [ ] **Step 5: Commit the benchmark update**

```bash
git add tests/turbo_searcher_benchmark_test.rs
git commit -m "test(turbo): refresh typed benchmark harness"
```

## Self-Review Checklist

- Spec coverage: this plan covers typed formula verification, sync searcher scope, deterministic recall, and benchmark harness updates. It does not introduce async APIs or router integration, matching the approved design.
- Placeholder scan: there are no `TBD`, `TODO`, or "handle later" steps. Every task names exact files and commands.
- Type consistency: the plan uses `TurboQuantSearcher`, `TurboRecord512`, `MmapIndex`, `score_query_against_record_512`, and the new `score_query_terms_against_record_512` consistently across tasks.
