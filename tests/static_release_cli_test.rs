//! Task 7: `run_static_build` CLI 重接线——从 pin 版本的 Lance 快照源产出 v3 release。
//!
//! Fixture 复用 `lance_source_test.rs` 的写法:用 arrow + lancedb 建一个含 512 维
//! `FixedSizeList<Float32,512>` 的 `documents` 表,捕获 `table.version()`,写 config
//! JSON,再驱动 `run_static_build(["--config", .., "--output", ..])`。

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
use ltsearch::index::MmapIndex;
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

async fn create_documents_table(
    dataset_path: &str,
    dim: i32,
    rows: &[FixtureRow],
) -> lancedb::Table {
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
        .unwrap()
}

fn embedding_for(base: f32) -> Vec<f32> {
    (0..512).map(|i| base + (i as f32) * 0.0009765625).collect()
}

#[tokio::test]
async fn run_static_build_builds_v3_release_from_lance_dataset() {
    let dir = TempDir::new().unwrap();
    let dataset_path = dir.path().join("lance");
    let dataset_path = dataset_path.to_str().unwrap().to_string();
    let out_dir = dir.path().join("out");
    let out_dir = out_dir.to_str().unwrap().to_string();

    let rows = vec![
        FixtureRow {
            doc_id: "doc-a",
            text: "alpha",
            metadata: r#"{"title":"A"}"#.to_string(),
            embedding: embedding_for(0.1),
        },
        FixtureRow {
            doc_id: "doc-b",
            text: "beta",
            metadata: r#"{"title":"B"}"#.to_string(),
            embedding: embedding_for(0.5),
        },
    ];

    let table = create_documents_table(&dataset_path, 512, &rows).await;
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

    let summary = run_static_build(["--config", &cfg_path, "--output", &out_dir])
        .await
        .expect("static build must succeed");

    // 输出目录含全部 10 个文件(9 .bin + release_manifest.json)。
    let file_count = std::fs::read_dir(&out_dir).unwrap().count();
    assert_eq!(file_count, 10, "expected 9 .bin + release_manifest.json");
    let manifest_path = std::path::Path::new(&out_dir).join("release_manifest.json");
    assert!(manifest_path.exists());

    // 摘要必须携带非空、且与 manifest 一致的 release_id。
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let release_id = manifest["release_id"].as_str().unwrap();
    assert!(!release_id.is_empty(), "release_id must be non-empty");
    assert!(
        summary.contains(release_id),
        "summary must contain release_id {release_id}: {summary}"
    );

    // 产物是可加载的 v3 image。
    let index = MmapIndex::load(std::path::Path::new(&out_dir)).expect("v3 image must load");
    assert_eq!(index.version(), 3);
    assert_eq!(index.record_count(), 2);
}
