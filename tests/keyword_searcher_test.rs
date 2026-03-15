use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::models::{FilterValue, SearchRequest};
use ltsearch::query::KeywordSearcher;
use ltsearch::storage::{version_manifest_key, LocalManifestStore, INDEX_HEAD_KEY};
use serde_json::json;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{Index, IndexWriter};

fn temp_fixture_dir(test_name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ltsearch-{test_name}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_fixture(root: &Path, relative_path: &str, contents: &str) {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn write_index(root: &Path, relative_path: &str, documents: &[(&str, &str)]) {
    let index_path = root.join(relative_path);
    fs::create_dir_all(&index_path).unwrap();

    let mut schema_builder = Schema::builder();
    let doc_id = schema_builder.add_text_field("doc_id", TEXT | STORED);
    let text = schema_builder.add_text_field("text", TEXT | STORED);
    let schema = schema_builder.build();

    let index = Index::create_in_dir(&index_path, schema).unwrap();
    let mut writer: IndexWriter = index.writer(15_000_000).unwrap();

    for (document_id, body) in documents {
        writer
            .add_document(doc!(doc_id => (*document_id).to_string(), text => (*body).to_string()))
            .unwrap();
    }

    writer.commit().unwrap();
    index
        .reader_builder()
        .try_into()
        .unwrap()
        .searcher()
        .search(
            &tantivy::query::AllQuery,
            &TopDocs::with_limit(documents.len().max(1)),
        )
        .unwrap();
}

fn write_index_with_metadata(
    root: &Path,
    relative_path: &str,
    documents: &[(&str, &str, serde_json::Value)],
) {
    let index_path = root.join(relative_path);
    fs::create_dir_all(&index_path).unwrap();

    let mut schema_builder = Schema::builder();
    let doc_id = schema_builder.add_text_field("doc_id", TEXT | STORED);
    let text = schema_builder.add_text_field("text", TEXT | STORED);
    let metadata = schema_builder.add_text_field("metadata", STORED);
    let schema = schema_builder.build();

    let index = Index::create_in_dir(&index_path, schema).unwrap();
    let mut writer: IndexWriter = index.writer(15_000_000).unwrap();

    for (document_id, body, metadata_value) in documents {
        writer
            .add_document(doc!(
                doc_id => (*document_id).to_string(),
                text => (*body).to_string(),
                metadata => metadata_value.to_string(),
            ))
            .unwrap();
    }

    writer.commit().unwrap();
    index
        .reader_builder()
        .try_into()
        .unwrap()
        .searcher()
        .search(
            &tantivy::query::AllQuery,
            &TopDocs::with_limit(documents.len().max(1)),
        )
        .unwrap();
}

fn sample_head_json(version_id: u64) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "manifest_path": "{}",
  "updated_at": 1700000005000
}}"#,
        version_manifest_key(version_id)
    )
}

fn sample_manifest_json(version_id: u64, shard_count: usize) -> String {
    let shards = (0..shard_count)
        .map(|shard_id| {
            format!(
                r#"    {{
      "shard_id": {shard_id},
      "document_count": 2,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_{shard_id}",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_{shard_id}"
    }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");

    format!(
        r#"{{
  "version_id": {version_id},
  "created_at": 1700000000000,
  "embedding_dim": 768,
  "document_count": {},
  "num_shards": {shard_count},
  "shards": [
{shards}
  ]
}}"#,
        shard_count * 2
    )
}

#[test]
fn keyword_searcher_returns_top_k_results_from_single_shard_manifest() {
    let root = temp_fixture_dir("keyword-searcher-single-shard");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 1));
    write_index(
        &root,
        "index/v7/shard_0",
        &[
            ("doc-1", "rust keyword search with tantivy"),
            ("doc-2", "rust keyword search"),
            ("doc-3", "rust"),
        ],
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);

    let results = searcher.search("rust keyword search", 2).unwrap();

    assert_eq!(results.len(), 2);
    assert!(results[0].score >= results[1].score);
    assert_eq!(results[0].doc_id, "doc-2");
    assert_eq!(results[1].doc_id, "doc-1");
    assert!(results.iter().all(|result| result.metadata.is_none()));
}

#[test]
fn keyword_searcher_includes_metadata_when_local_index_stores_it() {
    let root = temp_fixture_dir("keyword-searcher-metadata");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 1));
    write_index_with_metadata(
        &root,
        "index/v7/shard_0",
        &[
            (
                "doc-1",
                "rust keyword search",
                json!({"lang":"rust","published":true}),
            ),
            ("doc-2", "rust", json!({"lang":"go","published":true})),
        ],
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);

    let results = searcher.search("rust", 2).unwrap();

    let metadata_by_doc_id = results
        .into_iter()
        .map(|result| (result.doc_id, result.metadata.unwrap()))
        .collect::<HashMap<_, _>>();

    assert_eq!(metadata_by_doc_id["doc-1"]["lang"], json!("rust"));
    assert_eq!(metadata_by_doc_id["doc-2"]["lang"], json!("go"));
}

#[test]
fn keyword_searcher_deduplicates_duplicate_doc_ids_within_a_shard() {
    let root = temp_fixture_dir("keyword-searcher-duplicate-docids");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 1));
    write_index(
        &root,
        "index/v7/shard_0",
        &[
            (
                "doc-1",
                "tantivyunique tantivyunique tantivyunique tantivyunique",
            ),
            ("doc-1", "tantivyunique tantivyunique tantivyunique"),
            ("doc-2", "tantivyunique"),
            ("doc-3", "rust rust"),
        ],
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);

    let results = searcher.search("tantivyunique", 2).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].doc_id, "doc-1");
    assert_eq!(results[1].doc_id, "doc-2");
    assert_eq!(
        results
            .iter()
            .filter(|result| result.doc_id == "doc-1")
            .count(),
        1
    );
}

#[test]
fn keyword_searcher_rejects_invalid_query_syntax() {
    let root = temp_fixture_dir("keyword-searcher-invalid-query");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 1));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust keyword search with tantivy")],
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search("\"", 3).unwrap_err();

    assert!(error.to_string().contains("invalid Tantivy query"));
}

#[test]
fn keyword_searcher_rejects_multi_shard_manifests() {
    let root = temp_fixture_dir("keyword-searcher-multi-shard");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 2));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust keyword search")],
    );
    write_index(
        &root,
        "index/v7/shard_1",
        &[("doc-2", "rust keyword search")],
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search("rust", 2).unwrap_err();

    assert!(error.to_string().contains("single-shard"));
}

#[test]
fn keyword_searcher_rejects_tantivy_paths_that_escape_artifact_root() {
    let root = temp_fixture_dir("keyword-searcher-path-traversal");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(
        &root,
        &version_manifest_key(7),
        r#"{
  "version_id": 7,
  "created_at": 1700000000000,
  "embedding_dim": 768,
  "document_count": 2,
  "num_shards": 1,
  "shards": [
    {
      "shard_id": 0,
      "document_count": 2,
      "lance_path": "s3://bucket/lance/v7/shard_0",
      "tantivy_path": "s3://bucket/index/v7/../../escape"
    }
  ]
}"#,
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search("rust", 2).unwrap_err();

    assert!(error.to_string().contains("artifact path"));
    assert!(error.to_string().contains(".."));
}

#[test]
fn keyword_searcher_rejects_tantivy_symlink_escapes_from_artifact_root() {
    let root = temp_fixture_dir("keyword-searcher-symlink-escape");
    let outside = temp_fixture_dir("keyword-searcher-symlink-escape-outside");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 1));
    write_index(
        &outside,
        "escape-index",
        &[("doc-1", "rust keyword search")],
    );
    fs::create_dir_all(root.join("index/v7")).unwrap();
    symlink(outside.join("escape-index"), root.join("index/v7/shard_0")).unwrap();

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search("rust", 2).unwrap_err();

    assert!(error.to_string().contains("escapes artifact root"));
}

#[test]
fn keyword_searcher_search_request_rejects_unsupported_filters() {
    let root = temp_fixture_dir("keyword-searcher-unsupported-filters");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 1));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust keyword search")],
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher
        .search_request(&SearchRequest {
            query: "rust".into(),
            top_k: 1,
            filters: Some(HashMap::from([(
                "tenant".into(),
                FilterValue::StringEquals("acme".into()),
            )])),
            include_metadata: false,
        })
        .unwrap_err();

    assert!(error.to_string().contains("filters"));
    assert!(error.to_string().contains("unsupported"));
}

#[test]
fn keyword_searcher_search_request_rejects_include_metadata() {
    let root = temp_fixture_dir("keyword-searcher-include-metadata");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 1));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust keyword search")],
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher
        .search_request(&SearchRequest {
            query: "rust".into(),
            top_k: 1,
            filters: None,
            include_metadata: true,
        })
        .unwrap_err();

    assert!(error.to_string().contains("include_metadata"));
    assert!(error.to_string().contains("unsupported"));
}

#[test]
fn keyword_searcher_search_request_delegates_to_basic_search() {
    let root = temp_fixture_dir("keyword-searcher-search-request-delegates");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7, 1));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust keyword search"), ("doc-2", "rust")],
    );

    let searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let results = searcher
        .search_request(&SearchRequest {
            query: "rust keyword search".into(),
            top_k: 1,
            filters: None,
            include_metadata: false,
        })
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].doc_id, "doc-1");
}
