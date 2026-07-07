use std::fs;
use std::path::{Path, PathBuf};

use ltsearch::error::{SearchError, ValidationError};
use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, MmapIndex, ProjectionMatrix, TurboHeader,
    TurboRecord512, META_RECORD_SIZE,
};
use ltsearch::models::{CorpusType, SearchSource};
use ltsearch::models::{IndexManifest, ShardManifest};
use ltsearch::query::{ContextBuilder, StaticRetriever, TurboQuantSearcher};
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

fn padded_embedding(prefix: &[f32]) -> Vec<f32> {
    let mut embedding = vec![0.0; 512];
    embedding[..prefix.len()].copy_from_slice(prefix);
    embedding
}

struct FixtureDoc<'a> {
    doc_id: u64,
    corpus_type: u8,
    text: &'a str,
    title: Option<&'a str>,
    embedding: Vec<f32>,
}

fn write_test_index(dir: &Path, dim: u32, docs: &[FixtureDoc<'_>]) {
    assert_eq!(dim, 512);
    let mut centroid_values = Vec::with_capacity(dim as usize * 4);
    for _ in 0..dim {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(dim, 4, &centroid_values);
    let projection = identity_projection(dim as usize);
    let header = TurboHeader::new(dim, docs.len() as u64);

    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();
    let mut title_blob = Vec::new();

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

        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(doc.text.as_bytes());
        let title_offset = title_blob.len() as u64;
        let title_len = match doc.title {
            Some(title) => {
                title_blob.extend_from_slice(title.as_bytes());
                title.len() as u32
            }
            None => 0,
        };
        let meta = MetaRecord {
            doc_id: doc.doc_id,
            corpus_type: doc.corpus_type,
            _pad: [0; 7],
            text_offset,
            text_len: doc.text.len() as u32,
            title_offset,
            title_len,
        };
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&meta as *const MetaRecord as *const u8, META_RECORD_SIZE)
        };
        meta_data.extend_from_slice(meta_bytes);
    }

    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();
    fs::write(dir.join("turbo_static_meta.bin"), &meta_data).unwrap();
    fs::write(dir.join("turbo_static_text.bin"), &text_blob).unwrap();
    fs::write(dir.join("turbo_static_title.bin"), &title_blob).unwrap();
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
        512,
        &[
            FixtureDoc {
                doc_id: 20,
                corpus_type: 2,
                text: "rfc twenty",
                title: None,
                embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            },
            FixtureDoc {
                doc_id: 10,
                corpus_type: 0,
                text: "legal ten",
                title: None,
                embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            },
            FixtureDoc {
                doc_id: 30,
                corpus_type: 1,
                text: "contract thirty",
                title: None,
                embedding: padded_embedding(&[0.0, 0.0, 0.0, 0.0]),
            },
        ],
    );

    let searcher = load_searcher(&dir);

    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            2,
        )
        .unwrap();

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
        512,
        &[FixtureDoc {
            doc_id: 1,
            corpus_type: 0,
            text: "legal one",
            title: None,
            embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        }],
    );

    let searcher = load_searcher(&dir);
    let error = searcher
        .search(&stub_manifest(), &[1.0, 0.0, 0.0], 1)
        .unwrap_err();

    assert!(matches!(
        error,
        SearchError::Validation(ValidationError::InvalidValue {
            field: "query_embedding"
        })
    ));
}

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
            title: None,
            embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        }],
    );

    let searcher = load_searcher(&dir);

    for top_k in [0, 101] {
        let error = searcher
            .search(
                &stub_manifest(),
                &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
                top_k,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            SearchError::Validation(ValidationError::RangeOutOfRange {
                field: "top_k",
                min: 1,
                max: 100,
            })
        ));
    }
}

#[test]
fn turbo_searcher_allows_top_k_at_the_maximum_and_returns_all_available_docs() {
    let dir = temp_dir("top-k-maximum-success");
    write_test_index(
        &dir,
        512,
        &[
            FixtureDoc {
                doc_id: 10,
                corpus_type: 0,
                text: "legal ten",
                title: None,
                embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            },
            FixtureDoc {
                doc_id: 20,
                corpus_type: 1,
                text: "contract twenty",
                title: None,
                embedding: padded_embedding(&[1.0, -1.0, 0.0, 1.0]),
            },
            FixtureDoc {
                doc_id: 30,
                corpus_type: 2,
                text: "rfc thirty",
                title: None,
                embedding: padded_embedding(&[0.8, -0.8, 0.0, 0.8]),
            },
        ],
    );

    let searcher = load_searcher(&dir);
    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            100,
        )
        .unwrap();

    assert_eq!(results.len(), 3);
    assert_eq!(
        results
            .iter()
            .map(|result| result.doc_id.as_str())
            .collect::<Vec<_>>(),
        vec!["10", "20", "30"]
    );
}

#[test]
fn turbo_searcher_returns_stable_single_document_results_and_scores() {
    let dir = temp_dir("single-doc-stability");
    let query = padded_embedding(&[1.2, -1.4, 0.3, 0.9]);
    write_test_index(
        &dir,
        512,
        &[FixtureDoc {
            doc_id: 42,
            corpus_type: 1,
            text: "contract forty-two",
            title: None,
            embedding: query.clone(),
        }],
    );

    let searcher = load_searcher(&dir);

    let first_results = searcher.search(&stub_manifest(), &query, 1).unwrap();
    let second_results = searcher.search(&stub_manifest(), &query, 1).unwrap();

    assert_eq!(first_results.len(), 1);
    assert_eq!(second_results.len(), 1);

    let first = &first_results[0];
    let second = &second_results[0];

    assert_eq!(first.doc_id, "42");
    assert_eq!(first.text, "contract forty-two");
    assert_eq!(first.source, SearchSource::Static);
    assert_eq!(first.metadata, None);
    assert_eq!(first.corpus_type, Some(CorpusType::Contract));
    assert!(first.score.is_finite());
    assert!(first.score > 0.0);

    assert_eq!(second.doc_id, first.doc_id);
    assert_eq!(second.text, first.text);
    assert_eq!(second.source, first.source);
    assert_eq!(second.metadata, first.metadata);
    assert_eq!(second.corpus_type, first.corpus_type);
    assert_eq!(second.score, first.score);
}

#[test]
fn turbo_searcher_returns_best_top_k_without_leaking_lower_ranked_hits() {
    let dir = temp_dir("bounded-top-k");
    write_test_index(
        &dir,
        512,
        &[
            FixtureDoc {
                doc_id: 5,
                corpus_type: 0,
                text: "best",
                title: None,
                embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            },
            FixtureDoc {
                doc_id: 4,
                corpus_type: 1,
                text: "second",
                title: None,
                embedding: padded_embedding(&[1.0, -1.0, 0.0, 1.0]),
            },
            FixtureDoc {
                doc_id: 3,
                corpus_type: 2,
                text: "third",
                title: None,
                embedding: padded_embedding(&[0.8, -0.8, 0.0, 0.8]),
            },
            FixtureDoc {
                doc_id: 2,
                corpus_type: 0,
                text: "fourth",
                title: None,
                embedding: padded_embedding(&[0.0, 0.0, 0.0, 0.0]),
            },
        ],
    );

    let searcher = load_searcher(&dir);

    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            3,
        )
        .unwrap();

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

#[test]
fn turbo_searcher_populates_citation_from_title_and_leaves_untitled_bare() {
    let dir = temp_dir("citation-from-title");
    write_test_index(
        &dir,
        512,
        &[
            FixtureDoc {
                doc_id: 10,
                corpus_type: 0,
                text: "legal ten",
                title: Some("民法典"),
                embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            },
            FixtureDoc {
                doc_id: 20,
                corpus_type: 2,
                text: "rfc twenty",
                title: None,
                embedding: padded_embedding(&[1.0, -1.0, 0.0, 1.0]),
            },
        ],
    );

    let searcher = load_searcher(&dir);
    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            2,
        )
        .unwrap();

    assert_eq!(results.len(), 2);

    let titled = &results[0];
    assert_eq!(titled.doc_id, "10");
    let citation = titled
        .citation
        .as_ref()
        .expect("titled static chunk must carry a citation");
    assert_eq!(citation.title.as_deref(), Some("民法典"));
    assert_eq!(citation.source_type, "legal");
    assert_eq!(citation.resource_id, "10");
    assert_eq!(citation.source_ref, "10");
    assert_eq!(citation.url, None);

    let untitled = &results[1];
    assert_eq!(untitled.doc_id, "20");
    assert!(untitled.citation.is_none());
}

/// End-to-end: a static index built with a `metadata["title"]` renders the
/// enriched `[法规 #1] <title>` label through the real searcher → ContextBuilder
/// pipeline, while an untitled chunk keeps a bare label.
#[test]
fn static_title_renders_into_assembled_context_end_to_end() {
    let dir = temp_dir("context-e2e");
    write_test_index(
        &dir,
        512,
        &[
            FixtureDoc {
                doc_id: 10,
                corpus_type: 0,
                text: "民法典正文",
                title: Some("民法典"),
                embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            },
            FixtureDoc {
                doc_id: 20,
                corpus_type: 0,
                text: "无标题条文",
                title: None,
                embedding: padded_embedding(&[1.0, -1.0, 0.0, 1.0]),
            },
        ],
    );

    let searcher = load_searcher(&dir);
    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            2,
        )
        .unwrap();
    assert_eq!(results[0].doc_id, "10");

    let context = ContextBuilder::build_context(&results, &[], "民法典是什么?");

    // The titled top chunk carries its title into the label; the untitled chunk
    // renders a bare label with no title text.
    assert!(context.contains("[法规 #1] 民法典\n民法典正文"));
    assert!(context.contains("[法规 #2]\n无标题条文"));
}
