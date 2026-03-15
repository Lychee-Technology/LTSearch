use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::storage::{
    version_manifest_key, ActiveManifest, LocalManifestStore, ManifestStore, ManifestStoreError,
    INDEX_HEAD_KEY,
};

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

fn sample_manifest_json(version_id: u64) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "created_at": 1700000000000,
  "embedding_dim": 768,
  "document_count": 5,
  "num_shards": 2,
  "shards": [
    {{
      "shard_id": 0,
      "document_count": 2,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_0",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_0"
    }},
    {{
      "shard_id": 1,
      "document_count": 3,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_1",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_1"
    }}
  ]
}}"#
    )
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

#[test]
fn version_manifest_key_uses_canonical_layout() {
    assert_eq!(INDEX_HEAD_KEY, "index/_head");
    assert_eq!(version_manifest_key(42), "index/versions/42/manifest.json");
}

#[test]
fn local_store_loads_active_manifest_from_head() {
    let root = temp_fixture_dir("loads-active-manifest");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));

    let store = LocalManifestStore::new(&root);

    let active = store.load_active_manifest().unwrap();

    assert_eq!(
        active,
        ActiveManifest {
            head: store.load_head().unwrap(),
            manifest: serde_json::from_str(&sample_manifest_json(7)).unwrap(),
        }
    );
    assert_eq!(store.load_active_version().unwrap(), 7);
}

#[test]
fn local_store_errors_when_head_is_missing() {
    let root = temp_fixture_dir("missing-head");
    let store = LocalManifestStore::new(&root);

    let error = store.load_head().unwrap_err();

    assert!(matches!(
        error,
        ManifestStoreError::MissingHead { ref path }
        if path == &root.join(INDEX_HEAD_KEY)
    ));
}

#[test]
fn local_store_errors_when_head_manifest_path_is_not_canonical() {
    let root = temp_fixture_dir("noncanonical-head-path");
    write_fixture(
        &root,
        INDEX_HEAD_KEY,
        r#"{
  "version_id": 7,
  "manifest_path": "index/versions/99/manifest.json",
  "updated_at": 1700000005000
}"#,
    );

    let store = LocalManifestStore::new(&root);

    let error = store.load_head().unwrap_err();

    assert!(matches!(
        error,
        ManifestStoreError::InvalidHead { message }
        if message.contains("manifest_path") && message.contains("version_id")
    ));
}

#[test]
fn local_store_errors_when_head_updated_at_is_implausibly_small() {
    let root = temp_fixture_dir("implausible-head-updated-at");
    write_fixture(
        &root,
        INDEX_HEAD_KEY,
        r#"{
  "version_id": 7,
  "manifest_path": "index/versions/7/manifest.json",
  "updated_at": 1700000000
}"#,
    );

    let store = LocalManifestStore::new(&root);

    let error = store.load_head().unwrap_err();

    assert!(matches!(
        error,
        ManifestStoreError::InvalidHead { message }
        if message.contains("updated_at") && message.contains("epoch millis")
    ));
}

#[test]
fn local_store_errors_when_manifest_file_is_missing() {
    let root = temp_fixture_dir("missing-manifest");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    let store = LocalManifestStore::new(&root);

    let error = store.load_active_manifest().unwrap_err();

    assert!(matches!(
        error,
        ManifestStoreError::MissingManifest { ref path }
        if path == &root.join(version_manifest_key(7))
    ));
}

#[test]
fn local_store_errors_when_manifest_fails_validation() {
    let root = temp_fixture_dir("invalid-manifest");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(
        &root,
        &version_manifest_key(7),
        r#"{
  "version_id": 7,
  "created_at": 1700000000000,
  "embedding_dim": 768,
  "document_count": 1,
  "num_shards": 1,
  "shards": [
    {
      "shard_id": 0,
      "document_count": 2,
      "lance_path": "s3://bucket/lance/v7/shard_0",
      "tantivy_path": "s3://bucket/index/v7/shard_0"
    }
  ]
}"#,
    );

    let store = LocalManifestStore::new(&root);
    let error = store.load_active_manifest().unwrap_err();

    assert!(matches!(
        error,
        ManifestStoreError::InvalidManifest { message, .. }
        if message.contains("document_count")
    ));
}

#[test]
fn local_store_errors_when_manifest_version_does_not_match_head() {
    let root = temp_fixture_dir("manifest-version-mismatch");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(8));

    let store = LocalManifestStore::new(&root);
    let error = store.load_active_manifest().unwrap_err();

    assert!(matches!(
        error,
        ManifestStoreError::InvalidManifest { message, .. }
        if message.contains("version_id") && message.contains("_head")
    ));
}
