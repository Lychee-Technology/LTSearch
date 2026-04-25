use std::fs;
use std::path::{Path, PathBuf};

use ltsearch::error::{SearchError, ValidationError};
use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader,
    META_RECORD_SIZE,
};
use ltsearch::models::{CorpusType, SearchSource};
use ltsearch::query::TurboQuantSearcher;

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-turbo-searcher-{name}-{unique}"));
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

fn record_bytes(header: &TurboHeader, doc_id: u64, idx: &[u8], qjl: &[u8], gamma: f32) -> Vec<u8> {
    let mut record = vec![0u8; header.record_stride()];
    record[0..8].copy_from_slice(&doc_id.to_le_bytes());

    let idx_offset = header.idx_offset();
    record[idx_offset..idx_offset + idx.len()].copy_from_slice(idx);

    let qjl_offset = header.qjl_offset();
    record[qjl_offset..qjl_offset + qjl.len()].copy_from_slice(qjl);

    let gamma_offset = header.gamma_offset();
    record[gamma_offset..gamma_offset + 4].copy_from_slice(&gamma.to_le_bytes());
    record
}

struct FixtureDoc<'a> {
    doc_id: u64,
    corpus_type: u8,
    text: &'a str,
    embedding: &'a [f32],
}

fn write_test_index(dir: &Path, dim: u32, docs: &[FixtureDoc<'_>]) {
    let centroids = centroid_table(
        dim,
        4,
        &[
            -1.0, 0.0, 1.0, 2.0, -2.0, -1.0, 0.0, 1.0, 0.0, 1.0, 2.0, 3.0, -1.0, 0.0, 1.0, 3.0,
        ],
    );
    let projection = identity_projection(dim as usize);
    let header = TurboHeader::new(dim, docs.len() as u64);

    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();

    for doc in docs {
        let encoded = encode_vector(doc.embedding, &centroids, &projection).unwrap();
        bin_data.extend_from_slice(&record_bytes(
            &header,
            doc.doc_id,
            &encoded.idx,
            &encoded.qjl,
            encoded.gamma,
        ));

        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(doc.text.as_bytes());
        let meta = MetaRecord {
            doc_id: doc.doc_id,
            corpus_type: doc.corpus_type,
            _pad: [0; 3],
            text_offset,
            text_len: doc.text.len() as u32,
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

#[test]
fn turbo_searcher_returns_static_results_with_corpus_mapping_and_stable_tie_breaks() {
    let dir = temp_dir("results-and-ties");
    write_test_index(
        &dir,
        4,
        &[
            FixtureDoc {
                doc_id: 20,
                corpus_type: 2,
                text: "rfc twenty",
                embedding: &[1.2, -1.4, 0.3, 0.9],
            },
            FixtureDoc {
                doc_id: 10,
                corpus_type: 0,
                text: "legal ten",
                embedding: &[1.2, -1.4, 0.3, 0.9],
            },
            FixtureDoc {
                doc_id: 30,
                corpus_type: 1,
                text: "contract thirty",
                embedding: &[0.0, 0.0, 0.0, 0.0],
            },
        ],
    );

    let searcher = load_searcher(&dir);

    let results = searcher.search(&[1.2, -1.4, 0.3, 0.9], 2).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].doc_id, "10");
    assert_eq!(results[0].text, "legal ten");
    assert_eq!(results[0].source, SearchSource::Static);
    assert_eq!(results[0].metadata, None);
    assert_eq!(results[0].corpus_type, Some(CorpusType::Legal));

    assert_eq!(results[1].doc_id, "20");
    assert_eq!(results[1].text, "rfc twenty");
    assert_eq!(results[1].source, SearchSource::Static);
    assert_eq!(results[1].metadata, None);
    assert_eq!(results[1].corpus_type, Some(CorpusType::Rfc));

    assert!(results[0].score >= results[1].score);
}

#[test]
fn turbo_searcher_rejects_query_embeddings_with_wrong_dimension() {
    let dir = temp_dir("dimension-mismatch");
    write_test_index(
        &dir,
        4,
        &[FixtureDoc {
            doc_id: 1,
            corpus_type: 0,
            text: "legal one",
            embedding: &[1.2, -1.4, 0.3, 0.9],
        }],
    );

    let searcher = load_searcher(&dir);
    let error = searcher.search(&[1.0, 0.0, 0.0], 1).unwrap_err();

    assert!(matches!(
        error,
        SearchError::Validation(ValidationError::InvalidValue {
            field: "query_embedding"
        })
    ));
}

#[test]
fn turbo_searcher_returns_best_top_k_without_leaking_lower_ranked_hits() {
    let dir = temp_dir("bounded-top-k");
    write_test_index(
        &dir,
        4,
        &[
            FixtureDoc {
                doc_id: 5,
                corpus_type: 0,
                text: "best",
                embedding: &[1.2, -1.4, 0.3, 0.9],
            },
            FixtureDoc {
                doc_id: 4,
                corpus_type: 1,
                text: "second",
                embedding: &[1.0, -1.0, 0.0, 1.0],
            },
            FixtureDoc {
                doc_id: 3,
                corpus_type: 2,
                text: "third",
                embedding: &[0.8, -0.8, 0.0, 0.8],
            },
            FixtureDoc {
                doc_id: 2,
                corpus_type: 0,
                text: "fourth",
                embedding: &[0.0, 0.0, 0.0, 0.0],
            },
        ],
    );

    let searcher = load_searcher(&dir);

    let results = searcher.search(&[1.2, -1.4, 0.3, 0.9], 3).unwrap();

    assert_eq!(results.len(), 3);
    assert_eq!(
        results
            .iter()
            .map(|result| result.doc_id.as_str())
            .collect::<Vec<_>>(),
        vec!["5", "4", "3"]
    );
    assert!(results[0].score >= results[1].score);
    assert!(results[1].score >= results[2].score);
}
