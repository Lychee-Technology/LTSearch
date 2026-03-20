use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_schema::DataType;
use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::indexing::{BuildIndexRequest, LocalIndexBuilder};
use ltsearch::models::{Document, WalOperation, WalRecord};
use ltsearch::query::{KeywordSearcher, VectorSearcher};
use ltsearch::storage::{version_manifest_key, ActiveManifest, LocalManifestStore, ManifestHead};
use serde_json::{json, Value};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value as TantivyValue;
use tantivy::{Index, TantivyDocument};
use tokio::runtime::Runtime;

type EmbeddingResponses = VecDeque<(String, Vec<f32>)>;

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

fn active_manifest_from_built(built: &ltsearch::indexing::BuildIndexResult) -> ActiveManifest {
    ActiveManifest {
        head: ManifestHead {
            version_id: built.manifest.version_id,
            manifest_path: version_manifest_key(built.manifest.version_id),
            updated_at: built.manifest.created_at,
        },
        manifest: built.manifest.clone(),
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

#[test]
fn builder_writes_manifest_with_publishable_artifact_paths_and_consistent_counts() {
    let root = temp_fixture_dir("builder-artifact-contract-manifest");
    let generator =
        StubEmbeddingGenerator::from_pairs(vec![("generated body", vec![0.3, 0.7, 0.0])]);
    let builder = LocalIndexBuilder::new(&root, generator);

    let built = builder
        .build(&BuildIndexRequest {
            version_id: 11,
            created_at: 1_700_000_039_000,
            embedding_dim: 3,
            records: vec![
                upsert_record_with_metadata(
                    "event-1",
                    "doc-generated",
                    "generated body",
                    None,
                    HashMap::from([("lang".into(), json!("rust"))]),
                    1_700_000_000_200,
                ),
                upsert_record_with_metadata(
                    "event-2",
                    "doc-kept",
                    "stable body",
                    Some(vec![1.0, 0.0, 0.0]),
                    HashMap::from([("lang".into(), json!("rust"))]),
                    1_700_000_000_100,
                ),
            ],
        })
        .unwrap();

    assert_eq!(built.manifest.version_id, 11);
    assert_eq!(built.manifest.document_count, built.documents.len());
    assert_eq!(built.manifest.num_shards, built.manifest.shards.len());
    assert_eq!(
        built.manifest.shards[0].document_count,
        built.documents.len()
    );
    assert_eq!(
        built.manifest.shards[0].lance_path,
        "s3://local-artifacts/lance/v11/shard_0"
    );
    assert_eq!(
        built.manifest.shards[0].tantivy_path,
        "s3://local-artifacts/index/v11/shard_0"
    );

    let manifest_contents = fs::read_to_string(root.join(version_manifest_key(11))).unwrap();
    let manifest_from_disk: ltsearch::models::IndexManifest =
        serde_json::from_str(&manifest_contents).unwrap();
    assert_eq!(manifest_from_disk, built.manifest);

    assert!(root.join("lance/v11/shard_0").is_dir());
    assert!(root.join("index/v11/shard_0").is_dir());
    assert!(!root.join("index/_head").exists());
}

#[test]
fn builder_outputs_tantivy_and_lance_artifacts_matching_document_contract() {
    let root = temp_fixture_dir("builder-artifact-contract-searchable");
    let generator =
        StubEmbeddingGenerator::from_pairs(vec![("generated body", vec![0.3, 0.7, 0.0])]);
    let builder = LocalIndexBuilder::new(&root, generator.clone());

    let built = builder
        .build(&BuildIndexRequest {
            version_id: 12,
            created_at: 1_700_000_049_000,
            embedding_dim: 3,
            records: vec![
                upsert_record_with_metadata(
                    "event-1",
                    "doc-generated",
                    "generated body",
                    None,
                    HashMap::new(),
                    1_700_000_000_200,
                ),
                upsert_record_with_metadata(
                    "event-2",
                    "doc-kept",
                    "stable body",
                    Some(vec![1.0, 0.0, 0.0]),
                    HashMap::from([("lang".into(), json!("rust"))]),
                    1_700_000_000_100,
                ),
            ],
        })
        .unwrap();

    assert_eq!(generator.calls(), vec!["generated body"]);
    assert_real_lance_artifact(&root, 12, 2, 3);

    let tantivy_index = Index::open_in_dir(root.join("index/v12/shard_0")).unwrap();
    let schema = tantivy_index.schema();
    let doc_id = schema.get_field("doc_id").unwrap();
    let text = schema.get_field("text").unwrap();
    let reader = tantivy_index.reader().unwrap();
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(&tantivy_index, vec![text]);
    let query = parser.parse_query("generated").unwrap();
    let hits = searcher.search(&query, &TopDocs::with_limit(2)).unwrap();
    assert_eq!(hits.len(), 1);
    let retrieved: TantivyDocument = searcher.doc(hits[0].1).unwrap();
    let hit_doc_id = retrieved
        .get_first(doc_id)
        .and_then(|value| value.as_str())
        .unwrap();
    assert_eq!(hit_doc_id, "doc-generated");

    let active_manifest = active_manifest_from_built(&built);
    let keyword_results = KeywordSearcher::new(LocalManifestStore::new(&root), &root)
        .search_active_manifest(&active_manifest, "generated", 1)
        .unwrap();
    assert_eq!(keyword_results[0].doc_id, "doc-generated");

    let vector_results = VectorSearcher::new(LocalManifestStore::new(&root), &root)
        .search_active_manifest(&active_manifest, &[1.0, 0.0, 0.0], 2)
        .unwrap();
    assert_eq!(vector_results[0].doc_id, "doc-kept");
    assert_eq!(vector_results[1].doc_id, "doc-generated");
}
