use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader,
    TurboRecord512, META_RECORD_SIZE,
};
use ltsearch::query::{StaticRetriever, TurboQuantSearcher};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

const DIM: usize = 512;
const TOP_K: usize = 10;
const TOPICS: usize = 8;
const DOCS_PER_TOPIC: usize = 10;
const QUERIES_PER_TOPIC: usize = 2;
const TOPIC_WIDTH: usize = DIM / TOPICS;
const BLOCK_WIDTH: usize = TOPIC_WIDTH / 2;
const DOCUMENT_SEED: u64 = 72;
const QUERY_SEED: u64 = 7_200;
const MIN_MEAN_RECALL: f32 = 0.98;
const MIN_QUERY_RECALL: f32 = 0.90;

#[test]
fn turbo_searcher_recall_stays_above_ninety_percent_against_exact_dot_product() {
    let dir = temp_dir("recall-regression");
    let dataset = synthetic_dataset();
    write_test_index(&dir, &dataset.documents);
    let searcher = load_searcher(&dir);

    let report = recall_report(&searcher, &dataset.documents, &dataset.queries, TOP_K);

    assert!(
        report.mean_recall >= MIN_MEAN_RECALL,
        "expected mean recall >= {MIN_MEAN_RECALL:.2}, got {mean:.4}\n{details}",
        mean = report.mean_recall,
        details = report.failure_details(MIN_QUERY_RECALL)
    );

    let weakest = report
        .query_reports
        .iter()
        .find(|query| query.recall < MIN_QUERY_RECALL);
    assert!(
        weakest.is_none(),
        "expected every query recall >= {MIN_QUERY_RECALL:.2}\n{details}",
        details = report.failure_details(MIN_QUERY_RECALL)
    );
}

struct SyntheticDataset {
    documents: Vec<Vec<f32>>,
    queries: Vec<Vec<f32>>,
}

struct RecallReport {
    mean_recall: f32,
    query_reports: Vec<QueryRecall>,
}

struct QueryRecall {
    index: usize,
    recall: f32,
    exact_doc_ids: Vec<usize>,
    turbo_doc_ids: Vec<usize>,
}

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-turbo-recall-{name}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn synthetic_dataset() -> SyntheticDataset {
    let mut document_rng = ChaCha8Rng::seed_from_u64(DOCUMENT_SEED);
    let mut query_rng = ChaCha8Rng::seed_from_u64(QUERY_SEED);
    let mut documents = Vec::with_capacity(TOPICS * DOCS_PER_TOPIC);
    let mut queries = Vec::with_capacity(TOPICS * QUERIES_PER_TOPIC);

    for topic in 0..TOPICS {
        for doc_variant in 0..DOCS_PER_TOPIC {
            documents.push(synthetic_vector(
                &mut document_rng,
                topic,
                Some(doc_variant),
            ));
        }

        for _ in 0..QUERIES_PER_TOPIC {
            queries.push(synthetic_vector(&mut query_rng, topic, None));
        }
    }

    SyntheticDataset { documents, queries }
}

impl RecallReport {
    fn failure_details(&self, min_query_recall: f32) -> String {
        let failing_queries = self
            .query_reports
            .iter()
            .filter(|query| query.recall < min_query_recall)
            .map(QueryRecall::failure_line)
            .collect::<Vec<_>>();

        if failing_queries.is_empty() {
            let weakest = self
                .query_reports
                .iter()
                .min_by(|left, right| left.recall.total_cmp(&right.recall))
                .map(QueryRecall::failure_line)
                .unwrap_or_else(|| "no query reports".to_string());
            format!("weakest query: {weakest}")
        } else {
            failing_queries.join("\n")
        }
    }
}

impl QueryRecall {
    fn failure_line(&self) -> String {
        format!(
            "query {} recall={:.4} exact={:?} turbo={:?}",
            self.index, self.recall, self.exact_doc_ids, self.turbo_doc_ids
        )
    }
}

fn synthetic_vector(rng: &mut ChaCha8Rng, topic: usize, doc_variant: Option<usize>) -> Vec<f32> {
    let primary_block = topic * TOPIC_WIDTH;
    let secondary_block = primary_block + BLOCK_WIDTH;
    let variant = doc_variant.unwrap_or(0);
    let mut vector = Vec::with_capacity(DIM);

    for dim in 0..DIM {
        let base = if (primary_block..primary_block + BLOCK_WIDTH).contains(&dim) {
            if let Some(doc_variant) = doc_variant {
                if (doc_variant + dim) % 5 == 0 {
                    1.32
                } else {
                    1.94
                }
            } else {
                2.12
            }
        } else if (secondary_block..secondary_block + BLOCK_WIDTH).contains(&dim) {
            if let Some(doc_variant) = doc_variant {
                if (doc_variant + dim) % 3 == 0 {
                    0.88
                } else {
                    1.24
                }
            } else {
                1.10
            }
        } else if let Some(doc_variant) = doc_variant {
            if (doc_variant + dim + topic).is_multiple_of(11) {
                -0.12
            } else {
                -1.08
            }
        } else {
            -1.02
        };

        let noise = if doc_variant.is_some() {
            rng.gen_range(-0.04..=0.04)
        } else {
            rng.gen_range(-0.03..=0.03)
        };

        vector.push(base + noise + variant_bias(dim, variant));
    }

    vector
}

fn variant_bias(dim: usize, variant: usize) -> f32 {
    match (dim + variant) % 7 {
        0 => -0.02,
        1 => -0.01,
        2 => 0.0,
        3 => 0.01,
        4 => 0.02,
        5 => -0.015,
        _ => 0.015,
    }
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

fn write_test_index(dir: &Path, documents: &[Vec<f32>]) {
    let mut centroid_values = Vec::with_capacity(DIM * 4);
    for _ in 0..DIM {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(DIM as u32, 4, &centroid_values);
    let projection = identity_projection(DIM);
    let header = TurboHeader::new(DIM as u32, documents.len() as u64);

    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();

    for (index, embedding) in documents.iter().enumerate() {
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

        let text = format!("document {}", index + 1);
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

fn load_searcher(dir: &Path) -> TurboQuantSearcher {
    let index = Box::new(MmapIndex::load(dir).unwrap());
    TurboQuantSearcher::new(Box::leak(index))
}

fn recall_report(
    searcher: &TurboQuantSearcher,
    documents: &[Vec<f32>],
    queries: &[Vec<f32>],
    top_k: usize,
) -> RecallReport {
    let query_reports = queries
        .iter()
        .enumerate()
        .map(|(index, query)| {
            let exact = exact_top_k_doc_ids(documents, query, top_k);
            let turbo_doc_ids = searcher
                .search(query, top_k)
                .unwrap()
                .into_iter()
                .map(|result| result.doc_id.parse::<usize>().unwrap())
                .collect::<Vec<_>>();
            let turbo_doc_ids_set = turbo_doc_ids.iter().copied().collect::<HashSet<_>>();

            let hits = exact
                .iter()
                .filter(|doc_id| turbo_doc_ids_set.contains(doc_id))
                .count();

            QueryRecall {
                index,
                recall: hits as f32 / top_k as f32,
                exact_doc_ids: exact,
                turbo_doc_ids,
            }
        })
        .collect::<Vec<_>>();

    let mean_recall =
        query_reports.iter().map(|query| query.recall).sum::<f32>() / query_reports.len() as f32;

    RecallReport {
        mean_recall,
        query_reports,
    }
}

fn exact_top_k_doc_ids(documents: &[Vec<f32>], query: &[f32], top_k: usize) -> Vec<usize> {
    let mut scored = documents
        .iter()
        .enumerate()
        .map(|(index, document)| (index + 1, dot_product(query, document)))
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    scored
        .into_iter()
        .take(top_k)
        .map(|(doc_id, _)| doc_id)
        .collect()
}

fn dot_product(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(l, r)| l * r).sum()
}
