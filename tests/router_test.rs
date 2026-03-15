use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ltsearch::embedding::{EmbeddingError, EmbeddingGenerator};
use ltsearch::error::{SearchError, ValidationError};
use ltsearch::models::{
    FilterValue, IndexManifest, SearchRequest, SearchResult, SearchSource, ShardManifest,
};
use ltsearch::query::{
    KeywordRetriever, KeywordSearcher, QueryRouter, VectorRetriever, VectorSearcher,
};
use ltsearch::storage::{
    version_manifest_key, ActiveManifest, LocalManifestStore, ManifestHead, ManifestStore,
    ManifestStoreError, INDEX_HEAD_KEY,
};
use serde_json::json;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{Index, IndexWriter};

type EmbeddingOutcome = Result<Vec<f32>, EmbeddingError>;
type EmbeddingOutcomes = Arc<Mutex<Vec<EmbeddingOutcome>>>;

fn temp_fixture_dir(test_name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
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

fn write_rows_json(root: &Path, relative_path: &str, rows_json: &str) {
    let path = root.join(relative_path).join("rows.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, rows_json).unwrap();
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
  "document_count": 2,
  "num_shards": 1,
  "shards": [
    {{
      "shard_id": 0,
      "document_count": 2,
      "lance_path": "s3://bucket/lance/v{version_id}/shard_0",
      "tantivy_path": "s3://bucket/index/v{version_id}/shard_0"
    }}
  ]
}}"#
    )
}

fn sample_request() -> SearchRequest {
    SearchRequest {
        query: "rust search".into(),
        top_k: 3,
        filters: None,
        include_metadata: false,
    }
}

fn sample_result(doc_id: &str, score: f32, source: SearchSource) -> SearchResult {
    SearchResult {
        doc_id: doc_id.into(),
        score,
        text: format!("text for {doc_id}"),
        metadata: None,
        source,
    }
}

fn sample_active_manifest(version_id: u64) -> ActiveManifest {
    ActiveManifest {
        head: ManifestHead {
            version_id,
            manifest_path: format!("manifests/{version_id}.json"),
            updated_at: 1_700_000_000_000,
        },
        manifest: IndexManifest {
            version_id,
            created_at: 1_700_000_000_000,
            embedding_dim: 3,
            document_count: 1,
            num_shards: 1,
            shards: vec![ShardManifest {
                shard_id: 0,
                document_count: 1,
                lance_path: format!("s3://bucket/lance/v{version_id}/shard_0"),
                tantivy_path: format!("s3://bucket/index/v{version_id}/shard_0"),
            }],
        },
    }
}

#[derive(Clone)]
struct StubManifestStore {
    active_manifest: ActiveManifest,
    load_head_calls: Arc<AtomicUsize>,
    load_active_manifest_calls: Arc<AtomicUsize>,
}

impl StubManifestStore {
    fn new(version_id: u64) -> Self {
        Self {
            active_manifest: sample_active_manifest(version_id),
            load_head_calls: Arc::new(AtomicUsize::new(0)),
            load_active_manifest_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ManifestStore for StubManifestStore {
    fn load_head(&self) -> Result<ManifestHead, ManifestStoreError> {
        self.load_head_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.active_manifest.head.clone())
    }

    fn load_active_manifest(&self) -> Result<ActiveManifest, ManifestStoreError> {
        self.load_active_manifest_calls
            .fetch_add(1, Ordering::SeqCst);
        Ok(self.active_manifest.clone())
    }
}

#[derive(Clone)]
struct StubEmbeddingGenerator {
    outcomes: EmbeddingOutcomes,
    calls: Arc<AtomicUsize>,
    expected_query: String,
}

impl StubEmbeddingGenerator {
    fn success(embedding: Vec<f32>) -> Self {
        Self::success_for_query("rust search", embedding)
    }

    fn success_for_query(query: &str, embedding: Vec<f32>) -> Self {
        Self {
            outcomes: Arc::new(Mutex::new(vec![Ok(embedding)])),
            calls: Arc::new(AtomicUsize::new(0)),
            expected_query: query.into(),
        }
    }

    fn failure(message: &str) -> Self {
        Self {
            outcomes: Arc::new(Mutex::new(vec![Err(EmbeddingError::Generation {
                message: message.into(),
            })])),
            calls: Arc::new(AtomicUsize::new(0)),
            expected_query: "rust search".into(),
        }
    }

    fn sequence(query: &str, outcomes: Vec<EmbeddingOutcome>) -> Self {
        Self {
            outcomes: Arc::new(Mutex::new(outcomes)),
            calls: Arc::new(AtomicUsize::new(0)),
            expected_query: query.into(),
        }
    }
}

impl EmbeddingGenerator for StubEmbeddingGenerator {
    fn generate(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        assert_eq!(query, self.expected_query);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut outcomes = self.outcomes.lock().unwrap();
        if outcomes.is_empty() {
            return Err(EmbeddingError::Generation {
                message: "no embedding outcome configured".into(),
            });
        }
        outcomes.remove(0)
    }
}

#[derive(Clone, Default)]
struct SearchRecorder {
    calls: Arc<AtomicUsize>,
    started_at: Arc<Mutex<Vec<Duration>>>,
}

impl SearchRecorder {
    fn record_start(&self, started_after: Duration) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started_at.lock().unwrap().push(started_after);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn starts(&self) -> Vec<Duration> {
        self.started_at.lock().unwrap().clone()
    }
}

#[derive(Clone)]
struct StubKeywordRetriever {
    results: Vec<SearchResult>,
    delay: Duration,
    recorder: SearchRecorder,
    start: Arc<Instant>,
    expected_version: u64,
}

impl StubKeywordRetriever {
    fn new(
        results: Vec<SearchResult>,
        delay: Duration,
        start: Arc<Instant>,
        expected_version: u64,
    ) -> Self {
        Self {
            results,
            delay,
            recorder: SearchRecorder::default(),
            start,
            expected_version,
        }
    }
}

impl KeywordRetriever for StubKeywordRetriever {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        assert_eq!(active_manifest.head.version_id, self.expected_version);
        assert_eq!(query, "rust search");
        assert_eq!(top_k, 3);
        self.recorder.record_start(self.start.elapsed());
        std::thread::sleep(self.delay);
        Ok(self.results.clone())
    }
}

#[derive(Clone)]
struct StubVectorRetriever {
    results: Vec<SearchResult>,
    delay: Duration,
    recorder: SearchRecorder,
    start: Arc<Instant>,
    expected_version: u64,
}

impl StubVectorRetriever {
    fn new(
        results: Vec<SearchResult>,
        delay: Duration,
        start: Arc<Instant>,
        expected_version: u64,
    ) -> Self {
        Self {
            results,
            delay,
            recorder: SearchRecorder::default(),
            start,
            expected_version,
        }
    }
}

impl VectorRetriever for StubVectorRetriever {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        assert_eq!(active_manifest.head.version_id, self.expected_version);
        assert_eq!(active_manifest.manifest.embedding_dim, 3);
        assert_eq!(query_embedding, [0.1, 0.2, 0.3]);
        assert_eq!(top_k, 3);
        self.recorder.record_start(self.start.elapsed());
        std::thread::sleep(self.delay);
        Ok(self.results.clone())
    }
}

#[derive(Clone)]
struct PanickingKeywordRetriever;

impl KeywordRetriever for PanickingKeywordRetriever {
    fn search(
        &self,
        _active_manifest: &ActiveManifest,
        _query: &str,
        _top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        panic!("keyword retriever panicked");
    }
}

#[test]
fn router_fuses_hybrid_results_and_runs_retrievers_in_parallel() {
    let start = Arc::new(Instant::now());
    let manifest_store = StubManifestStore::new(42);
    let load_head_calls = manifest_store.load_head_calls.clone();
    let load_active_manifest_calls = manifest_store.load_active_manifest_calls.clone();
    let generator = StubEmbeddingGenerator::success(vec![0.1, 0.2, 0.3]);
    let keyword = StubKeywordRetriever::new(
        vec![
            sample_result("doc-2", 8.0, SearchSource::Keyword),
            sample_result("doc-3", 7.0, SearchSource::Keyword),
        ],
        Duration::from_millis(200),
        start.clone(),
        42,
    );
    let vector = StubVectorRetriever::new(
        vec![
            sample_result("doc-1", 0.9, SearchSource::Vector),
            sample_result("doc-2", 0.8, SearchSource::Vector),
        ],
        Duration::from_millis(200),
        start.clone(),
        42,
    );
    let router = QueryRouter::new(manifest_store, generator, keyword.clone(), vector.clone());

    let response = router.search(&sample_request()).unwrap();

    assert_eq!(response.index_version, 42);
    assert_eq!(response.total_count, 3);
    assert_eq!(
        response
            .results
            .iter()
            .map(|result| result.doc_id.as_str())
            .collect::<Vec<_>>(),
        vec!["doc-2", "doc-1", "doc-3"]
    );
    assert!(response
        .results
        .iter()
        .all(|result| result.source == SearchSource::Hybrid));
    assert_eq!(keyword.recorder.calls(), 1);
    assert_eq!(vector.recorder.calls(), 1);
    assert_eq!(load_active_manifest_calls.load(Ordering::SeqCst), 1);
    assert_eq!(load_head_calls.load(Ordering::SeqCst), 0);

    let keyword_started = keyword.recorder.starts()[0];
    let vector_started = vector.recorder.starts()[0];
    let started_gap = if keyword_started >= vector_started {
        keyword_started - vector_started
    } else {
        vector_started - keyword_started
    };
    assert!(
        started_gap < Duration::from_millis(80),
        "expected keyword/vector retrieval to start in parallel, gap was {started_gap:?}"
    );
}

#[test]
fn router_falls_back_to_keyword_only_when_embedding_generation_fails() {
    let start = Arc::new(Instant::now());
    let manifest_store = StubManifestStore::new(7);
    let load_head_calls = manifest_store.load_head_calls.clone();
    let load_active_manifest_calls = manifest_store.load_active_manifest_calls.clone();
    let generator = StubEmbeddingGenerator::failure("embedding backend unavailable");
    let keyword = StubKeywordRetriever::new(
        vec![sample_result("doc-9", 5.0, SearchSource::Keyword)],
        Duration::from_millis(0),
        start.clone(),
        7,
    );
    let vector = StubVectorRetriever::new(
        vec![sample_result("doc-10", 0.9, SearchSource::Vector)],
        Duration::from_millis(0),
        start,
        7,
    );
    let router = QueryRouter::new(manifest_store, generator, keyword.clone(), vector.clone());

    let response = router.search(&sample_request()).unwrap();

    assert_eq!(response.index_version, 7);
    assert_eq!(response.total_count, 1);
    assert_eq!(response.results[0].doc_id, "doc-9");
    assert_eq!(response.results[0].source, SearchSource::Keyword);
    assert_eq!(keyword.recorder.calls(), 1);
    assert_eq!(vector.recorder.calls(), 0);
    assert_eq!(load_active_manifest_calls.load(Ordering::SeqCst), 1);
    assert_eq!(load_head_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn router_retries_embedding_generation_before_keyword_only_fallback() {
    let start = Arc::new(Instant::now());
    let manifest_store = StubManifestStore::new(8);
    let generator = StubEmbeddingGenerator::sequence(
        "rust search",
        vec![
            Err(EmbeddingError::Generation {
                message: "transient timeout".into(),
            }),
            Ok(vec![0.1, 0.2, 0.3]),
        ],
    );
    let generator_calls = generator.calls.clone();
    let keyword = StubKeywordRetriever::new(
        vec![sample_result("doc-1", 5.0, SearchSource::Keyword)],
        Duration::from_millis(0),
        start.clone(),
        8,
    );
    let vector = StubVectorRetriever::new(
        vec![sample_result("doc-2", 0.9, SearchSource::Vector)],
        Duration::from_millis(0),
        start,
        8,
    );
    let router = QueryRouter::new(manifest_store, generator, keyword.clone(), vector.clone());

    let response = router.search(&sample_request()).unwrap();

    assert_eq!(generator_calls.load(Ordering::SeqCst), 2);
    assert_eq!(keyword.recorder.calls(), 1);
    assert_eq!(vector.recorder.calls(), 1);
    assert!(response
        .results
        .iter()
        .all(|result| result.source == SearchSource::Hybrid));
}

#[test]
fn router_rejects_invalid_requests_before_touching_dependencies() {
    let start = Arc::new(Instant::now());
    let manifest_store = StubManifestStore::new(9);
    let load_head_calls = manifest_store.load_head_calls.clone();
    let generator = StubEmbeddingGenerator::success(vec![0.1, 0.2, 0.3]);
    let generator_calls = generator.calls.clone();
    let keyword = StubKeywordRetriever::new(vec![], Duration::from_millis(0), start.clone(), 9);
    let vector = StubVectorRetriever::new(vec![], Duration::from_millis(0), start, 9);
    let router = QueryRouter::new(manifest_store, generator, keyword.clone(), vector.clone());
    let request = SearchRequest {
        query: String::new(),
        top_k: 3,
        filters: None,
        include_metadata: false,
    };

    let error = router.search(&request).unwrap_err();

    assert!(matches!(
        error,
        SearchError::Validation(ValidationError::Required { field: "query" })
    ));
    assert_eq!(load_head_calls.load(Ordering::SeqCst), 0);
    assert_eq!(generator_calls.load(Ordering::SeqCst), 0);
    assert_eq!(keyword.recorder.calls(), 0);
    assert_eq!(vector.recorder.calls(), 0);
}

#[test]
fn router_uses_concrete_retrievers_without_forwarding_router_only_request_fields() {
    let root = temp_fixture_dir("router-concrete-retrievers-router-owned-fields");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    write_rows_json(
        &root,
        "lance/v7/shard_0",
        r#"[
  {"doc_id":"doc-1","text":"rust hybrid search","embedding":[0.9,0.0,0.0]},
  {"doc_id":"doc-2","text":"rust keyword","embedding":[0.8,0.0,0.0]}
]"#,
    );

    let router = QueryRouter::new(
        LocalManifestStore::new(&root),
        StubEmbeddingGenerator::success_for_query("rust", vec![0.1, 0.2, 0.3]),
        KeywordSearcher::new(LocalManifestStore::new(&root), &root),
        VectorSearcher::new(LocalManifestStore::new(&root), &root),
    );
    let request = SearchRequest {
        query: "rust".into(),
        top_k: 2,
        filters: Some(HashMap::new()),
        include_metadata: true,
    };

    let response = router.search(&request).unwrap();

    assert_eq!(response.index_version, 7);
    assert_eq!(response.results.len(), 2);
    assert_eq!(response.total_count, 2);
}

#[test]
fn router_applies_non_empty_filters_with_concrete_retrievers_using_local_metadata() {
    let root = temp_fixture_dir("router-concrete-retrievers-non-empty-filters");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    write_rows_json(
        &root,
        "lance/v7/shard_0",
        r#"[
  {"doc_id":"doc-1","text":"rust hybrid search","embedding":[0.9,0.0,0.0],"metadata":{"lang":"rust","published":true}},
  {"doc_id":"doc-2","text":"rust keyword","embedding":[0.8,0.0,0.0],"metadata":{"lang":"go","published":true}}
]"#,
    );

    let router = QueryRouter::new(
        LocalManifestStore::new(&root),
        StubEmbeddingGenerator::success_for_query("rust", vec![0.1, 0.2, 0.3]),
        KeywordSearcher::new(LocalManifestStore::new(&root), &root),
        VectorSearcher::new(LocalManifestStore::new(&root), &root),
    );
    let request = SearchRequest {
        query: "rust".into(),
        top_k: 2,
        filters: Some(HashMap::from([(
            "lang".into(),
            FilterValue::StringEquals("rust".into()),
        )])),
        include_metadata: true,
    };

    let response = router.search(&request).unwrap();

    assert_eq!(response.index_version, 7);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.total_count, 1);
    assert_eq!(response.results[0].doc_id, "doc-1");
    assert_eq!(
        response.results[0].metadata.as_ref().unwrap()["lang"],
        json!("rust")
    );
}

#[test]
fn router_applies_non_empty_filters_with_keyword_only_fallback_using_concrete_metadata() {
    let root = temp_fixture_dir("router-keyword-fallback-non-empty-filters");
    write_fixture(&root, INDEX_HEAD_KEY, &sample_head_json(7));
    write_fixture(&root, &version_manifest_key(7), &sample_manifest_json(7));
    write_index_with_metadata(
        &root,
        "index/v7/shard_0",
        &[
            (
                "doc-1",
                "rust hybrid search",
                json!({"lang":"rust","published":true}),
            ),
            (
                "doc-2",
                "rust keyword",
                json!({"lang":"go","published":true}),
            ),
        ],
    );
    write_rows_json(
        &root,
        "lance/v7/shard_0",
        r#"[
  {"doc_id":"doc-1","text":"rust hybrid search","embedding":[0.9,0.0,0.0]},
  {"doc_id":"doc-2","text":"rust keyword","embedding":[0.8,0.0,0.0]}
]"#,
    );

    let router = QueryRouter::new(
        LocalManifestStore::new(&root),
        StubEmbeddingGenerator::sequence(
            "rust",
            vec![
                Err(EmbeddingError::Generation {
                    message: "embedding unavailable".into(),
                }),
                Err(EmbeddingError::Generation {
                    message: "embedding unavailable".into(),
                }),
            ],
        ),
        KeywordSearcher::new(LocalManifestStore::new(&root), &root),
        VectorSearcher::new(LocalManifestStore::new(&root), &root),
    );
    let request = SearchRequest {
        query: "rust".into(),
        top_k: 2,
        filters: Some(HashMap::from([(
            "lang".into(),
            FilterValue::StringEquals("rust".into()),
        )])),
        include_metadata: true,
    };

    let response = router.search(&request).unwrap();

    assert_eq!(response.results.len(), 1);
    assert_eq!(response.total_count, 1);
    assert_eq!(response.results[0].doc_id, "doc-1");
    assert_eq!(
        response.results[0].metadata.as_ref().unwrap()["lang"],
        json!("rust")
    );
}

#[test]
fn router_applies_exact_match_filters_before_returning_results() {
    let start = Arc::new(Instant::now());
    let manifest_store = StubManifestStore::new(11);
    let generator = StubEmbeddingGenerator::success(vec![0.1, 0.2, 0.3]);
    let keyword = StubKeywordRetriever::new(
        vec![SearchResult {
            doc_id: "doc-1".into(),
            score: 4.0,
            text: "rust book".into(),
            metadata: Some(HashMap::from([
                ("lang".into(), json!("rust")),
                ("published".into(), json!(true)),
            ])),
            source: SearchSource::Keyword,
        }],
        Duration::from_millis(0),
        start.clone(),
        11,
    );
    let vector = StubVectorRetriever::new(
        vec![
            SearchResult {
                doc_id: "doc-2".into(),
                score: 0.9,
                text: "go book".into(),
                metadata: Some(HashMap::from([
                    ("lang".into(), json!("go")),
                    ("published".into(), json!(true)),
                ])),
                source: SearchSource::Vector,
            },
            SearchResult {
                doc_id: "doc-3".into(),
                score: 0.8,
                text: "draft rust notes".into(),
                metadata: Some(HashMap::from([
                    ("lang".into(), json!("rust")),
                    ("published".into(), json!(false)),
                ])),
                source: SearchSource::Vector,
            },
        ],
        Duration::from_millis(0),
        start,
        11,
    );
    let router = QueryRouter::new(manifest_store, generator, keyword, vector);
    let request = SearchRequest {
        filters: Some(HashMap::from([
            ("lang".into(), FilterValue::StringEquals("rust".into())),
            ("published".into(), FilterValue::BoolEquals(true)),
        ])),
        include_metadata: true,
        ..sample_request()
    };

    let response = router.search(&request).unwrap();

    assert_eq!(response.total_count, 1);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].doc_id, "doc-1");
    assert_eq!(
        response.results[0].metadata.as_ref().unwrap()["lang"],
        json!("rust")
    );
}

#[test]
fn router_returns_search_error_when_parallel_retriever_panics() {
    let start = Arc::new(Instant::now());
    let router = QueryRouter::new(
        StubManifestStore::new(21),
        StubEmbeddingGenerator::success(vec![0.1, 0.2, 0.3]),
        PanickingKeywordRetriever,
        StubVectorRetriever::new(vec![], Duration::from_millis(0), start, 21),
    );

    let error = router.search(&sample_request()).unwrap_err();

    assert!(matches!(error, SearchError::Execution { .. }));
    assert!(error.to_string().contains("panicked"));
}

#[test]
fn router_rejects_invalid_retriever_results_before_ranking() {
    let start = Arc::new(Instant::now());
    let router = QueryRouter::new(
        StubManifestStore::new(22),
        StubEmbeddingGenerator::success(vec![0.1, 0.2, 0.3]),
        StubKeywordRetriever::new(
            vec![SearchResult {
                doc_id: String::new(),
                score: 2.0,
                text: "broken keyword result".into(),
                metadata: None,
                source: SearchSource::Keyword,
            }],
            Duration::from_millis(0),
            start.clone(),
            22,
        ),
        StubVectorRetriever::new(
            vec![sample_result("doc-2", 0.8, SearchSource::Vector)],
            Duration::from_millis(0),
            start,
            22,
        ),
    );

    let error = router.search(&sample_request()).unwrap_err();

    assert!(matches!(error, SearchError::Validation(_)));
    assert!(error.to_string().contains("doc_id") || error.to_string().contains("score"));
}
