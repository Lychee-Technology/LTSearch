use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ltsearch::index::{
    encode_vector, CentroidTable, KnownRecordLayout, MetaRecord, MmapIndex, ProjectionMatrix,
    TurboHeader, TurboRecord512, TurboRecordSlice, META_RECORD_SIZE,
};
use ltsearch::models::{IndexManifest, ShardManifest};
use ltsearch::query::{StaticRetriever, TurboQuantSearcher};
use ltsearch::storage::{ActiveManifest, ManifestHead};

fn stub_manifest() -> ActiveManifest {
    ActiveManifest {
        head: ManifestHead {
            version_id: 1,
            manifest_path: "m.json".into(),
            updated_at: 0,
        },
        manifest: IndexManifest {
            version_id: 1,
            created_at: 0,
            embedding_dim: 512,
            document_count: 0,
            num_shards: 0,
            shards: vec![ShardManifest {
                shard_id: 0,
                document_count: 0,
                lance_path: String::new(),
                tantivy_path: String::new(),
            }],
        },
    }
}

const DIM: usize = 512;

struct FixtureDoc<'a> {
    doc_id: u64,
    corpus_type: u8,
    text: &'a str,
    embedding: Vec<f32>,
}

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-turbo-bench-{name}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn centroid_table(dim: u32, centroids_per_dim: u32, values: &[f32]) -> CentroidTable {
    let mut bytes = Vec::with_capacity(8 + values.len() * 4);
    bytes.extend_from_slice(&dim.to_le_bytes());
    bytes.extend_from_slice(&centroids_per_dim.to_le_bytes());
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    CentroidTable::from_bytes(&bytes).unwrap()
}

fn identity_projection(dim: usize) -> ProjectionMatrix {
    let mut rows = Vec::with_capacity(dim);
    for row_index in 0..dim {
        let mut row = vec![0.0; dim];
        row[row_index] = 1.0;
        rows.push(row);
    }
    ProjectionMatrix::from_rows(rows)
}

fn padded_embedding(seed: usize) -> Vec<f32> {
    let mut embedding = vec![0.0; DIM];
    for (index, value) in embedding.iter_mut().enumerate() {
        *value = match index % 16 {
            0..=3 => 1.2 + (seed % 5) as f32 * 0.02,
            4..=7 => 0.5 - (seed % 7) as f32 * 0.01,
            8..=11 => (index % 11) as f32 * 0.03,
            _ => -0.25 + ((seed + index) % 9) as f32 * 0.005,
        };
    }
    embedding
}

fn write_benchmark_index(dir: &Path, docs: &[FixtureDoc<'_>]) {
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

    for doc in docs {
        let encoded = encode_vector(&doc.embedding, &centroids, &projection).unwrap();
        let record = TurboRecord512 {
            doc_id: doc.doc_id,
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

        let text = doc.text;
        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(text.as_bytes());
        let meta = MetaRecord {
            doc_id: doc.doc_id,
            corpus_type: doc.corpus_type,
            _pad: [0; 7],
            title_offset: 0,
            title_len: 0,
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
    fs::write(dir.join("turbo_static_title.bin"), []).unwrap();
    fs::write(dir.join("centroids.bin"), centroids.to_bytes()).unwrap();
    fs::write(dir.join("projection.bin"), projection.to_bytes()).unwrap();
}

const DOC_COUNTS: [usize; 2] = [2_000, 10_000];
const WARMUP_QUERIES: usize = 3;
// Odd, so p50 is a single measured sample rather than an interpolation.
const MEASURED_QUERIES: usize = 25;
const TOP_K: usize = 10;

/// Run with `cargo test --release --test turbo_searcher_benchmark_test -- --ignored --nocapture`
/// to measure the shipping profile; the dev profile is only useful as a smoke test.
#[test]
#[ignore = "benchmark-style smoke test"]
fn turbo_searcher_benchmark_reports_typed_512d_search_latency() {
    for doc_count in DOC_COUNTS {
        benchmark_search_latency(doc_count);
    }
}

fn benchmark_search_latency(doc_count: usize) {
    let dir = temp_dir(&format!("smoke-{doc_count}"));
    let docs = (0..doc_count)
        .map(|doc_id| FixtureDoc {
            doc_id: (doc_id + 1) as u64,
            corpus_type: (doc_id % 3) as u8,
            text: "benchmark document",
            embedding: padded_embedding(doc_id),
        })
        .collect::<Vec<_>>();
    write_benchmark_index(&dir, &docs);

    let index = Arc::new(MmapIndex::load(&dir).unwrap());
    assert_eq!(index.layout(), KnownRecordLayout::V2Dim512);
    match index.records() {
        TurboRecordSlice::V2Dim512(records) => assert_eq!(records.len(), docs.len()),
    }
    let searcher = TurboQuantSearcher::new(index);

    let mut latencies_us = Vec::with_capacity(MEASURED_QUERIES);
    for query_index in 0..WARMUP_QUERIES + MEASURED_QUERIES {
        let query = padded_embedding(1_000 + query_index);
        let start = Instant::now();
        let results = searcher.search(&stub_manifest(), &query, TOP_K).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(results.len(), TOP_K);
        if query_index >= WARMUP_QUERIES {
            latencies_us.push(elapsed.as_secs_f64() * 1_000_000.0);
        }
    }
    latencies_us.sort_by(f64::total_cmp);

    println!(
        "turbo_searcher benchmark dim={DIM} docs={doc_count} top_k={TOP_K} queries={MEASURED_QUERIES} \
         rayon_threads={} debug_assertions={} p50_us={:.1} p95_us={:.1} min_us={:.1} max_us={:.1}",
        rayon::current_num_threads(),
        cfg!(debug_assertions),
        nearest_rank(&latencies_us, 0.50),
        nearest_rank(&latencies_us, 0.95),
        latencies_us[0],
        latencies_us[latencies_us.len() - 1],
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// Nearest-rank percentile over an ascending slice.
fn nearest_rank(sorted: &[f64], percentile: f64) -> f64 {
    let rank = (percentile * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}
