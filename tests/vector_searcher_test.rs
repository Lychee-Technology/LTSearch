use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::error::{SearchError, ValidationError};
use ltsearch::models::CacheStats;
use ltsearch::query::VectorSearcher;
use ltsearch::storage::{version_manifest_key, LocalManifestStore, INDEX_HEAD_KEY};
use serde_json::json;

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

fn write_lance_fixture(root: &Path, relative_path: &str, rows: &[serde_json::Value]) {
    let path = root.join(relative_path).join("rows.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_string_pretty(rows).unwrap()).unwrap();
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

fn sample_manifest_json(version_id: u64) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "created_at": 1700000000000,
  "embedding_dim": 3,
  "document_count": 5,
  "num_shards": 2,
  "shards": [
    {{
      "shard_id": 0,
      "document_count": 3,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_0",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_0"
    }},
    {{
      "shard_id": 1,
      "document_count": 2,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_1",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_1"
    }}
  ]
}}"#
    )
}

fn sample_manifest_json_with_embedding_dim(version_id: u64, embedding_dim: usize) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "created_at": 1700000000000,
  "embedding_dim": {embedding_dim},
  "document_count": 1,
  "num_shards": 2,
  "shards": [
    {{
      "shard_id": 0,
      "document_count": 1,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_0",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_0"
    }},
    {{
      "shard_id": 1,
      "document_count": 0,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_1",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_1"
    }}
  ]
}}"#
    )
}

#[test]
fn vector_searcher_loads_active_manifest_and_returns_top_k_results() {
    let root = temp_fixture_dir("vector-searcher-top-k");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "doc-1", "text": "alpha", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-2", "text": "beta", "embedding": [0.8, 0.6, 0.0]}),
            json!({"doc_id": "doc-3", "text": "gamma", "embedding": [0.0, 1.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-5", "text": "epsilon", "embedding": [0.5, 0.5, 0.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);

    let results = searcher.search(&[1.0, 0.0, 0.0], 3).unwrap();

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].doc_id, "doc-1");
    assert_eq!(results[1].doc_id, "doc-2");
    assert_eq!(results[2].doc_id, "doc-5");
    assert!(results[0].score >= results[1].score);
    assert!(results[1].score >= results[2].score);
    assert!(results.iter().all(|result| result.metadata.is_none()));
}

#[test]
fn vector_searcher_includes_metadata_when_local_rows_have_it() {
    let root = temp_fixture_dir("vector-searcher-metadata");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "doc-1", "text": "alpha", "embedding": [1.0, 0.0, 0.0], "metadata": {"lang": "rust", "published": true}}),
            json!({"doc_id": "doc-2", "text": "beta", "embedding": [0.8, 0.6, 0.0], "metadata": {"lang": "go", "published": true}}),
            json!({"doc_id": "doc-3", "text": "gamma", "embedding": [0.0, 1.0, 0.0]}),
        ],
    );
    write_lance_fixture(&root, "lance/v7/shard_1", &[]);

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);

    let results = searcher.search(&[1.0, 0.0, 0.0], 3).unwrap();

    assert_eq!(results[0].metadata.as_ref().unwrap()["lang"], json!("rust"));
    assert_eq!(results[1].metadata.as_ref().unwrap()["lang"], json!("go"));
    assert!(results[2].metadata.is_none());
}

#[test]
fn vector_searcher_rejects_query_embeddings_with_wrong_dimension() {
    let root = temp_fixture_dir("vector-searcher-dim-mismatch");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[json!({"doc_id": "doc-1", "text": "alpha", "embedding": [1.0, 0.0, 0.0]})],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[json!({"doc_id": "doc-2", "text": "beta", "embedding": [0.0, 1.0, 0.0]})],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search(&[1.0, 0.0], 2).unwrap_err();

    assert!(matches!(
        error,
        SearchError::Validation(ValidationError::InvalidValue {
            field: "query_embedding"
        })
    ));
    assert_eq!(error.to_string(), "query_embedding has an invalid value");
}

#[test]
fn vector_searcher_accepts_query_embeddings_larger_than_previous_arbitrary_cap_when_manifest_matches(
) {
    let root = temp_fixture_dir("vector-searcher-large-embedding-dim");
    let embedding_dim = 16_385;
    let mut embedding = vec![0.0_f32; embedding_dim];
    embedding[0] = 1.0;

    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(
        &root,
        &version_manifest_key(7),
        &sample_manifest_json_with_embedding_dim(7, embedding_dim),
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[json!({"doc_id": "doc-1", "text": "alpha", "embedding": embedding})],
    );
    write_lance_fixture(&root, "lance/v7/shard_1", &[]);

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let query_embedding = vec![0.0_f32; embedding_dim]
        .into_iter()
        .enumerate()
        .map(|(index, value)| if index == 0 { 1.0 } else { value })
        .collect::<Vec<_>>();

    let results = searcher.search(&query_embedding, 1).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].doc_id, "doc-1");
}

#[test]
fn vector_searcher_deduplicates_doc_ids_and_sorts_ties_stably() {
    let root = temp_fixture_dir("vector-searcher-dedup-and-sort");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "doc-z", "text": "older", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-a", "text": "tie-a", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-b", "text": "tie-b", "embedding": [1.0, 0.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-z", "text": "newer and worse", "embedding": [0.4, 0.0, 0.0]}),
            json!({"doc_id": "doc-c", "text": "tail", "embedding": [0.2, 0.0, 0.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);

    let results = searcher.search(&[1.0, 0.0, 0.0], 4).unwrap();

    assert_eq!(results.len(), 4);
    assert_eq!(results[0].doc_id, "doc-a");
    assert_eq!(results[1].doc_id, "doc-b");
    assert_eq!(results[2].doc_id, "doc-z");
    assert_eq!(results[3].doc_id, "doc-c");
    assert_eq!(
        results
            .iter()
            .filter(|result| result.doc_id == "doc-z")
            .count(),
        1
    );
    assert_eq!(results[2].text, "older");
}

#[test]
fn vector_searcher_reads_explicit_local_rows_json_shim_fixture() {
    let root = temp_fixture_dir("vector-searcher-local-rows-json-shim");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_fixture(
        &root,
        "lance/v7/shard_0/rows.json",
        r#"[
  {"doc_id": "doc-1", "text": "alpha", "embedding": [1.0, 0.0, 0.0]},
  {"doc_id": "doc-2", "text": "beta", "embedding": [0.2, 0.0, 0.0]},
  {"doc_id": "doc-3", "text": "gamma", "embedding": [0.1, 0.0, 0.0]}
]"#,
    );
    write_fixture(
        &root,
        "lance/v7/shard_1/rows.json",
        r#"[
  {"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 1.0, 0.0]},
  {"doc_id": "doc-5", "text": "epsilon", "embedding": [0.0, 0.0, 1.0]}
]"#,
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);

    let results = searcher.search(&[1.0, 0.0, 0.0], 2).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].doc_id, "doc-1");
    assert_eq!(results[1].doc_id, "doc-2");
}

#[test]
fn vector_searcher_rejects_non_finite_score_from_local_shim_rows() {
    let root = temp_fixture_dir("vector-searcher-non-finite-score");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "doc-1", "text": "alpha", "embedding": [1.0e38, 0.0, 0.0]}),
            json!({"doc_id": "doc-2", "text": "beta", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-3", "text": "gamma", "embedding": [0.0, 1.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-5", "text": "epsilon", "embedding": [0.5, 0.5, 0.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search(&[1.0e10, 0.0, 0.0], 3).unwrap_err();

    assert!(error.to_string().contains("non-finite"));
    assert!(error.to_string().contains("local"));
    assert!(error.to_string().contains("rows.json"));
}

#[test]
fn vector_searcher_rejects_local_rows_symlink_escapes_from_artifact_root() {
    let root = temp_fixture_dir("vector-searcher-symlink-escape");
    let outside = temp_fixture_dir("vector-searcher-symlink-escape-outside");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_fixture(
        &outside,
        "external-shard/rows.json",
        r#"[
  {"doc_id": "doc-1", "text": "outside", "embedding": [1.0, 0.0, 0.0]}
]"#,
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-2", "text": "inside", "embedding": [0.0, 1.0, 0.0]}),
            json!({"doc_id": "doc-3", "text": "inside-2", "embedding": [0.0, 0.0, 1.0]}),
        ],
    );

    let symlink_path = root.join("lance/v7/shard_0");
    fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
    symlink(outside.join("external-shard"), &symlink_path).unwrap();

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search(&[1.0, 0.0, 0.0], 1).unwrap_err();

    assert!(error.to_string().contains("escape"));
    assert!(error.to_string().contains("artifact root"));
}

#[test]
fn vector_searcher_rejects_rows_json_symlink_escapes_from_inside_shard_directory() {
    let root = temp_fixture_dir("vector-searcher-rows-json-symlink-escape");
    let outside = temp_fixture_dir("vector-searcher-rows-json-symlink-escape-outside");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_fixture(
        &outside,
        "outside-rows.json",
        r#"[
  {"doc_id": "doc-1", "text": "outside", "embedding": [1.0, 0.0, 0.0]}
]"#,
    );
    fs::create_dir_all(root.join("lance/v7/shard_0")).unwrap();
    symlink(
        outside.join("outside-rows.json"),
        root.join("lance/v7/shard_0/rows.json"),
    )
    .unwrap();
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-2", "text": "inside", "embedding": [0.0, 1.0, 0.0]}),
            json!({"doc_id": "doc-3", "text": "inside-2", "embedding": [0.0, 0.0, 1.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search(&[1.0, 0.0, 0.0], 1).unwrap_err();

    assert!(error.to_string().contains("rows.json"));
    assert!(error.to_string().contains("escape"));
    assert!(error.to_string().contains("artifact root"));
}

#[test]
fn vector_searcher_rejects_rows_with_empty_doc_ids() {
    let root = temp_fixture_dir("vector-searcher-empty-doc-id");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "", "text": "broken", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-2", "text": "beta", "embedding": [0.8, 0.6, 0.0]}),
            json!({"doc_id": "doc-3", "text": "gamma", "embedding": [0.0, 1.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-5", "text": "epsilon", "embedding": [0.5, 0.5, 0.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search(&[1.0, 0.0, 0.0], 3).unwrap_err();

    assert!(error.to_string().contains("doc_id"));
    assert!(error.to_string().contains("required"));
}

#[test]
fn vector_searcher_tracks_local_rows_shim_cache_stats() {
    let root = temp_fixture_dir("vector-searcher-cache-stats");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "doc-1", "text": "alpha", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-2", "text": "beta", "embedding": [0.8, 0.6, 0.0]}),
            json!({"doc_id": "doc-3", "text": "gamma", "embedding": [0.0, 1.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-5", "text": "epsilon", "embedding": [0.5, 0.5, 0.0]}),
        ],
    );

    let shard_0_bytes = fs::metadata(root.join("lance/v7/shard_0/rows.json"))
        .unwrap()
        .len();
    let shard_1_bytes = fs::metadata(root.join("lance/v7/shard_1/rows.json"))
        .unwrap()
        .len();
    let total_bytes = shard_0_bytes + shard_1_bytes;

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);

    assert_eq!(
        searcher.cache_stats(),
        CacheStats {
            hit_count: 0,
            miss_count: 0,
            current_version: None,
            bytes_used: 0,
        }
    );

    searcher.search(&[1.0, 0.0, 0.0], 2).unwrap();

    assert_eq!(
        searcher.cache_stats(),
        CacheStats {
            hit_count: 0,
            miss_count: 2,
            current_version: Some(7),
            bytes_used: total_bytes,
        }
    );

    searcher.search(&[1.0, 0.0, 0.0], 2).unwrap();

    assert_eq!(
        searcher.cache_stats(),
        CacheStats {
            hit_count: 2,
            miss_count: 2,
            current_version: Some(7),
            bytes_used: total_bytes,
        }
    );
}

#[test]
fn vector_searcher_does_not_set_current_version_before_first_successful_shard_load() {
    let root = temp_fixture_dir("vector-searcher-cache-version-delayed");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-5", "text": "epsilon", "embedding": [0.5, 0.5, 0.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let error = searcher.search(&[1.0, 0.0, 0.0], 2).unwrap_err();

    assert!(error.to_string().contains("rows.json"));
    assert_eq!(
        searcher.cache_stats(),
        CacheStats {
            hit_count: 0,
            miss_count: 0,
            current_version: None,
            bytes_used: 0,
        }
    );
}

#[test]
fn vector_searcher_resets_cache_stats_when_active_manifest_version_changes() {
    let root = temp_fixture_dir("vector-searcher-cache-version-reset");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "doc-1", "text": "alpha", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-2", "text": "beta", "embedding": [0.8, 0.6, 0.0]}),
            json!({"doc_id": "doc-3", "text": "gamma", "embedding": [0.0, 1.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-5", "text": "epsilon", "embedding": [0.5, 0.5, 0.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    searcher.search(&[1.0, 0.0, 0.0], 2).unwrap();
    searcher.search(&[1.0, 0.0, 0.0], 2).unwrap();

    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(8));
    write_fixture(&root, &version_manifest_key(8), &sample_manifest_json(8));
    write_lance_fixture(
        &root,
        "lance/v8/shard_0",
        &[
            json!({"doc_id": "doc-10", "text": "theta theta", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-11", "text": "iota", "embedding": [0.7, 0.3, 0.0]}),
            json!({"doc_id": "doc-12", "text": "kappa", "embedding": [0.0, 1.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v8/shard_1",
        &[
            json!({"doc_id": "doc-13", "text": "lambda lambda", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-14", "text": "mu mu mu", "embedding": [0.4, 0.4, 0.2]}),
        ],
    );

    let v8_total_bytes = fs::metadata(root.join("lance/v8/shard_0/rows.json"))
        .unwrap()
        .len()
        + fs::metadata(root.join("lance/v8/shard_1/rows.json"))
            .unwrap()
            .len();

    searcher.search(&[1.0, 0.0, 0.0], 2).unwrap();

    assert_eq!(
        searcher.cache_stats(),
        CacheStats {
            hit_count: 0,
            miss_count: 2,
            current_version: Some(8),
            bytes_used: v8_total_bytes,
        }
    );
}

#[test]
fn vector_searcher_keeps_cache_stats_empty_when_new_version_rows_json_parse_fails() {
    let root = temp_fixture_dir("vector-searcher-cache-parse-failure");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "doc-1", "text": "alpha", "embedding": [1.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-2", "text": "beta", "embedding": [0.8, 0.6, 0.0]}),
            json!({"doc_id": "doc-3", "text": "gamma", "embedding": [0.0, 1.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-5", "text": "epsilon", "embedding": [0.5, 0.5, 0.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    searcher.search(&[1.0, 0.0, 0.0], 2).unwrap();

    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(8));
    write_fixture(&root, &version_manifest_key(8), &sample_manifest_json(8));
    write_fixture(&root, "lance/v8/shard_0/rows.json", "not valid json");

    let error = searcher.search(&[1.0, 0.0, 0.0], 2).unwrap_err();

    assert!(error.to_string().contains("parse"));
    assert!(error.to_string().contains("rows.json"));
    assert_eq!(
        searcher.cache_stats(),
        CacheStats {
            hit_count: 0,
            miss_count: 0,
            current_version: None,
            bytes_used: 0,
        }
    );
}

#[test]
fn vector_searcher_clamps_scores_to_documented_unit_interval() {
    let root = temp_fixture_dir("vector-searcher-score-clamp");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            json!({"doc_id": "doc-1", "text": "alpha", "embedding": [5.0, 0.0, 0.0]}),
            json!({"doc_id": "doc-2", "text": "beta", "embedding": [0.8, 0.6, 0.0]}),
            json!({"doc_id": "doc-3", "text": "gamma", "embedding": [-2.0, 0.0, 0.0]}),
        ],
    );
    write_lance_fixture(
        &root,
        "lance/v7/shard_1",
        &[
            json!({"doc_id": "doc-4", "text": "delta", "embedding": [0.0, 0.0, 1.0]}),
            json!({"doc_id": "doc-5", "text": "epsilon", "embedding": [0.5, 0.5, 0.0]}),
        ],
    );

    let searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let results = searcher.search(&[1.0, 0.0, 0.0], 3).unwrap();

    assert_eq!(results[0].doc_id, "doc-1");
    assert_eq!(results[0].score, 1.0);
    assert!(results
        .iter()
        .all(|result| (0.0..=1.0).contains(&result.score)));
}
