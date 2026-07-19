use std::collections::HashMap;
use std::path::PathBuf;

use ltsearch::models::{
    CacheStats, ChunkSource, DeleteResponse, Document, FilterValue, HealthStatus, IndexCache,
    IndexManifest, IngestResponse, SearchRequest, SearchResponse, SearchResult, SearchSource,
    ShardManifest, WalOperation, WalRecord,
};
use serde_json::{json, Value};

fn sample_document() -> Document {
    Document {
        doc_id: "doc-1".into(),
        text: "hello world".into(),
        embedding: Some(vec![0.5; 768]),
        metadata: HashMap::from([("category".into(), Value::String("guide".into()))]),
        timestamp: 1_700_000_000_000,
    }
}

fn sample_index_manifest() -> IndexManifest {
    IndexManifest {
        version_id: 7,
        created_at: 1_700_000_000_000,
        embedding_dim: 768,
        document_count: 5,
        num_shards: 2,
        shards: vec![
            ShardManifest {
                shard_id: 0,
                document_count: 2,
                lance_path: "s3://bucket/lance/v7/shard_0".into(),
                tantivy_path: "s3://bucket/index/v7/shard_0".into(),
            },
            ShardManifest {
                shard_id: 1,
                document_count: 3,
                lance_path: "s3://bucket/lance/v7/shard_1".into(),
                tantivy_path: "s3://bucket/index/v7/shard_1".into(),
            },
        ],
    }
}

#[test]
fn search_request_validates_boundaries() {
    let request = SearchRequest {
        query: "lambda search".into(),
        top_k: 10,
        filters: Some(HashMap::from([
            ("tenant".into(), FilterValue::StringEquals("acme".into())),
            ("published".into(), FilterValue::BoolEquals(true)),
            ("boost".into(), FilterValue::NumberEquals(1.5)),
        ])),
        include_metadata: true,
        corpus_weights: None,
    };

    assert!(request.validate().is_ok());

    let empty_query = SearchRequest {
        query: "".into(),
        ..request.clone()
    };
    assert!(empty_query.validate().is_err());

    let long_query = SearchRequest {
        query: "x".repeat(1001),
        ..request.clone()
    };
    assert!(long_query.validate().is_err());

    let zero_top_k = SearchRequest {
        top_k: 0,
        ..request.clone()
    };
    assert!(zero_top_k.validate().is_err());

    let high_top_k = SearchRequest {
        top_k: 101,
        ..request
    };
    assert!(high_top_k.validate().is_err());
}

#[test]
fn search_models_round_trip_through_serde() {
    let result = SearchResult {
        doc_id: "doc-1".into(),
        score: 0.0,
        text: "hello".into(),
        metadata: Some(HashMap::from([("lang".into(), json!("en"))])),
        source: SearchSource::Hybrid,
        chunk_source: ChunkSource::Dynamic,
        corpus_type: None,
        citation: None,
    };
    assert!(result.validate().is_ok());

    let response = SearchResponse {
        static_chunks: vec![result.clone()],
        static_count: 1,
        dynamic_chunks: vec![result],
        dynamic_count: 1,
        latency_ms: 12,
        index_version: 7,
        static_release_id: None,
    };
    assert!(response.validate(10).is_ok());

    let encoded = serde_json::to_string(&response).unwrap();
    let decoded: SearchResponse = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.static_count, 1);
    assert_eq!(decoded.dynamic_count, 1);
    assert_eq!(decoded.index_version, 7);
    assert_eq!(decoded.dynamic_chunks[0].source, SearchSource::Hybrid);
    assert_eq!(
        decoded.dynamic_chunks[0].metadata.as_ref().unwrap()["lang"],
        json!("en")
    );

    let too_many_results = SearchResponse {
        static_chunks: vec![
            decoded.static_chunks[0].clone(),
            decoded.static_chunks[0].clone(),
        ],
        static_count: 1,
        dynamic_chunks: vec![decoded.dynamic_chunks[0].clone()],
        dynamic_count: 1,
        latency_ms: 1,
        index_version: 7,
        static_release_id: None,
    };
    assert!(too_many_results.validate(10).is_err());

    let exceeds_requested_top_k = SearchResponse {
        static_chunks: vec![],
        static_count: 0,
        dynamic_chunks: vec![
            decoded.dynamic_chunks[0].clone(),
            decoded.dynamic_chunks[0].clone(),
        ],
        dynamic_count: 2,
        latency_ms: 1,
        index_version: 7,
        static_release_id: None,
    };
    assert!(exceeds_requested_top_k.validate(1).is_err());
}

#[test]
fn document_validation_enforces_design_limits() {
    assert!(sample_document().validate().is_ok());

    let missing_id = Document {
        doc_id: "".into(),
        ..sample_document()
    };
    assert!(missing_id.validate().is_err());

    let long_id = Document {
        doc_id: "d".repeat(257),
        ..sample_document()
    };
    assert!(long_id.validate().is_err());

    let missing_text = Document {
        text: "".into(),
        ..sample_document()
    };
    assert!(missing_text.validate().is_err());

    let non_finite_embedding = Document {
        embedding: Some(vec![1.0, f32::NAN]),
        ..sample_document()
    };
    assert!(non_finite_embedding.validate().is_err());

    let wrong_embedding_dim = Document {
        embedding: Some(vec![1.0; 32]),
        ..sample_document()
    };
    assert!(wrong_embedding_dim.validate().is_ok());
    assert!(wrong_embedding_dim.validate_for_embedding_dim(768).is_err());

    let oversized_metadata = Document {
        metadata: HashMap::from([("blob".into(), Value::String("x".repeat(10_001)))]),
        ..sample_document()
    };
    assert!(oversized_metadata.validate().is_err());

    let invalid_timestamp = Document {
        timestamp: 0,
        ..sample_document()
    };
    assert!(invalid_timestamp.validate().is_err());

    let seconds_timestamp = Document {
        timestamp: 1_700_000_000,
        ..sample_document()
    };
    assert!(seconds_timestamp.validate().is_err());
}

#[test]
fn index_manifest_validation_checks_shards_and_counts() {
    let manifest = sample_index_manifest();

    assert!(manifest.validate().is_ok());

    let zero_shards = IndexManifest {
        num_shards: 0,
        ..manifest.clone()
    };
    assert!(zero_shards.validate().is_err());

    let zero_dim = IndexManifest {
        embedding_dim: 0,
        ..manifest.clone()
    };
    assert!(zero_dim.validate().is_err());

    let mismatched_counts = IndexManifest {
        document_count: 4,
        ..manifest.clone()
    };
    assert!(mismatched_counts.validate().is_err());

    let invalid_timestamp = IndexManifest {
        created_at: 0,
        ..manifest.clone()
    };
    assert!(invalid_timestamp.validate().is_err());

    let seconds_timestamp = IndexManifest {
        created_at: 1_700_000_000,
        ..manifest.clone()
    };
    assert!(seconds_timestamp.validate().is_err());

    let malformed_s3_uri = IndexManifest {
        shards: vec![ShardManifest {
            lance_path: "s3:///bad/path".into(),
            ..manifest.shards[0].clone()
        }],
        num_shards: 1,
        document_count: 2,
        ..manifest
    };
    assert!(malformed_s3_uri.validate().is_err());
}

#[test]
fn index_cache_validation_requires_tmp_and_lambda_size_limit() {
    let cache = IndexCache {
        cache_dir: PathBuf::from("/tmp/search-index"),
        max_size_bytes: 10 * 1024 * 1024,
        current_version: Some(7),
    };
    assert!(cache.validate().is_ok());

    let wrong_dir = IndexCache {
        cache_dir: PathBuf::from("/var/cache/search-index"),
        ..cache.clone()
    };
    assert!(wrong_dir.validate().is_err());

    let escaping_dir = IndexCache {
        cache_dir: PathBuf::from("/tmp/../var/cache"),
        ..cache.clone()
    };
    assert!(escaping_dir.validate().is_err());

    let too_large = IndexCache {
        max_size_bytes: 10 * 1024 * 1024 * 1024 + 1,
        ..cache
    };
    assert!(too_large.validate().is_err());
}

#[test]
fn wal_record_validation_enforces_operation_shape() {
    let document = sample_document();

    let upsert = WalRecord {
        event_id: "op-1".into(),
        doc_id: document.doc_id.clone(),
        op: WalOperation::Upsert,
        document: Some(document.clone()),
        timestamp: 1_700_000_000_000,
    };
    assert!(upsert.validate().is_ok());

    let delete = WalRecord {
        event_id: "op-2".into(),
        doc_id: document.doc_id.clone(),
        op: WalOperation::Delete,
        document: None,
        timestamp: 1_700_000_000_000,
    };
    assert!(delete.validate().is_ok());

    let missing_document = WalRecord {
        document: None,
        ..upsert.clone()
    };
    assert!(missing_document.validate().is_err());

    let unexpected_document = WalRecord {
        document: Some(document.clone()),
        ..delete.clone()
    };
    assert!(unexpected_document.validate().is_err());

    let mismatched_doc_id = WalRecord {
        doc_id: "other-doc".into(),
        document: Some(document),
        ..upsert
    };
    assert!(mismatched_doc_id.validate().is_err());

    let seconds_timestamp = WalRecord {
        timestamp: 1_700_000_000,
        ..delete
    };
    assert!(seconds_timestamp.validate().is_err());
}

#[test]
fn boundary_models_support_serde() {
    let status = HealthStatus {
        status: "ok".into(),
        index_version: Some(7),
        cache: Some(CacheStats {
            hit_count: 10,
            miss_count: 2,
            current_version: Some(7),
            bytes_used: 1024,
        }),
    };

    let ingest = IngestResponse {
        accepted_count: 2,
        wal_event_ids: vec!["wal-1".into(), "wal-2".into()],
        batch_id: "batch-1".into(),
    };

    let delete = DeleteResponse {
        accepted_count: 1,
        wal_event_ids: vec!["wal-3".into()],
        batch_id: "batch-2".into(),
    };

    let status_json = serde_json::to_value(status).unwrap();
    let ingest_json = serde_json::to_value(ingest).unwrap();
    let delete_json = serde_json::to_value(delete).unwrap();

    assert_eq!(status_json["status"], json!("ok"));
    assert_eq!(status_json["index_version"], json!(7));
    assert_eq!(ingest_json["accepted_count"], json!(2));
    assert_eq!(ingest_json["wal_event_ids"], json!(["wal-1", "wal-2"]));
    assert_eq!(delete_json["accepted_count"], json!(1));
    assert_eq!(delete_json["batch_id"], json!("batch-2"));
}
