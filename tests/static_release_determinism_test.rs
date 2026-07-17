//! Task 8: build-twice 逐字节决定性验收。
//!
//! 从同一 pinned Lance 快照 + 同一 config,`run_static_build` 两次到**两个不同的
//! 输出目录**,断言全部 10 个产物(9 `.bin` + `release_manifest.json`)逐字节相同,
//! 且两份 manifest 的 `release_id` 相同。
//!
//! **HashMap 序列化陷阱**:fixture 里至少一行的 metadata 用**多个键**,并特意用
//! **两种不同的插入/书写顺序**构造。metadata 以 JSON 字符串存进 Lance,读回时被
//! `serde_json` 解析成 `HashMap`,其迭代顺序受 per-run 随机 `RandomState` 影响。
//! 若 v3 writer 直接序列化该 HashMap,两次 build(乃至同一 build 内不同行)就会
//! 产生不同字节;唯有 `canonical_metadata_json` 的 BTreeMap 规范化能让产物稳定。
//! 本测试正是对这条决定性缝的端到端把关。

// `ltsearch::app` 仅在 local profile 下编译;aws/lambda profile 的
// `clippy --all-targets` 也会编译本 test crate,须整文件门控。
#![cfg(feature = "local")]

use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{
    FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, RecordBatchReader,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use ltsearch::app::run_static_build;
use tempfile::TempDir;

struct FixtureRow {
    doc_id: &'static str,
    text: &'static str,
    metadata: String,
    embedding: Vec<f32>,
}

fn make_schema(dim: i32) -> Arc<ArrowSchema> {
    Arc::new(ArrowSchema::new(vec![
        Field::new("doc_id", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("metadata", DataType::Utf8, false),
        Field::new("timestamp", DataType::Int64, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            true,
        ),
    ]))
}

fn make_batch(schema: Arc<ArrowSchema>, dim: i32, rows: &[FixtureRow]) -> RecordBatch {
    let doc_ids = StringArray::from(rows.iter().map(|r| r.doc_id).collect::<Vec<_>>());
    let texts = StringArray::from(rows.iter().map(|r| r.text).collect::<Vec<_>>());
    let metadata = StringArray::from(rows.iter().map(|r| r.metadata.as_str()).collect::<Vec<_>>());
    let timestamps = Int64Array::from(vec![0_i64; rows.len()]);
    let embeddings = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter()
            .map(|r| Some(r.embedding.iter().copied().map(Some).collect::<Vec<_>>())),
        dim,
    );

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(doc_ids),
            Arc::new(texts),
            Arc::new(metadata),
            Arc::new(timestamps),
            Arc::new(embeddings),
        ],
    )
    .unwrap()
}

async fn create_documents_table(dataset_path: &str, dim: i32, rows: &[FixtureRow]) {
    let schema = make_schema(dim);
    let batch = make_batch(schema.clone(), dim, rows);
    let batches: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
        vec![Ok(batch)].into_iter(),
        schema,
    ));

    let conn = lancedb::connect(dataset_path).execute().await.unwrap();
    conn.create_table("documents", batches)
        .execute()
        .await
        .unwrap();
}

fn embedding_for(base: f32) -> Vec<f32> {
    (0..512).map(|i| base + (i as f32) * 0.0009765625).collect()
}

#[tokio::test]
async fn static_release_build_twice_is_byte_identical_including_manifest() {
    let dir = TempDir::new().unwrap();
    let dataset_path = dir.path().join("lance");
    let dataset_path = dataset_path.to_str().unwrap().to_string();

    // Two rows with **multi-key** metadata written in **two different key
    // orders**. Once parsed into a HashMap the iteration order is randomized
    // per run, so a naive (non-canonical) writer would emit different bytes on
    // build A vs build B. Canonicalization must flatten this out.
    let rows = vec![
        FixtureRow {
            doc_id: "doc-a",
            text: "alpha",
            metadata: r#"{"title":"A","author":"Ada","year":"2020","lang":"en"}"#.to_string(),
            embedding: embedding_for(0.1),
        },
        FixtureRow {
            doc_id: "doc-b",
            text: "beta",
            // Same set of keys as doc-a but written in a deliberately different
            // order to exercise the HashMap-serialization trap.
            metadata: r#"{"lang":"fr","year":"2021","author":"Bob","title":"B"}"#.to_string(),
            embedding: embedding_for(0.5),
        },
    ];

    create_documents_table(&dataset_path, 512, &rows).await;

    // Pin the table version so both builds read the exact same snapshot.
    let conn = lancedb::connect(&dataset_path).execute().await.unwrap();
    let table = conn.open_table("documents").execute().await.unwrap();
    let version = table.version().await.unwrap();

    let cfg_json = serde_json::json!({
        "dataset_path": dataset_path,
        "table_version": version,
        "corpus_type": "legal",
        "embedding_profile": { "model_id": "jina-v5-nano/512", "dim": 512 }
    })
    .to_string();
    let cfg_path = dir.path().join("config.json");
    std::fs::write(&cfg_path, cfg_json).unwrap();
    let cfg_path = cfg_path.to_str().unwrap().to_string();

    // Two distinct output directories — never the same dir overwritten.
    let out_a = dir.path().join("out_a");
    let out_a = out_a.to_str().unwrap().to_string();
    let out_b = dir.path().join("out_b");
    let out_b = out_b.to_str().unwrap().to_string();

    run_static_build(["--config", &cfg_path, "--output", &out_a])
        .await
        .expect("first static build must succeed");
    run_static_build(["--config", &cfg_path, "--output", &out_b])
        .await
        .expect("second static build must succeed");

    // Enumerate build A's outputs and require exactly 10 files
    // (9 `.bin` + `release_manifest.json`).
    let mut files: Vec<String> = std::fs::read_dir(&out_a)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(
        files.len(),
        10,
        "expected 9 .bin + release_manifest.json, got {files:?}"
    );
    let bin_count = files.iter().filter(|name| name.ends_with(".bin")).count();
    assert_eq!(bin_count, 9, "expected exactly 9 .bin files, got {files:?}");
    assert!(
        files.iter().any(|name| name == "release_manifest.json"),
        "release_manifest.json must be present: {files:?}"
    );

    // Every one of the 10 files must be byte-identical across the two builds.
    for name in &files {
        let bytes_a = std::fs::read(std::path::Path::new(&out_a).join(name)).unwrap();
        let bytes_b = std::fs::read(std::path::Path::new(&out_b).join(name)).unwrap();
        assert_eq!(
            bytes_a, bytes_b,
            "file {name} differs byte-for-byte between build A and build B"
        );
    }

    // And the content-derived release_id must match across builds.
    let manifest_a: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(std::path::Path::new(&out_a).join("release_manifest.json"))
            .unwrap(),
    )
    .unwrap();
    let manifest_b: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(std::path::Path::new(&out_b).join("release_manifest.json"))
            .unwrap(),
    )
    .unwrap();
    let release_id_a = manifest_a["release_id"].as_str().unwrap();
    let release_id_b = manifest_b["release_id"].as_str().unwrap();
    assert!(!release_id_a.is_empty(), "release_id must be non-empty");
    assert_eq!(
        release_id_a, release_id_b,
        "release_id must be identical across builds"
    );
}
