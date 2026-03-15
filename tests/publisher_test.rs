use std::collections::HashMap;
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ltsearch::error::PublishError;
use ltsearch::indexing::{IndexPublisher, PublishRequest, PublishStorage};
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

impl PublishStorage for RecordingPublishStorage {
    fn upload_directory(&self, key: &str, source: &Path) -> Result<(), PublishError> {
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

    fn upload_file(&self, key: &str, source: &Path) -> Result<(), PublishError> {
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

    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, PublishError> {
        self.calls.lock().unwrap().push(format!("read:{key}"));
        Ok(self.file_bytes(key))
    }

    fn compare_and_swap(
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

#[test]
fn publisher_uploads_artifacts_and_manifest_before_updating_head() {
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

#[test]
fn publisher_conditionally_updates_head_with_current_head_contents() {
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
        .unwrap();

    assert_eq!(storage.last_expected(), Some(Some(current_head)));
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(9, 1_700_000_000_500)
    );
}

#[test]
fn publisher_rejects_publish_race_without_corrupting_active_version() {
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
        .unwrap_err();

    assert!(error.to_string().contains("conflict"));
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(10, 1_700_000_000_600)
    );
}

#[test]
fn publisher_preserves_previous_version_artifacts_on_success() {
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

#[test]
fn publisher_rejects_zero_version_before_upload_or_head_update() {
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
        .unwrap_err();

    assert!(error.to_string().contains("version_id"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
    assert_eq!(
        storage.file_bytes(INDEX_HEAD_KEY).unwrap(),
        head_json(8, 1_700_000_000_100)
    );
}

#[test]
fn publisher_rejects_path_escaping_artifact_keys_before_upload() {
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
        .unwrap_err();

    assert!(error.to_string().contains("path") || error.to_string().contains("artifact"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[test]
fn publisher_rejects_absolute_style_artifact_keys_before_upload() {
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
        .unwrap_err();

    assert!(error.to_string().contains("path") || error.to_string().contains("artifact"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[test]
fn publisher_rejects_stale_manifest_file_before_upload() {
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
        .unwrap_err();

    assert!(error.to_string().contains("manifest"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[test]
fn publisher_rejects_invalid_current_head_before_upload() {
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
        .unwrap_err();

    assert!(
        error.to_string().contains("version_id") || error.to_string().contains("manifest_path")
    );
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[test]
fn publisher_rejects_symlinked_shard_path_that_escapes_artifact_root() {
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
        .unwrap_err();

    assert!(error.to_string().contains("artifact") || error.to_string().contains("escape"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[test]
fn publisher_rejects_symlinked_manifest_path_that_escapes_artifact_root() {
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
        .unwrap_err();

    assert!(error.to_string().contains("artifact") || error.to_string().contains("escape"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[test]
fn publisher_rejects_mismatched_manifest_path_in_current_head_before_upload() {
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
        .unwrap_err();

    assert!(error.to_string().contains("manifest_path"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[test]
fn publisher_rejects_nested_symlink_escape_inside_shard_directory() {
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
        .unwrap_err();

    assert!(error.to_string().contains("artifact") || error.to_string().contains("escape"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}

#[test]
fn publisher_validates_all_shards_before_starting_any_upload() {
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
        .unwrap_err();

    assert!(error.to_string().contains("artifact") || error.to_string().contains("escape"));
    assert_eq!(storage.calls(), vec![format!("read:{INDEX_HEAD_KEY}")]);
}
