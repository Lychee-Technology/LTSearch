//! Parity between `PreparedTurboQuery` and the pre-#162 legacy scorer.
//!
//! `reference_score` keeps the arithmetic of the removed per-record scorer
//! (`score_query_against_record_512_breakdown`) as a test-only oracle. The
//! prepared path performs the same multiplications and sums them in the same
//! order, so parity is asserted bit-for-bit (tolerance 0), which is stricter
//! than a float-reassociation tolerance.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, MmapIndex, PreparedTurboQuery, ProjectionMatrix,
    TurboHeader, TurboQuantConfig, TurboRecord512, META_RECORD_SIZE,
};
use ltsearch::models::{IndexManifest, ShardManifest};
use ltsearch::query::{StaticRetriever, TurboQuantSearcher};
use ltsearch::storage::{ActiveManifest, ManifestHead};

const DIM: usize = 512;
const ENCODED_RECORD_COUNT: usize = 1_024;
const RAW_RECORD_COUNT: usize = 1_024;
const QUERY_COUNT: usize = 16;
const TOP_KS: [usize; 3] = [1, 10, 100];

/// The pre-#162 score `Σ_d q_d·c_d[idx_d] + γ·Σ_j sign_j·(S·q)_j`, summed in
/// dimension order. `projected_query` is `projection.project_checked(query)`,
/// which the legacy scorer recomputed for every record; it is deterministic,
/// so computing it once per query here changes no value, only test runtime.
fn reference_score(
    query: &[f32],
    projected_query: &[f32],
    record: &TurboRecord512,
    centroids: &CentroidTable,
) -> f32 {
    let centroid_term = (0..query.len())
        .map(|dim| query[dim] * centroids.values()[dim * 4 + read_idx(&record.idx, dim) as usize])
        .sum::<f32>();
    let qjl_term = projected_query
        .iter()
        .enumerate()
        .map(|(dim, value)| {
            value
                * if read_sign_bit(&record.qjl, dim) {
                    1.0
                } else {
                    -1.0
                }
        })
        .sum::<f32>();
    centroid_term + record.gamma * qjl_term
}

fn read_idx(bytes: &[u8], dim: usize) -> u8 {
    let bit_offset = dim * 2;
    (bytes[bit_offset / 8] >> (bit_offset % 8)) & 0b11
}

fn read_sign_bit(bytes: &[u8], dim: usize) -> bool {
    (bytes[dim / 8] >> (dim % 8)) & 1 == 1
}

/// The searcher's ranking: score descending, then doc_id ascending.
fn reference_top_k(scored: &[(u64, f32)], top_k: usize) -> Vec<(u64, f32)> {
    let mut ranked = scored.to_vec();
    ranked.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.truncate(top_k);
    ranked
}

/// SplitMix64, so fixtures are deterministic without a `rand` dev-dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [-1, 1).
    fn next_signed_unit(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }

    fn unit_vector(&mut self) -> Vec<f32> {
        let mut vector = (0..DIM)
            .map(|_| self.next_signed_unit())
            .collect::<Vec<_>>();
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        vector.iter_mut().for_each(|value| *value /= norm);
        vector
    }
}

fn encoded_record(
    doc_id: u64,
    embedding: &[f32],
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) -> TurboRecord512 {
    let encoded = encode_vector(embedding, centroids, projection).unwrap();
    TurboRecord512 {
        doc_id,
        idx: encoded.idx.try_into().unwrap(),
        qjl: encoded.qjl.try_into().unwrap(),
        gamma: encoded.gamma,
        _reserved: [0; 4],
    }
}

/// Arbitrary bit patterns, so every centroid index and sign is exercised in
/// every dimension regardless of what the encoder tends to emit.
fn raw_random_record(doc_id: u64, rng: &mut SplitMix64) -> TurboRecord512 {
    let mut record = TurboRecord512 {
        doc_id,
        idx: [0; 128],
        qjl: [0; 64],
        gamma: (rng.next_signed_unit() + 1.0) * 0.75,
        _reserved: [0; 4],
    };
    record
        .idx
        .iter_mut()
        .chain(record.qjl.iter_mut())
        .for_each(|byte| *byte = rng.next_u64() as u8);
    record
}

/// Asserts bit-identical scores for every (query, record) pair and identical
/// top-K for every K in `TOP_KS`.
fn assert_prepared_matches_reference(
    queries: &[Vec<f32>],
    records: &[TurboRecord512],
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) {
    for (query_index, query) in queries.iter().enumerate() {
        let projected_query = projection.project_checked(query).unwrap();
        let prepared = PreparedTurboQuery::prepare(query, centroids, projection).unwrap();

        let mut reference_scored = Vec::with_capacity(records.len());
        let mut prepared_scored = Vec::with_capacity(records.len());
        for (record_index, record) in records.iter().enumerate() {
            let expected = reference_score(query, &projected_query, record, centroids);
            let actual = prepared.score(record);
            assert_eq!(
                actual.to_bits(),
                expected.to_bits(),
                "query {query_index} record {record_index}: prepared {actual} != legacy {expected}"
            );
            reference_scored.push((record.doc_id, expected));
            prepared_scored.push((record.doc_id, actual));
        }

        for top_k in TOP_KS {
            assert_eq!(
                ranked_ids(&reference_top_k(&prepared_scored, top_k)),
                ranked_ids(&reference_top_k(&reference_scored, top_k)),
                "query {query_index} top_k {top_k}"
            );
        }
    }
}

fn ranked_ids(ranked: &[(u64, f32)]) -> Vec<u64> {
    ranked.iter().map(|(doc_id, _)| *doc_id).collect()
}

/// The assets the static builders generate for the legacy codec.
fn legacy_assets() -> (CentroidTable, ProjectionMatrix) {
    let legacy = TurboQuantConfig::legacy_v1();
    (
        CentroidTable::generate(legacy.dim, legacy.centroids_per_dim(), legacy.mse_seed),
        ProjectionMatrix::generate(legacy.dim, legacy.qjl_dim, legacy.qjl_seed),
    )
}

/// 1,024 records encoded from random unit embeddings plus 1,024 raw random
/// records, under the generated legacy assets. Half of the queries are
/// perturbed copies of indexed embeddings, so the top of each ranking is a
/// real near-neighbour rather than noise.
fn random_legacy_fixture() -> (
    CentroidTable,
    ProjectionMatrix,
    Vec<TurboRecord512>,
    Vec<Vec<f32>>,
) {
    let (centroids, projection) = legacy_assets();
    let mut rng = SplitMix64(0x0162_0162_0162_0162);

    let embeddings = (0..ENCODED_RECORD_COUNT)
        .map(|_| rng.unit_vector())
        .collect::<Vec<_>>();
    let mut records = embeddings
        .iter()
        .map(|embedding| encoded_record(rng.next_u64(), embedding, &centroids, &projection))
        .collect::<Vec<_>>();
    for _ in 0..RAW_RECORD_COUNT {
        let doc_id = rng.next_u64();
        records.push(raw_random_record(doc_id, &mut rng));
    }

    let queries = (0..QUERY_COUNT)
        .map(|query_index| {
            if query_index % 2 == 0 {
                rng.unit_vector()
            } else {
                let source = &embeddings[rng.next_u64() as usize % embeddings.len()];
                source
                    .iter()
                    .map(|value| value + rng.next_signed_unit() * 0.01)
                    .collect()
            }
        })
        .collect::<Vec<_>>();

    (centroids, projection, records, queries)
}

#[test]
fn prepared_scores_match_legacy_scorer_on_searcher_test_fixture_assets() {
    // The asset shape used by the turbo_searcher_* fixtures and the benchmark:
    // centroids [-1, 0, 1, 2] in every dimension and an identity projection.
    // Its sums are exact in any order, so it would not notice a change in
    // summation order; the generated-asset tests below do.
    let centroid_values = [-1.0f32, 0.0, 1.0, 2.0].repeat(DIM);
    let mut bytes = Vec::with_capacity(8 + centroid_values.len() * 4);
    bytes.extend_from_slice(&(DIM as u32).to_le_bytes());
    bytes.extend_from_slice(&4u32.to_le_bytes());
    for value in &centroid_values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let centroids = CentroidTable::from_bytes(&bytes).unwrap();
    let projection = ProjectionMatrix::from_rows(
        (0..DIM)
            .map(|row_index| {
                let mut row = vec![0.0; DIM];
                row[row_index] = 1.0;
                row
            })
            .collect(),
    );

    let records = (0..ENCODED_RECORD_COUNT)
        .map(|seed| {
            encoded_record(
                seed as u64 + 1,
                &benchmark_embedding(seed),
                &centroids,
                &projection,
            )
        })
        .collect::<Vec<_>>();
    let queries = (0..QUERY_COUNT)
        .map(|query_index| benchmark_embedding(1_000 + query_index))
        .collect::<Vec<_>>();

    assert_prepared_matches_reference(&queries, &records, &centroids, &projection);
}

/// `padded_embedding` from `legacy_plumbing_turbo_searcher_benchmark_test.rs`.
fn benchmark_embedding(seed: usize) -> Vec<f32> {
    (0..DIM)
        .map(|index| match index % 16 {
            0..=3 => 1.2 + (seed % 5) as f32 * 0.02,
            4..=7 => 0.5 - (seed % 7) as f32 * 0.01,
            8..=11 => (index % 11) as f32 * 0.03,
            _ => -0.25 + ((seed + index) % 9) as f32 * 0.005,
        })
        .collect()
}

#[test]
fn prepared_scores_match_legacy_scorer_on_random_records_with_legacy_assets() {
    let (centroids, projection, records, queries) = random_legacy_fixture();
    assert!(records.len() >= 1_000);

    assert_prepared_matches_reference(&queries, &records, &centroids, &projection);
}

#[test]
fn searcher_top_k_matches_legacy_ranking_end_to_end() {
    let (centroids, projection, records, queries) = random_legacy_fixture();
    let dir = temp_dir("e2e");
    write_v2_index(&dir, &records, &centroids, &projection);
    let searcher = TurboQuantSearcher::new(Arc::new(MmapIndex::load(&dir).unwrap()));

    for (query_index, query) in queries.iter().enumerate() {
        let projected_query = projection.project_checked(query).unwrap();
        let reference_scored = records
            .iter()
            .map(|record| {
                (
                    record.doc_id,
                    reference_score(query, &projected_query, record, &centroids),
                )
            })
            .collect::<Vec<_>>();

        for top_k in TOP_KS {
            let expected = reference_top_k(&reference_scored, top_k)
                .into_iter()
                .map(|(doc_id, score)| (doc_id.to_string(), score.to_bits()))
                .collect::<Vec<_>>();
            let actual = searcher
                .search(&stub_manifest(), query, top_k)
                .unwrap()
                .into_iter()
                .map(|result| (result.doc_id, result.score.to_bits()))
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "query {query_index} top_k {top_k}");
        }
    }

    fs::remove_dir_all(&dir).unwrap();
}

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
            embedding_dim: DIM,
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

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-turbo-parity-{name}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_v2_index(
    dir: &Path,
    records: &[TurboRecord512],
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
) {
    const TEXT: &str = "parity document";
    let mut bin_data = TurboHeader::new(DIM as u32, records.len() as u64).to_bytes();
    let mut meta_data = Vec::with_capacity(records.len() * META_RECORD_SIZE);
    let mut text_blob = Vec::new();

    for record in records {
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                record as *const TurboRecord512 as *const u8,
                std::mem::size_of::<TurboRecord512>(),
            )
        };
        bin_data.extend_from_slice(record_bytes);

        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(TEXT.as_bytes());
        let meta = MetaRecord {
            doc_id: record.doc_id,
            corpus_type: 0,
            _pad: [0; 7],
            title_offset: 0,
            title_len: 0,
            text_offset,
            text_len: TEXT.len() as u32,
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

#[test]
fn reference_top_k_breaks_score_ties_by_ascending_doc_id() {
    let scored = [(30, 1.0), (10, 1.0), (20, 2.0), (5, 0.5)];
    assert_eq!(ranked_ids(&reference_top_k(&scored, 3)), vec![20, 10, 30]);
}
