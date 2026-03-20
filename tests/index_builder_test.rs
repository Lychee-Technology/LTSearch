use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_schema::DataType;
use ltsearch::embedding::{
    EmbeddingError, EmbeddingGenerator, LTEmbedConfig, LTEmbedEmbeddingGenerator,
    LTEmbedPoolingMode,
};
use ltsearch::indexing::{
    materialize_latest_snapshot, BuildIndexRequest, BuildIndexResult, LocalIndexBuilder,
};
use ltsearch::models::{Document, WalOperation, WalRecord};
use ltsearch::query::{KeywordSearcher, VectorSearcher};
use ltsearch::storage::{version_manifest_key, ActiveManifest, LocalManifestStore, ManifestHead};
use serde_json::{json, Value};
use tokio::runtime::Runtime;

type EmbeddingResponses = VecDeque<(String, Vec<f32>)>;

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

fn maybe_ltembed_assets_dir() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|ancestor| ancestor.join("LTEmbed/assets"))
        .find(|candidate| {
            candidate.join("config.json").exists()
                && candidate.join("tokenizer.json").exists()
                && candidate.join("model.safetensors").exists()
        })
}

fn repeated_embedding(dim: usize, value: f32) -> Vec<f32> {
    (0..dim).map(|_| value).collect()
}

fn upsert_record(
    event_id: &str,
    doc_id: &str,
    text: &str,
    embedding: Option<Vec<f32>>,
    timestamp: i64,
) -> WalRecord {
    WalRecord {
        event_id: event_id.into(),
        doc_id: doc_id.into(),
        op: WalOperation::Upsert,
        document: Some(Document {
            doc_id: doc_id.into(),
            text: text.into(),
            embedding,
            metadata: HashMap::new(),
            timestamp,
        }),
        timestamp,
    }
}

fn upsert_record_with_metadata(
    event_id: &str,
    doc_id: &str,
    text: &str,
    embedding: Option<Vec<f32>>,
    metadata: HashMap<String, Value>,
    timestamp: i64,
) -> WalRecord {
    WalRecord {
        event_id: event_id.into(),
        doc_id: doc_id.into(),
        op: WalOperation::Upsert,
        document: Some(Document {
            doc_id: doc_id.into(),
            text: text.into(),
            embedding,
            metadata,
            timestamp,
        }),
        timestamp,
    }
}

fn delete_record(event_id: &str, doc_id: &str, timestamp: i64) -> WalRecord {
    WalRecord {
        event_id: event_id.into(),
        doc_id: doc_id.into(),
        op: WalOperation::Delete,
        document: None,
        timestamp,
    }
}

fn assert_real_lance_artifact(
    root: &Path,
    version_id: u64,
    expected_rows: usize,
    expected_dim: usize,
) {
    let shard_dir = root.join(format!("lance/v{version_id}/shard_0"));
    assert!(shard_dir.is_dir());
    assert!(!shard_dir.join("rows.json").exists());

    Runtime::new().unwrap().block_on(async {
        let conn = lancedb::connect(shard_dir.to_str().unwrap())
            .execute()
            .await
            .unwrap();
        let table = conn.open_table("documents").execute().await.unwrap();

        assert_eq!(table.count_rows(None).await.unwrap(), expected_rows);

        let schema = table.schema().await.unwrap();
        let embedding_field = schema.field_with_name("embedding").unwrap();
        match embedding_field.data_type() {
            DataType::FixedSizeList(item, size) => {
                assert_eq!(*size as usize, expected_dim);
                assert_eq!(item.data_type(), &DataType::Float32);
            }
            other => panic!("expected FixedSizeList embedding column, got {other:?}"),
        }
    });
}

fn staging_dirs(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".index-build-staging-"))
        })
        .collect()
}

struct PermissionsGuard {
    path: PathBuf,
    original: fs::Permissions,
}

impl PermissionsGuard {
    fn make_readonly(path: &Path) -> Self {
        let original = fs::metadata(path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_mode(0o555);
        fs::set_permissions(path, readonly).unwrap();

        Self {
            path: path.to_path_buf(),
            original,
        }
    }
}

impl Drop for PermissionsGuard {
    fn drop(&mut self) {
        fs::set_permissions(&self.path, self.original.clone()).unwrap();
    }
}

#[derive(Clone, Debug)]
struct StubEmbeddingGenerator {
    calls: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<EmbeddingResponses>>,
}

impl StubEmbeddingGenerator {
    fn from_pairs(pairs: Vec<(&str, Vec<f32>)>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(
                pairs
                    .into_iter()
                    .map(|(text, embedding)| (text.to_string(), embedding))
                    .collect(),
            )),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl EmbeddingGenerator for StubEmbeddingGenerator {
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.calls.lock().unwrap().push(query.to_string());

        let (expected_query, embedding) = self.responses.lock().unwrap().pop_front().unwrap();
        assert_eq!(query, expected_query);

        Ok(embedding)
    }
}

#[test]
fn materialize_latest_snapshot_prefers_latest_event_per_document() {
    let snapshot = materialize_latest_snapshot(&[
        upsert_record("event-1", "doc-a", "alpha v1", None, 1_700_000_000_100),
        upsert_record(
            "event-2",
            "doc-b",
            "bravo current",
            Some(vec![0.0, 1.0, 0.0]),
            1_700_000_000_400,
        ),
        delete_record("event-3", "doc-a", 1_700_000_000_200),
        upsert_record("event-4", "doc-a", "alpha v2", None, 1_700_000_000_500),
        upsert_record(
            "event-5",
            "doc-b",
            "bravo stale",
            Some(vec![1.0, 0.0, 0.0]),
            1_700_000_000_300,
        ),
        upsert_record("event-6", "doc-c", "charlie", None, 1_700_000_000_050),
        delete_record("event-7", "doc-c", 1_700_000_000_600),
    ])
    .unwrap();

    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[0].doc_id, "doc-a");
    assert_eq!(snapshot[0].text, "alpha v2");
    assert_eq!(snapshot[1].doc_id, "doc-b");
    assert_eq!(snapshot[1].text, "bravo current");
    assert_eq!(snapshot[1].embedding, Some(vec![0.0, 1.0, 0.0]));
}

#[test]
fn materialize_latest_snapshot_uses_input_order_when_timestamps_tie() {
    let snapshot = materialize_latest_snapshot(&[
        upsert_record("event-1", "doc-a", "alpha v1", None, 1_700_000_000_100),
        upsert_record("event-2", "doc-a", "alpha v2", None, 1_700_000_000_100),
        upsert_record(
            "event-3",
            "doc-b",
            "bravo should disappear",
            Some(vec![0.0, 1.0, 0.0]),
            1_700_000_000_200,
        ),
        delete_record("event-4", "doc-b", 1_700_000_000_200),
    ])
    .unwrap();

    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].doc_id, "doc-a");
    assert_eq!(snapshot[0].text, "alpha v2");
}

#[test]
fn local_index_builder_does_not_leave_final_artifacts_after_failed_build() {
    let root = temp_fixture_dir("index-builder-no-partial-artifacts");
    let generator = StubEmbeddingGenerator::from_pairs(vec![]);
    let builder = LocalIndexBuilder::new(&root, generator);

    fs::create_dir_all(root.join("index/v7")).unwrap();
    fs::write(root.join("index/v7/shard_0"), "blocking file").unwrap();

    let error = builder
        .build(&BuildIndexRequest {
            version_id: 7,
            created_at: 1_700_000_009_000,
            embedding_dim: 3,
            records: vec![upsert_record(
                "event-1",
                "doc-kept",
                "stable body",
                Some(vec![1.0, 0.0, 0.0]),
                1_700_000_000_100,
            )],
        })
        .unwrap_err();

    assert!(error.to_string().contains("Tantivy") || error.to_string().contains("artifact"));
    assert!(!root.join("lance/v7/shard_0").exists());
    assert!(!root.join(version_manifest_key(7)).exists());
    assert!(!root.join("index/v7/shard_0/meta.json").exists());
}

#[test]
fn local_index_builder_can_build_from_async_context() {
    let root = temp_fixture_dir("index-builder-async-context");
    let builder = LocalIndexBuilder::new(&root, StubEmbeddingGenerator::from_pairs(vec![]));

    Runtime::new().unwrap().block_on(async {
        let built = builder
            .build(&BuildIndexRequest {
                version_id: 7,
                created_at: 1_700_000_009_000,
                embedding_dim: 3,
                records: vec![upsert_record(
                    "event-1",
                    "doc-kept",
                    "stable body",
                    Some(vec![1.0, 0.0, 0.0]),
                    1_700_000_000_100,
                )],
            })
            .unwrap();

        assert_eq!(built.manifest.version_id, 7);
    });
}

#[test]
fn local_index_builder_cleans_staging_directory_when_first_publish_move_fails() {
    let root = temp_fixture_dir("index-builder-staging-cleanup-on-publish-failure");
    let root_for_hook = root.clone();
    let builder = LocalIndexBuilder::new(&root, StubEmbeddingGenerator::from_pairs(vec![]))
        .with_before_publish_hook(move || {
            fs::create_dir_all(root_for_hook.join("lance/v7")).unwrap();
            fs::write(root_for_hook.join("lance/v7/shard_0"), "blocking file").unwrap();
            Ok(())
        });

    let error = builder
        .build(&BuildIndexRequest {
            version_id: 7,
            created_at: 1_700_000_009_000,
            embedding_dim: 3,
            records: vec![upsert_record(
                "event-1",
                "doc-kept",
                "stable body",
                Some(vec![1.0, 0.0, 0.0]),
                1_700_000_000_100,
            )],
        })
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("failed to publish staged artifact"));
    assert!(staging_dirs(&root).is_empty());
}

#[test]
fn local_index_builder_reports_cleanup_failure_after_publish_error() {
    let root = temp_fixture_dir("index-builder-surfaces-cleanup-failure");
    let root_for_hook = root.clone();
    let builder = LocalIndexBuilder::new(&root, StubEmbeddingGenerator::from_pairs(vec![]))
        .with_before_publish_hook(move || {
            fs::create_dir_all(root_for_hook.join("lance/v7")).unwrap();
            fs::write(root_for_hook.join("lance/v7/shard_0"), "blocking file").unwrap();
            let _guard = PermissionsGuard::make_readonly(&root_for_hook);
            std::mem::forget(_guard);
            Ok(())
        });

    let error = builder
        .build(&BuildIndexRequest {
            version_id: 7,
            created_at: 1_700_000_009_000,
            embedding_dim: 3,
            records: vec![upsert_record(
                "event-1",
                "doc-kept",
                "stable body",
                Some(vec![1.0, 0.0, 0.0]),
                1_700_000_000_100,
            )],
        })
        .unwrap_err();

    let mut restore = fs::metadata(&root).unwrap().permissions();
    restore.set_mode(0o755);
    fs::set_permissions(&root, restore).unwrap();

    assert!(error
        .to_string()
        .contains("failed to publish staged artifact"));
    assert!(error.to_string().contains("cleanup failed"));
}

#[test]
fn local_index_builder_generates_missing_embeddings_and_writes_searcher_compatible_artifacts() {
    let root = temp_fixture_dir("index-builder-local-artifacts");
    let generator =
        StubEmbeddingGenerator::from_pairs(vec![("generated body", vec![0.3, 0.7, 0.0])]);
    let metadata = HashMap::from([("lang".into(), json!("rust"))]);
    let builder = LocalIndexBuilder::new(&root, generator.clone());

    let built = builder
        .build(&BuildIndexRequest {
            version_id: 7,
            created_at: 1_700_000_009_000,
            embedding_dim: 3,
            records: vec![
                upsert_record_with_metadata(
                    "event-1",
                    "doc-kept",
                    "stable body",
                    Some(vec![1.0, 0.0, 0.0]),
                    metadata.clone(),
                    1_700_000_000_100,
                ),
                upsert_record(
                    "event-2",
                    "doc-generated",
                    "generated body",
                    None,
                    1_700_000_000_200,
                ),
                upsert_record(
                    "event-3",
                    "doc-removed",
                    "removed body",
                    Some(vec![0.0, 0.0, 1.0]),
                    1_700_000_000_150,
                ),
                delete_record("event-4", "doc-removed", 1_700_000_000_300),
            ],
        })
        .unwrap();

    assert_eq!(generator.calls(), vec!["generated body"]);

    assert_eq!(
        built,
        BuildIndexResult {
            manifest: built.manifest.clone(),
            documents: vec![
                Document {
                    doc_id: "doc-generated".into(),
                    text: "generated body".into(),
                    embedding: Some(vec![0.3, 0.7, 0.0]),
                    metadata: HashMap::new(),
                    timestamp: 1_700_000_000_200,
                },
                Document {
                    doc_id: "doc-kept".into(),
                    text: "stable body".into(),
                    embedding: Some(vec![1.0, 0.0, 0.0]),
                    metadata,
                    timestamp: 1_700_000_000_100,
                },
            ],
        }
    );

    built.manifest.validate().unwrap();
    assert_eq!(built.manifest.version_id, 7);
    assert_eq!(built.manifest.document_count, 2);
    assert_eq!(built.manifest.num_shards, 1);
    assert_eq!(
        built.manifest.shards[0].lance_path,
        "s3://local-artifacts/lance/v7/shard_0"
    );
    assert_eq!(
        built.manifest.shards[0].tantivy_path,
        "s3://local-artifacts/index/v7/shard_0"
    );

    let manifest_contents = fs::read_to_string(root.join(version_manifest_key(7))).unwrap();
    let manifest_from_disk: ltsearch::models::IndexManifest =
        serde_json::from_str(&manifest_contents).unwrap();
    assert_eq!(manifest_from_disk, built.manifest);
    assert!(!root.join("index/_head").exists());

    assert_real_lance_artifact(&root, 7, 2, 3);

    let active_manifest = ActiveManifest {
        head: ManifestHead {
            version_id: built.manifest.version_id,
            manifest_path: version_manifest_key(built.manifest.version_id),
            updated_at: built.manifest.created_at,
        },
        manifest: built.manifest.clone(),
    };

    let keyword_searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let keyword_results = keyword_searcher
        .search_active_manifest(&active_manifest, "generated", 1)
        .unwrap();
    assert_eq!(keyword_results.len(), 1);
    assert_eq!(keyword_results[0].doc_id, "doc-generated");

    let vector_searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let vector_results = vector_searcher
        .search_active_manifest(&active_manifest, &[1.0, 0.0, 0.0], 2)
        .unwrap();
    assert_eq!(vector_results.len(), 2);
    assert_eq!(vector_results[0].doc_id, "doc-kept");
    assert_eq!(vector_results[0].text, "stable body");
    assert_eq!(
        vector_results[0].metadata.as_ref().unwrap()["lang"],
        json!("rust")
    );
    assert_eq!(vector_results[0].score, 1.0);
    assert_eq!(vector_results[1].doc_id, "doc-generated");
    assert_eq!(vector_results[1].text, "generated body");
    assert!(vector_results[1].metadata.is_none());
    assert!((vector_results[1].score - 0.3).abs() < f32::EPSILON);
}

#[test]
fn local_index_builder_generates_missing_embeddings_with_ltembed() {
    let Some(assets_dir) = maybe_ltembed_assets_dir() else {
        eprintln!("Skipping: LTEmbed assets not found in sibling checkout");
        return;
    };

    let root = temp_fixture_dir("index-builder-ltembed-artifacts");
    let generator = LTEmbedEmbeddingGenerator::from_config(&LTEmbedConfig {
        model_path: assets_dir.join("model.safetensors").display().to_string(),
        config_path: assets_dir.join("config.json").display().to_string(),
        tokenizer_path: assets_dir.join("tokenizer.json").display().to_string(),
        pooling: LTEmbedPoolingMode::Mean,
        prefix: Some("passage:".into()),
    })
    .expect("expected LTEmbed generator to bootstrap from local assets");
    let builder = LocalIndexBuilder::new(&root, generator);

    let built = builder
        .build(&BuildIndexRequest {
            version_id: 8,
            created_at: 1_700_000_019_000,
            embedding_dim: 384,
            records: vec![
                upsert_record(
                    "event-1",
                    "doc-generated",
                    "rust embeddings for build path",
                    None,
                    1_700_000_001_100,
                ),
                upsert_record(
                    "event-2",
                    "doc-kept",
                    "already embedded body",
                    Some(repeated_embedding(384, 0.01)),
                    1_700_000_001_200,
                ),
            ],
        })
        .expect("expected LTEmbed-backed builder to succeed");

    assert_eq!(built.manifest.embedding_dim, 384);
    assert_eq!(built.manifest.document_count, 2);
    assert_eq!(built.documents.len(), 2);
    assert!(built
        .documents
        .iter()
        .all(|document| document.embedding.as_ref().is_some_and(|v| v.len() == 384)));

    assert_real_lance_artifact(&root, 8, 2, 384);

    let active_manifest = ActiveManifest {
        head: ManifestHead {
            version_id: built.manifest.version_id,
            manifest_path: version_manifest_key(built.manifest.version_id),
            updated_at: built.manifest.created_at,
        },
        manifest: built.manifest.clone(),
    };

    let keyword_searcher = KeywordSearcher::new(LocalManifestStore::new(&root), &root);
    let keyword_results = keyword_searcher
        .search_active_manifest(&active_manifest, "rust", 2)
        .unwrap();
    assert!(keyword_results
        .iter()
        .any(|result| result.doc_id == "doc-generated"));

    let query_generator = LTEmbedEmbeddingGenerator::from_config(&LTEmbedConfig {
        model_path: assets_dir.join("model.safetensors").display().to_string(),
        config_path: assets_dir.join("config.json").display().to_string(),
        tokenizer_path: assets_dir.join("tokenizer.json").display().to_string(),
        pooling: LTEmbedPoolingMode::Mean,
        prefix: Some("query:".into()),
    })
    .expect("expected LTEmbed query generator to bootstrap from local assets");
    let query_embedding = query_generator
        .generate("rust embeddings")
        .expect("expected LTEmbed query embedding to be generated");

    let vector_searcher = VectorSearcher::new(LocalManifestStore::new(&root), &root);
    let vector_results = vector_searcher
        .search_active_manifest(&active_manifest, &query_embedding, 2)
        .unwrap();
    assert!(!vector_results.is_empty());
    assert!(vector_results
        .iter()
        .any(|result| result.doc_id == "doc-generated"));
}
