use std::collections::HashMap;
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ltsearch::error::PublishError;
use ltsearch::indexing::{IndexPublisher, PublishRequest, PublishStorage, RollbackRequest};
use ltsearch::models::{IndexManifest, ShardManifest};
use ltsearch::storage::{version_manifest_key, INDEX_HEAD_KEY};

#[derive(Clone, Debug, PartialEq, Eq)]
enum StoredObject {
    File(Vec<u8>),
    Directory,
}

#[derive(Clone, Debug, Default)]
struct RecordingPublishStorage {
    objects: Arc<Mutex<HashMap<String, StoredObject>>>,
    calls: Arc<Mutex<Vec<String>>>,
    last_expected: Arc<Mutex<Option<Option<Vec<u8>>>>>,
    compare_and_swap_conflict: Arc<Mutex<Option<Vec<u8>>>>,
}

impl RecordingPublishStorage {
    fn seed_file(&self, key: &str, bytes: Vec<u8>) {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), StoredObject::File(bytes));
    }

    fn seed_directory(&self, key: &str) {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), StoredObject::Directory);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn file_bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(key).and_then(|object| {
            if let StoredObject::File(bytes) = object {
                Some(bytes.clone())
            } else {
                None
            }
        })
    }

    fn last_expected(&self) -> Option<Option<Vec<u8>>> {
        self.last_expected.lock().unwrap().clone()
    }

    fn conflict_on_compare_and_swap(&self, bytes: Vec<u8>) {
        *self.compare_and_swap_conflict.lock().unwrap() = Some(bytes);
    }
}

#[async_trait]
impl PublishStorage for RecordingPublishStorage {
    async fn upload_directory(&self, key: &str, source: &Path) -> Result<(), PublishError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("upload_directory:{key}"));

        if !source.is_dir() {
            return Err(PublishError::Operation {
                message: format!("missing source directory {}", source.display()),
            });
        }

        self.seed_directory(key);
        Ok(())
    }

    async fn upload_file(&self, key: &str, source: &Path) -> Result<(), PublishError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("upload_file:{key}"));

        let bytes = fs::read(source).map_err(|source_error| PublishError::Operation {
            message: format!("failed to read {}: {source_error}", source.display()),
        })?;
        self.seed_file(key, bytes);
        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Option<Vec<u8>>, PublishError> {
        self.calls.lock().unwrap().push(format!("read:{key}"));
        Ok(self.file_bytes(key))
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("compare_and_swap:{key}"));
        *self.last_expected.lock().unwrap() = Some(expected.map(|bytes| bytes.to_vec()));

        if let Some(conflict_bytes) = self.compare_and_swap_conflict.lock().unwrap().take() {
            self.seed_file(key, conflict_bytes);
            return Ok(false);
        }

        let current = self.file_bytes(key);
        if current.as_deref() != expected {
            return Ok(false);
        }

        self.seed_file(key, new_value.to_vec());
        Ok(true)
    }
}

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

fn write_fixture(root: &Path, relative_path: &str, contents: &[u8]) {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn create_source_build(root: &Path, manifest: &IndexManifest) {
    for shard in &manifest.shards {
        let lance_key = s3_key(&shard.lance_path);
        let tantivy_key = s3_key(&shard.tantivy_path);
        fs::create_dir_all(root.join(&lance_key)).unwrap();
        fs::create_dir_all(root.join(&tantivy_key)).unwrap();
        write_fixture(root, &format!("{lance_key}/data.bin"), b"lance");
        write_fixture(root, &format!("{tantivy_key}/meta.json"), b"tantivy");
    }

    write_fixture(
        root,
        &version_manifest_key(manifest.version_id),
        serde_json::to_string_pretty(manifest).unwrap().as_bytes(),
    );
}

fn sample_manifest(version_id: u64) -> IndexManifest {
    IndexManifest {
        version_id,
        created_at: 1_700_000_000_000,
        embedding_dim: 3,
        document_count: 2,
        num_shards: 1,
        shards: vec![ShardManifest {
            shard_id: 0,
            document_count: 2,
            lance_path: format!("s3://local-artifacts/lance/v{version_id}/shard_0"),
            tantivy_path: format!("s3://local-artifacts/index/v{version_id}/shard_0"),
        }],
    }
}

fn head_json(version_id: u64, updated_at: i64) -> Vec<u8> {
    format!(
        "{{\n  \"version_id\": {version_id},\n  \"manifest_path\": \"{}\",\n  \"updated_at\": {updated_at}\n}}",
        version_manifest_key(version_id)
    )
    .into_bytes()
}

fn manifest_json(manifest: &IndexManifest) -> Vec<u8> {
    serde_json::to_vec_pretty(manifest).unwrap()
}

fn s3_key(value: &str) -> String {
    value
        .split_once("//")
        .unwrap()
        .1
        .split_once('/')
        .unwrap()
        .1
        .to_string()
}

#[tokio::test]
async fn publisher_uploads_artifacts_and_manifest_before_updating_head() {
    let build_root = temp_fixture_dir("publisher-upload-order");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    publisher
        .publish(&PublishRequest {
            manifest: manifest.clone(),
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap();

    let calls = storage.calls();
    let compare_index = calls
        .iter()
        .position(|call| call == &format!("compare_and_swap:{INDEX_HEAD_KEY}"))
        .unwrap();

    for required in [
        "upload_directory:lance/v9/shard_0",
        "upload_directory:index/v9/shard_0",
        "upload_file:index/versions/9/manifest.json",
    ] {
        let index = calls.iter().position(|call| call == required).unwrap();
        assert!(
            index < compare_index,
            "{required} should happen before _head update"
        );
    }
}

#[tokio::test]
async fn publisher_conditionally_updates_head_with_current_head_contents() {
    let build_root = temp_fixture_dir("publisher-conditional-head");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let storage = RecordingPublishStorage::default();
    let current_head = head_json(8, 1_700_000_000_100);
    storage.seed_file(INDEX_HEAD_KEY, current_head.clone());

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    publisher
        .publish(&PublishRequest {
            manifest: manifest.clone(),
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap();

    assert_eq!(storage.last_expected(), Some(Some(current_head)));
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(9, 1_700_000_000_500)
    );
}

#[tokio::test]
async fn publisher_rejects_publish_race_without_corrupting_active_version() {
    let build_root = temp_fixture_dir("publisher-race-rejection");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));
    storage.conflict_on_compare_and_swap(head_json(10, 1_700_000_000_600));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("conflict"));
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(10, 1_700_000_000_600)
    );
}

#[tokio::test]
async fn publisher_preserves_previous_version_artifacts_on_success() {
    let build_root = temp_fixture_dir("publisher-preserves-previous-version");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));
    storage.seed_file(&version_manifest_key(8), b"previous manifest".to_vec());
    storage.seed_directory("lance/v8/shard_0");
    storage.seed_directory("index/v8/shard_0");

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let published = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap();

    assert_eq!(published.previous_version_id, Some(8));
    assert_eq!(published.activated_version_id, 9);
    assert_eq!(
        storage.file_bytes(&version_manifest_key(8)).unwrap(),
        b"previous manifest".to_vec()
    );
    assert_eq!(
        storage.objects.lock().unwrap().get("lance/v8/shard_0"),
        Some(&StoredObject::Directory)
    );
    assert_eq!(
        storage.objects.lock().unwrap().get("index/v8/shard_0"),
        Some(&StoredObject::Directory)
    );
}

#[tokio::test]
async fn rollback_restores_previous_active_version() {
    let build_root = temp_fixture_dir("publisher-rollback-restores-previous-active-version");
    let current_manifest = sample_manifest(9);
    create_source_build(&build_root, &current_manifest);

    let previous_manifest = sample_manifest(8);
    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(9, 1_700_000_000_500));
    storage.seed_file(
        &version_manifest_key(previous_manifest.version_id),
        manifest_json(&previous_manifest),
    );
    storage.seed_file(
        &version_manifest_key(current_manifest.version_id),
        manifest_json(&current_manifest),
    );

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let rolled_back = publisher
        .rollback(&RollbackRequest {
            target_version_id: previous_manifest.version_id,
            expected_current_version: Some(current_manifest.version_id),
            updated_at: 1_700_000_000_900,
        })
        .await
        .unwrap();

    assert_eq!(rolled_back.previous_version_id, Some(9));
    assert_eq!(rolled_back.activated_version_id, 8);
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(8, 1_700_000_000_900)
    );
    assert_eq!(
        storage.calls(),
        vec![
            format!("read:{INDEX_HEAD_KEY}"),
            format!("read:{}", version_manifest_key(8)),
            format!("compare_and_swap:{INDEX_HEAD_KEY}"),
        ]
    );
}

#[tokio::test]
async fn rollback_rejects_stale_expected_current_version_before_target_lookup() {
    let build_root = temp_fixture_dir("publisher-rollback-stale-expected-current-version");
    let current_manifest = sample_manifest(9);
    create_source_build(&build_root, &current_manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(9, 1_700_000_000_500));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .rollback(&RollbackRequest {
            target_version_id: 8,
            expected_current_version: Some(7),
            updated_at: 1_700_000_000_900,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("conflict"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(9, 1_700_000_000_500)
    );
}

#[tokio::test]
async fn rollback_rejects_compare_and_swap_conflict_without_corrupting_active_version() {
    let build_root = temp_fixture_dir("publisher-rollback-compare-and-swap-conflict");
    let current_manifest = sample_manifest(9);
    create_source_build(&build_root, &current_manifest);

    let previous_manifest = sample_manifest(8);
    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(9, 1_700_000_000_500));
    storage.seed_file(
        &version_manifest_key(previous_manifest.version_id),
        manifest_json(&previous_manifest),
    );
    storage.conflict_on_compare_and_swap(head_json(10, 1_700_000_001_000));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .rollback(&RollbackRequest {
            target_version_id: previous_manifest.version_id,
            expected_current_version: Some(current_manifest.version_id),
            updated_at: 1_700_000_000_900,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("conflict"));
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(10, 1_700_000_001_000)
    );
}

#[tokio::test]
async fn rollback_rejects_missing_target_manifest_before_head_update() {
    let build_root = temp_fixture_dir("publisher-rollback-missing-target-manifest");
    let current_manifest = sample_manifest(9);
    create_source_build(&build_root, &current_manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(9, 1_700_000_000_500));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .rollback(&RollbackRequest {
            target_version_id: 8,
            expected_current_version: Some(current_manifest.version_id),
            updated_at: 1_700_000_000_900,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("missing"));
    assert_eq!(
        storage.calls(),
        vec![
            format!("read:{INDEX_HEAD_KEY}"),
            format!("read:{}", version_manifest_key(8)),
        ]
    );
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(9, 1_700_000_000_500)
    );
}

#[tokio::test]
async fn rollback_rejects_invalid_target_manifest_before_head_update() {
    let build_root = temp_fixture_dir("publisher-rollback-invalid-target-manifest");
    let current_manifest = sample_manifest(9);
    create_source_build(&build_root, &current_manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(9, 1_700_000_000_500));
    storage.seed_file(&version_manifest_key(8), br#"{ not valid json }"#.to_vec());

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .rollback(&RollbackRequest {
            target_version_id: 8,
            expected_current_version: Some(current_manifest.version_id),
            updated_at: 1_700_000_000_900,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("manifest"));
    assert_eq!(
        storage.calls(),
        vec![
            format!("read:{INDEX_HEAD_KEY}"),
            format!("read:{}", version_manifest_key(8)),
        ]
    );
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(9, 1_700_000_000_500)
    );
}

#[tokio::test]
async fn rollback_rejects_target_manifest_with_mismatched_version_id_before_head_update() {
    let build_root = temp_fixture_dir("publisher-rollback-mismatched-target-version-id");
    let current_manifest = sample_manifest(9);
    create_source_build(&build_root, &current_manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(9, 1_700_000_000_500));
    storage.seed_file(&version_manifest_key(8), manifest_json(&sample_manifest(7)));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .rollback(&RollbackRequest {
            target_version_id: 8,
            expected_current_version: Some(current_manifest.version_id),
            updated_at: 1_700_000_000_900,
        })
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("rollback target version")
            || error.to_string().contains("does not match")
    );
    assert_eq!(
        storage.calls(),
        vec![
            format!("read:{INDEX_HEAD_KEY}"),
            format!("read:{}", version_manifest_key(8)),
        ]
    );
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(9, 1_700_000_000_500)
    );
}

#[tokio::test]
async fn publisher_rejects_zero_version_before_upload_or_head_update() {
    let build_root = temp_fixture_dir("publisher-zero-version");
    let manifest = sample_manifest(0);
    create_source_build(&build_root, &manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("version_id"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(8, 1_700_000_000_100)
    );
}

#[tokio::test]
async fn publisher_rejects_path_escaping_artifact_keys_before_upload() {
    let build_root = temp_fixture_dir("publisher-path-escape");
    let mut manifest = sample_manifest(9);
    manifest.shards[0].lance_path = "s3://local-artifacts/../outside".into();
    create_source_build(&build_root, &sample_manifest(9));

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("path") || error.to_string().contains("artifact"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_rejects_absolute_style_artifact_keys_before_upload() {
    let build_root = temp_fixture_dir("publisher-absolute-style-path");
    let mut manifest = sample_manifest(9);
    manifest.shards[0].tantivy_path = "s3://local-artifacts//absolute-style".into();
    create_source_build(&build_root, &sample_manifest(9));

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("path") || error.to_string().contains("artifact"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_rejects_stale_manifest_file_before_upload() {
    let build_root = temp_fixture_dir("publisher-stale-manifest-file");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);
    write_fixture(
        &build_root,
        &version_manifest_key(9),
        serde_json::to_string_pretty(&sample_manifest(10))
            .unwrap()
            .as_bytes(),
    );

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("manifest"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_rejects_invalid_current_head_before_upload() {
    let build_root = temp_fixture_dir("publisher-invalid-current-head");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(
        INDEX_HEAD_KEY,
        br#"{
  "version_id": 0,
  "manifest_path": "index/versions/99/manifest.json",
  "updated_at": 1700000000100
}"#
        .to_vec(),
    );

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("version_id") || error.to_string().contains("manifest_path")
    );
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_rejects_symlinked_shard_path_that_escapes_artifact_root() {
    let build_root = temp_fixture_dir("publisher-symlinked-shard-escape");
    let outside_root = temp_fixture_dir("publisher-symlinked-shard-outside");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let shard_key = s3_key(&manifest.shards[0].lance_path);
    fs::remove_dir_all(build_root.join(&shard_key)).unwrap();
    fs::create_dir_all(&outside_root).unwrap();
    unix_fs::symlink(&outside_root, build_root.join(&shard_key)).unwrap();

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("artifact") || error.to_string().contains("escape"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_rejects_symlinked_manifest_path_that_escapes_artifact_root() {
    let build_root = temp_fixture_dir("publisher-symlinked-manifest-escape");
    let outside_root = temp_fixture_dir("publisher-symlinked-manifest-outside");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let manifest_key = version_manifest_key(manifest.version_id);
    let outside_manifest = outside_root.join("outside-manifest.json");
    fs::write(
        &outside_manifest,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::remove_file(build_root.join(&manifest_key)).unwrap();
    unix_fs::symlink(&outside_manifest, build_root.join(&manifest_key)).unwrap();

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("artifact") || error.to_string().contains("escape"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_rejects_mismatched_manifest_path_in_current_head_before_upload() {
    let build_root = temp_fixture_dir("publisher-invalid-current-head-manifest-path");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let storage = RecordingPublishStorage::default();
    storage.seed_file(
        INDEX_HEAD_KEY,
        br#"{
  "version_id": 8,
  "manifest_path": "index/versions/99/manifest.json",
  "updated_at": 1700000000100
}"#
        .to_vec(),
    );

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("manifest_path"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_rejects_nested_symlink_escape_inside_shard_directory() {
    let build_root = temp_fixture_dir("publisher-nested-symlink-escape");
    let outside_root = temp_fixture_dir("publisher-nested-symlink-outside");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let shard_key = s3_key(&manifest.shards[0].lance_path);
    fs::create_dir_all(outside_root.join("nested")).unwrap();
    unix_fs::symlink(
        outside_root.join("nested"),
        build_root.join(&shard_key).join("escaped"),
    )
    .unwrap();

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("artifact") || error.to_string().contains("escape"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_handles_symlink_cycle_inside_shard_directory_without_hanging() {
    let build_root = temp_fixture_dir("publisher-symlink-cycle-inside-shard");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let shard_key = s3_key(&manifest.shards[0].lance_path);
    unix_fs::symlink(
        build_root.join(&shard_key),
        build_root.join(&shard_key).join("cycle"),
    )
    .unwrap();

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime
            .block_on(async move {
                publisher
                    .publish(&PublishRequest {
                        manifest,
                        expected_current_version: Some(8),
                        updated_at: 1_700_000_000_500,
                    })
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        let _ = tx.send(result);
    });

    let outcome = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("publisher publish hung on symlink cycle");

    match outcome {
        Ok(()) => {}
        Err(error) => panic!("publisher should not fail on in-root symlink cycle: {error}"),
    }
}

#[tokio::test]
async fn publisher_validates_all_shards_before_starting_any_upload() {
    let build_root = temp_fixture_dir("publisher-validate-all-shards-first");
    let mut manifest = sample_manifest(9);
    manifest.document_count = 4;
    manifest.num_shards = 2;
    manifest.shards.push(ShardManifest {
        shard_id: 1,
        document_count: 2,
        lance_path: "s3://local-artifacts/lance/v9/shard_1".into(),
        tantivy_path: "s3://local-artifacts/index/v9/shard_1".into(),
    });
    create_source_build(&build_root, &manifest);

    let invalid_shard_key = s3_key(&manifest.shards[1].lance_path);
    fs::remove_dir_all(build_root.join(&invalid_shard_key)).unwrap();
    unix_fs::symlink(
        temp_fixture_dir("publisher-invalid-late-shard-outside"),
        build_root.join(&invalid_shard_key),
    )
    .unwrap();

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("artifact") || error.to_string().contains("escape"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[tokio::test]
async fn publisher_rejects_file_backed_shard_source_before_starting_any_upload() {
    let build_root = temp_fixture_dir("publisher-file-backed-shard-source");
    let manifest = sample_manifest(9);
    create_source_build(&build_root, &manifest);

    let shard_key = s3_key(&manifest.shards[0].lance_path);
    fs::remove_dir_all(build_root.join(&shard_key)).unwrap();
    write_fixture(&build_root, &shard_key, b"not a directory");

    let storage = RecordingPublishStorage::default();
    storage.seed_file(INDEX_HEAD_KEY, head_json(8, 1_700_000_000_100));

    let publisher = IndexPublisher::new(&build_root, storage.clone());
    let error = publisher
        .publish(&PublishRequest {
            manifest,
            expected_current_version: Some(8),
            updated_at: 1_700_000_000_500,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("directory"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}
