//! Shared test support for the static-release surface: a real v3 release
//! `build_v3_release_fixture` (built by `StaticReleaseBuilder`) and an in-memory
//! `RecordingPublishStorage` fake. Both were previously copy-pasted across
//! `static_activation_test.rs`, `write_build_publish_test.rs`, and
//! `publisher_test.rs`; converging them here keeps the fixture and fake from
//! drifting apart.
//!
//! Included via `mod support;` by several test binaries, each of which uses only
//! a subset — hence the crate-level `dead_code` allowance. Depends only on the
//! provider-neutral `PublishStorage` / `StaticReleaseBuilder` contracts, so it
//! compiles unchanged under both the `local` and `aws` profiles (no AWS deps).
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use ltsearch::error::PublishError;
use ltsearch::index::{EmbeddingProfile, ReleaseSource, StaticChunk, StaticReleaseBuilder};
use ltsearch::indexing::{PublishStorage, UploadMode, VersionedObject};
use ltsearch::models::CorpusType;

// --- Real v3 release fixture -------------------------------------------------

pub const FIXTURE_MODEL_ID: &str = "jina-embeddings-v2";

/// Returns a fresh, uniquely-named release directory under the OS temp dir. A
/// per-process atomic counter guarantees uniqueness even when two fixtures are
/// built on different threads within the same clock tick; without it, a shared
/// directory could be renamed out from under a concurrent test.
pub fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ltsearch-support-{name}-{}-{unique}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir.join("release")
}

pub fn finite_embedding(seed: f32) -> Vec<f32> {
    (0..512)
        .map(|i| ((i as f32) * 0.001 + seed).sin())
        .collect()
}

pub fn citation_metadata(title: &str, resource_id: &str) -> HashMap<String, Value> {
    let mut metadata = HashMap::new();
    metadata.insert("title".to_string(), json!(title));
    metadata.insert("resource_id".to_string(), json!(resource_id));
    metadata.insert("source_type".to_string(), json!("statute"));
    metadata.insert("source_ref".to_string(), json!("第一条"));
    metadata.insert("url".to_string(), json!("https://example.com/law"));
    metadata.insert("section".to_string(), json!("总则"));
    metadata
}

/// Builds a real, self-consistent v3 static release via `StaticReleaseBuilder`,
/// returning the release directory. Each call gets a uniquely-named directory so
/// multiple fixtures in one test never collide on disk.
pub fn build_v3_release_fixture() -> PathBuf {
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

/// Flips the first byte of the file at `path`, corrupting its content-hash.
pub fn corrupt_one_byte(path: &Path) {
    let mut bytes = fs::read(path).unwrap();
    assert!(!bytes.is_empty(), "cannot corrupt an empty file");
    bytes[0] ^= 0xff;
    fs::write(path, bytes).unwrap();
}

// --- In-memory PublishStorage fake -------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredObject {
    File { bytes: Vec<u8>, etag: String },
    Directory,
}

/// Records every call and stores objects in memory. `conflict_on_compare_and_swap`
/// pre-plants a competing current value so the next CAS observes a stale
/// expectation and loses (`Ok(false)`), the way a real racing writer would.
#[derive(Clone, Debug, Default)]
pub struct RecordingPublishStorage {
    pub objects: Arc<Mutex<HashMap<String, StoredObject>>>,
    pub calls: Arc<Mutex<Vec<String>>>,
    pub etag_counter: Arc<Mutex<u64>>,
    pub last_expected: Arc<Mutex<Option<Option<String>>>>,
    pub compare_and_swap_conflict: Arc<Mutex<Option<Vec<u8>>>>,
}

impl RecordingPublishStorage {
    pub fn seed_file(&self, key: &str, bytes: Vec<u8>) {
        let mut counter = self.etag_counter.lock().unwrap();
        *counter += 1;
        let etag = format!("\"etag-{}\"", *counter);
        drop(counter);
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), StoredObject::File { bytes, etag });
    }

    pub fn etag_of(&self, key: &str) -> Option<String> {
        self.objects.lock().unwrap().get(key).and_then(|object| {
            if let StoredObject::File { etag, .. } = object {
                Some(etag.clone())
            } else {
                None
            }
        })
    }

    pub fn seed_directory(&self, key: &str) {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), StoredObject::Directory);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn file_bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(key).and_then(|object| {
            if let StoredObject::File { bytes, .. } = object {
                Some(bytes.clone())
            } else {
                None
            }
        })
    }

    pub fn last_expected(&self) -> Option<Option<String>> {
        self.last_expected.lock().unwrap().clone()
    }

    pub fn conflict_on_compare_and_swap(&self, bytes: Vec<u8>) {
        *self.compare_and_swap_conflict.lock().unwrap() = Some(bytes);
    }
}

#[async_trait]
impl PublishStorage for RecordingPublishStorage {
    async fn upload_directory(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
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
        for entry in fs::read_dir(source).map_err(|source_error| PublishError::Operation {
            message: format!("failed to read {}: {source_error}", source.display()),
        })? {
            let entry = entry.map_err(|source_error| PublishError::Operation {
                message: format!("failed to iterate {}: {source_error}", source.display()),
            })?;
            let path = entry.path();
            let child_key = format!("{key}/{}", entry.file_name().to_string_lossy());
            let file_type = entry
                .file_type()
                .map_err(|source_error| PublishError::Operation {
                    message: format!("failed to inspect {}: {source_error}", path.display()),
                })?;
            if file_type.is_dir() {
                self.upload_directory(&child_key, &path, mode).await?;
            } else if file_type.is_file() {
                self.upload_file(&child_key, &path, mode).await?;
            }
        }
        Ok(())
    }

    async fn upload_file(
        &self,
        key: &str,
        source: &Path,
        mode: UploadMode,
    ) -> Result<(), PublishError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("upload_file:{key}"));

        if mode == UploadMode::CreateOnly && self.file_bytes(key).is_some() {
            return Err(PublishError::Operation {
                message: format!(
                    "refusing to overwrite existing version artifact {key}: version artifacts are immutable"
                ),
            });
        }

        let bytes = fs::read(source).map_err(|source_error| PublishError::Operation {
            message: format!("failed to read {}: {source_error}", source.display()),
        })?;
        self.seed_file(key, bytes);
        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Option<VersionedObject>, PublishError> {
        self.calls.lock().unwrap().push(format!("read:{key}"));
        Ok(self.objects.lock().unwrap().get(key).and_then(|object| {
            if let StoredObject::File { bytes, etag } = object {
                Some(VersionedObject {
                    bytes: bytes.clone(),
                    etag: etag.clone(),
                })
            } else {
                None
            }
        }))
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_etag: Option<&str>,
        new_value: &[u8],
    ) -> Result<bool, PublishError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("compare_and_swap:{key}"));
        *self.last_expected.lock().unwrap() = Some(expected_etag.map(|etag| etag.to_string()));

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
