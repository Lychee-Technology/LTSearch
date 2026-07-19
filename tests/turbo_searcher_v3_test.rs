//! v3 materialization surface for `TurboQuantSearcher`: the static searcher must
//! expose the original string `doc_id`, the freeform `metadata` map, and a
//! metadata-derived `citation` for v3 images, and those results must survive a
//! metadata `apply_filters` pass. v2 images keep their legacy behavior (hashed
//! u64 doc_id, `metadata: None`, title-only citation) — the last test guards it.

use std::fs;
use std::path::{Path, PathBuf};

use ltsearch::index::{
    encode_vector, CentroidTable, MetaExtRecord, MetaRecord, MmapIndex, ProjectionMatrix,
    TurboHeader, TurboRecord512, META_EXT_RECORD_SIZE, META_RECORD_SIZE,
};
use ltsearch::models::{FilterValue, IndexManifest, ShardManifest};
use ltsearch::query::filter::apply_filters;
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

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ltsearch-turbo-v3-{name}-{unique}"));
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

struct Doc {
    doc_id: u64,
    doc_id_str: &'static str,
    corpus_type: u8,
    text: &'static str,
    title: Option<&'static str>,
    /// Raw metadata JSON blob for this record. `Some` writes the v3 sidecars.
    metadata_json: Option<String>,
    embedding: Vec<f32>,
}

/// Writes a turbo static index into `dir`. When any doc carries a
/// `metadata_json`, the v3 sidecars (`meta_ext` / `docid` / `meta_json`) are
/// emitted and the header is stamped v3; otherwise a plain v2 image is written.
fn write_index(dir: &Path, docs: &[Doc]) {
    let dim = 512u32;
    let is_v3 = docs.iter().any(|doc| doc.metadata_json.is_some());

    let mut centroid_values = Vec::with_capacity(dim as usize * 4);
    for _ in 0..dim {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(dim, 4, &centroid_values);
    let projection = identity_projection(dim as usize);

    let header = if is_v3 {
        TurboHeader::new_v3(dim, docs.len() as u64)
    } else {
        TurboHeader::new(dim, docs.len() as u64)
    };

    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();
    let mut title_blob = Vec::new();
    let mut ext_data = Vec::new();
    let mut docid_blob = Vec::new();
    let mut json_blob = Vec::new();

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

        if is_v3 {
            let docid_offset = docid_blob.len() as u64;
            let docid_len = doc.doc_id_str.len() as u32;
            docid_blob.extend_from_slice(doc.doc_id_str.as_bytes());
            let meta_json = doc
                .metadata_json
                .clone()
                .unwrap_or_else(|| "{}".to_string());
            let meta_json_offset = json_blob.len() as u64;
            let meta_json_len = meta_json.len() as u32;
            json_blob.extend_from_slice(meta_json.as_bytes());
            let ext = MetaExtRecord {
                docid_offset,
                meta_json_offset,
                docid_len,
                meta_json_len,
            };
            let ext_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    &ext as *const MetaExtRecord as *const u8,
                    META_EXT_RECORD_SIZE,
                )
            };
            ext_data.extend_from_slice(ext_bytes);
        }
    }

    fs::write(dir.join("turbo_static.bin"), &bin_data).unwrap();
    fs::write(dir.join("turbo_static_meta.bin"), &meta_data).unwrap();
    fs::write(dir.join("turbo_static_text.bin"), &text_blob).unwrap();
    fs::write(dir.join("turbo_static_title.bin"), &title_blob).unwrap();
    fs::write(dir.join("centroids.bin"), centroids.to_bytes()).unwrap();
    fs::write(dir.join("projection.bin"), projection.to_bytes()).unwrap();
    if is_v3 {
        fs::write(dir.join("turbo_static_meta_ext.bin"), &ext_data).unwrap();
        fs::write(dir.join("turbo_static_docid.bin"), &docid_blob).unwrap();
        fs::write(dir.join("turbo_static_meta_json.bin"), &json_blob).unwrap();
    }
}

fn load_searcher(dir: &Path) -> TurboQuantSearcher {
    let index = Box::new(MmapIndex::load(dir).unwrap());
    TurboQuantSearcher::new(Box::leak(index))
}

fn alpha_metadata() -> String {
    r#"{"resource_id":"res-α","source_type":"statute","source_ref":"第一条","title":"宪法总纲","url":"https://example.com/law-α","lang":"zh"}"#.to_string()
}

fn beta_metadata() -> String {
    r#"{"resource_id":"res-β","source_type":"statute","source_ref":"第二条","title":"合同法则","url":"https://example.com/law-β","lang":"en"}"#.to_string()
}

fn alpha_beta_docs() -> Vec<Doc> {
    vec![
        Doc {
            doc_id: 111,
            doc_id_str: "doc-α",
            corpus_type: 0,
            text: "第一条文本",
            title: Some("宪法总纲"),
            metadata_json: Some(alpha_metadata()),
            embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        },
        Doc {
            doc_id: 222,
            doc_id_str: "doc-β",
            corpus_type: 1,
            text: "第二条文本",
            title: Some("合同法则"),
            metadata_json: Some(beta_metadata()),
            embedding: padded_embedding(&[0.2, 0.4, -0.3, 0.1]),
        },
    ]
}

#[test]
fn v3_searcher_exposes_original_doc_id_metadata_and_citation() {
    let dir = temp_dir("exposes");
    write_index(&dir, &alpha_beta_docs());

    let searcher = load_searcher(&dir);
    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            10,
        )
        .unwrap();

    let alpha = results
        .iter()
        .find(|r| r.doc_id == "doc-α")
        .expect("v3 result must carry the original string doc_id, not a hashed u64");

    // metadata map is exposed verbatim from the sidecar
    let metadata = alpha
        .metadata
        .as_ref()
        .expect("v3 result must expose its metadata map");
    assert_eq!(metadata.get("lang"), Some(&serde_json::json!("zh")));
    assert_eq!(
        metadata.get("resource_id"),
        Some(&serde_json::json!("res-α"))
    );

    // citation is derived from the metadata, not hand-built from the title
    let citation = alpha
        .citation
        .as_ref()
        .expect("v3 result must expose a metadata-derived citation");
    assert_eq!(citation.resource_id, "res-α");
    assert_eq!(citation.title.as_deref(), Some("宪法总纲"));
    assert_eq!(citation.url.as_deref(), Some("https://example.com/law-α"));
}

#[test]
fn v3_searcher_results_survive_metadata_filter() {
    let dir = temp_dir("filter");
    write_index(&dir, &alpha_beta_docs());

    let searcher = load_searcher(&dir);
    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            10,
        )
        .unwrap();
    assert_eq!(results.len(), 2, "both docs are returned before filtering");

    let mut filters = std::collections::HashMap::new();
    filters.insert(
        "lang".to_string(),
        FilterValue::StringEquals("zh".to_string()),
    );
    let filtered = apply_filters(results, Some(&filters));

    assert_eq!(filtered.len(), 1, "lang==zh keeps α and drops β");
    assert_eq!(filtered[0].doc_id, "doc-α");
}

#[test]
fn v3_searcher_falls_back_to_none_on_unparseable_metadata() {
    let dir = temp_dir("unparseable");
    write_index(
        &dir,
        &[Doc {
            doc_id: 333,
            doc_id_str: "doc-γ",
            corpus_type: 0,
            text: "损坏元数据文本",
            title: Some("损坏条目"),
            // valid UTF-8 but not valid JSON
            metadata_json: Some("this is not json {".to_string()),
            embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        }],
    );

    let searcher = load_searcher(&dir);
    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            10,
        )
        .unwrap();

    assert_eq!(results.len(), 1);
    let result = &results[0];
    // original doc_id is still exposed; only metadata parsing failed
    assert_eq!(result.doc_id, "doc-γ");
    assert_eq!(
        result.metadata, None,
        "unparseable metadata must fall back to None, not panic"
    );
    // citation falls back to the v2 title-only construction
    let citation = result
        .citation
        .as_ref()
        .expect("title-only citation must survive the metadata fallback");
    assert_eq!(citation.title.as_deref(), Some("损坏条目"));
    assert_eq!(citation.resource_id, "doc-γ");
    assert_eq!(citation.source_ref, "doc-γ");
    assert_eq!(citation.url, None);
}

#[test]
fn v2_searcher_keeps_hashed_docid_and_none_metadata() {
    let dir = temp_dir("v2-guardrail");
    write_index(
        &dir,
        &[Doc {
            doc_id: 42,
            doc_id_str: "unused-in-v2",
            corpus_type: 0,
            text: "民法典正文",
            title: Some("民法典"),
            // no metadata_json anywhere => v2 image
            metadata_json: None,
            embedding: padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        }],
    );

    let searcher = load_searcher(&dir);
    let results = searcher
        .search(
            &stub_manifest(),
            &padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
            10,
        )
        .unwrap();

    assert_eq!(results.len(), 1);
    let result = &results[0];
    // v2 keeps the hashed u64 doc_id and no metadata map
    assert_eq!(result.doc_id, "42");
    assert_eq!(result.metadata, None);
    // v2 citation is title-only with the hashed doc_id as resource/source ref
    let citation = result
        .citation
        .as_ref()
        .expect("titled v2 chunk must carry a title-only citation");
    assert_eq!(citation.title.as_deref(), Some("民法典"));
    assert_eq!(citation.resource_id, "42");
    assert_eq!(citation.source_ref, "42");
    assert_eq!(citation.url, None);
}
