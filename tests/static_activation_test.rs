//! End-to-end coverage for the static release activation orchestration:
//! `verify_release_dir` (8-step self-consistency), `install_into_managed_store`
//! (idempotent local install), and `activate_static_pointer` (CAS the pointer).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use ltsearch::error::PublishError;
use ltsearch::index::{
    derive_release_id, EmbeddingProfile, ReleaseManifest, ReleaseSource, StaticChunk,
    StaticReleaseBuilder,
};
use ltsearch::indexing::{
    activate_static_pointer, install_into_managed_store, verify_release_dir, StaticActivateError,
};
use ltsearch::indexing::{PublishStorage, UploadMode, VersionedObject};
use ltsearch::models::CorpusType;
use ltsearch::storage::{StaticReleaseHead, STATIC_HEAD_KEY};

// --- Fixture: a real v3 release built by StaticReleaseBuilder -----------------

fn temp_dir(name: &str) -> PathBuf {
    // A per-process atomic counter guarantees uniqueness even when two fixtures
    // are built on different threads within the same clock tick; without it, a
    // shared directory could be renamed out from under a concurrent test.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ltsearch-static-activation-{name}-{}-{unique}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir.join("release")
}

fn finite_embedding(seed: f32) -> Vec<f32> {
    (0..512)
        .map(|i| ((i as f32) * 0.001 + seed).sin())
        .collect()
}

fn citation_metadata(title: &str, resource_id: &str) -> HashMap<String, Value> {
    let mut metadata = HashMap::new();
    metadata.insert("title".to_string(), json!(title));
    metadata.insert("resource_id".to_string(), json!(resource_id));
    metadata.insert("source_type".to_string(), json!("statute"));
    metadata.insert("source_ref".to_string(), json!("第一条"));
    metadata.insert("url".to_string(), json!("https://example.com/law"));
    metadata.insert("section".to_string(), json!("总则"));
    metadata
}

const FIXTURE_MODEL_ID: &str = "jina-embeddings-v2";

fn build_v3_release_fixture() -> PathBuf {
    // Each fixture gets its own uniquely-named directory so multiple fixtures in
    // one test never collide on disk.
    let dir = temp_dir("fixture");
    let chunks = vec![
        StaticChunk {
            doc_id: "文档-1".to_string(),
            text: "第一条文本".to_string(),
            metadata: citation_metadata("宪法总纲", "res-1"),
            corpus_type: CorpusType::Legal,
        },
        StaticChunk {
            doc_id: "文档-2".to_string(),
            text: "第二条文本".to_string(),
            metadata: citation_metadata("合同法则", "res-2"),
            corpus_type: CorpusType::Contract,
        },
    ];
    let embeddings = vec![finite_embedding(0.1), finite_embedding(0.2)];
    let profile = EmbeddingProfile {
        model_id: FIXTURE_MODEL_ID.to_string(),
        dim: 512,
    };
    let source = ReleaseSource {
        kind: "lance".to_string(),
        dataset_path: "/data/corpus.lance".to_string(),
        table_version: 9,
        table_row_count: 2,
        corpus_type: CorpusType::Legal,
    };

    StaticReleaseBuilder
        .build_release(&dir, &chunks, &embeddings, &profile, &source)
        .expect("build_release should succeed");
    dir
}

fn corrupt_one_byte(path: &Path) {
    let mut bytes = fs::read(path).unwrap();
    assert!(!bytes.is_empty(), "cannot corrupt an empty file");
    bytes[0] ^= 0xff;
    fs::write(path, bytes).unwrap();
}

// --- verify_release_dir ------------------------------------------------------

#[test]
fn verify_rejects_tampered_output_hash() {
    let dir = build_v3_release_fixture();
    corrupt_one_byte(&dir.join("turbo_static_text.bin"));
    assert!(matches!(
        verify_release_dir(&dir, None, None).unwrap_err(),
        StaticActivateError::Verify { .. }
    ));
}

#[test]
fn verify_rejects_unexpected_model_id() {
    let dir = build_v3_release_fixture();
    assert!(matches!(
        verify_release_dir(&dir, Some("wrong-model"), None).unwrap_err(),
        StaticActivateError::Verify { .. }
    ));
}

#[test]
fn verify_rejects_manifest_with_missing_output_entry() {
    // A crafted manifest that drops one of the nine v3 `.bin` outputs, with
    // `release_id` re-derived over the *reduced* output set so the forged
    // manifest stays self-consistent through steps 1-4. The file still exists on
    // disk (MmapIndex would still read it by fixed name), so only an explicit
    // "outputs must cover all nine artifacts" check can catch this.
    let dir = build_v3_release_fixture();
    let manifest_path = dir.join("release_manifest.json");
    let mut manifest: ReleaseManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();

    // Drop one listed output (its file is left on disk untouched).
    let dropped = manifest
        .outputs
        .iter()
        .position(|output| output.name == "turbo_static_title.bin")
        .expect("fixture must list turbo_static_title.bin");
    manifest.outputs.remove(dropped);

    // Re-derive release_id over the reduced set so steps 1-4 all pass.
    manifest.release_id = derive_release_id(
        manifest.turbo_version,
        &manifest.embedding_profile,
        &manifest.codec,
        &manifest.input_fingerprint.content_digest,
        &manifest.outputs,
    );
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    assert!(matches!(
        verify_release_dir(&dir, None, None).unwrap_err(),
        StaticActivateError::Verify { .. }
    ));
}

#[test]
fn verify_accepts_valid_release() {
    let dir = build_v3_release_fixture();
    let manifest = verify_release_dir(&dir, None, Some(512)).unwrap();
    assert_eq!(manifest.turbo_version, 3);
    assert_eq!(manifest.release_id.len(), 64);
}

// --- install_into_managed_store ----------------------------------------------

#[test]
fn install_into_managed_store_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let src = build_v3_release_fixture();
    let rid = "c".repeat(64);
    install_into_managed_store(root.path(), &rid, &src).unwrap();
    install_into_managed_store(root.path(), &rid, &src).unwrap(); // 二次不报错
    assert!(root
        .path()
        .join(format!("static/releases/{rid}/release_manifest.json"))
        .exists());
}

// --- activate_static_pointer -------------------------------------------------

#[tokio::test]
async fn activate_writes_pointer_when_none_present() {
    let storage = RecordingPublishStorage::default();
    let res = activate_static_pointer(&storage, &"a".repeat(64), 1_700_000_000_000)
        .await
        .unwrap();
    assert_eq!(res.previous_release_id, None);
    let obj = storage.read(STATIC_HEAD_KEY).await.unwrap().unwrap();
    let head = StaticReleaseHead::from_json(&obj.bytes).unwrap();
    assert_eq!(head.release_id, "a".repeat(64));
}

#[tokio::test]
async fn activate_reports_lost_cas_on_conflict() {
    let storage = RecordingPublishStorage::default();
    // 预植抢先写入的现值 → 我方 expected(None) 过期 → lost CAS
    storage.conflict_on_compare_and_swap(
        StaticReleaseHead::new("f".repeat(64), 1_700_000_000_000)
            .to_json_pretty()
            .into_bytes(),
    );
    let err = activate_static_pointer(&storage, &"b".repeat(64), 1_700_000_000_001)
        .await
        .unwrap_err();
    assert!(matches!(err, StaticActivateError::LostCas { .. }));
}

// --- Minimal in-memory PublishStorage fake -----------------------------------
// Mirrors the `RecordingPublishStorage` in `tests/publisher_test.rs`, trimmed to
// the surface `activate_static_pointer` exercises: `read` + `compare_and_swap`,
// with `conflict_on_compare_and_swap` pre-planting a competing current value so a
// CAS with a stale expectation loses.

#[derive(Clone, Debug)]
struct StoredFile {
    bytes: Vec<u8>,
    etag: String,
}

#[derive(Clone, Debug, Default)]
struct RecordingPublishStorage {
    objects: Arc<Mutex<HashMap<String, StoredFile>>>,
    etag_counter: Arc<Mutex<u64>>,
    compare_and_swap_conflict: Arc<Mutex<Option<Vec<u8>>>>,
}

impl RecordingPublishStorage {
    fn seed_file(&self, key: &str, bytes: Vec<u8>) {
        let mut counter = self.etag_counter.lock().unwrap();
        *counter += 1;
        let etag = format!("\"etag-{}\"", *counter);
        drop(counter);
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), StoredFile { bytes, etag });
    }

    fn etag_of(&self, key: &str) -> Option<String> {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .map(|object| object.etag.clone())
    }

    fn conflict_on_compare_and_swap(&self, bytes: Vec<u8>) {
        *self.compare_and_swap_conflict.lock().unwrap() = Some(bytes);
    }
}

#[async_trait]
impl PublishStorage for RecordingPublishStorage {
    async fn upload_directory(
        &self,
        _key: &str,
        _source: &Path,
        _mode: UploadMode,
    ) -> Result<(), PublishError> {
        unimplemented!("activate_static_pointer never uploads directories")
    }

    async fn upload_file(
        &self,
        _key: &str,
        _source: &Path,
        _mode: UploadMode,
    ) -> Result<(), PublishError> {
        unimplemented!("activate_static_pointer never uploads files")
    }

    async fn read(&self, key: &str) -> Result<Option<VersionedObject>, PublishError> {
        Ok(self
            .objects
            .lock()
            .unwrap()
            .get(key)
            .map(|object| VersionedObject {
                bytes: object.bytes.clone(),
                etag: object.etag.clone(),
            }))
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_etag: Option<&str>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        if let Some(conflict_bytes) = self.compare_and_swap_conflict.lock().unwrap().take() {
            self.seed_file(key, conflict_bytes);
            return Ok(false);
        }

        let current_etag = self.etag_of(key);
        if current_etag.as_deref() != expected_etag {
            return Ok(false);
        }

        self.seed_file(key, new_value.to_vec());
        Ok(true)
    }
}
