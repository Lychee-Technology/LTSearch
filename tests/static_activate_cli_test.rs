//! Task 5: `run_static_activate` CLI 端到端——先用 `run_static_build` 从 pin 版本的
//! Lance 快照产出一个真实 v3 release,再驱动 `run_static_activate` 验证 → 安装进
//! 受管存储 → CAS 切换 `static/_head` 指针,断言:摘要含 "activated"、受管目录落位、
//! SQLite `static_release_head` 行可经 `StaticReleaseHead::from_json` 解析且 release_id
//! 与 manifest 一致。
//!
//! Fixture 复用 `static_release_cli_test.rs` 的 Lance 建表写法。

// `ltsearch::app` 仅在 local profile 下编译。
#![cfg(feature = "local")]

use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{
    FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, RecordBatchReader,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use ltsearch::app::{run_static_activate, run_static_build};
use ltsearch::indexing::PublishStorage;
use ltsearch::local::{LocalPublishStorage, SqliteDb};
use ltsearch::storage::{StaticReleaseHead, STATIC_HEAD_KEY};
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

/// Builds a real v3 release into `release_dir` via `run_static_build`, reusing the
/// Lance fixture. Returns the built release's `release_id` (read from the manifest).
async fn build_v3_release_via_cli(work: &TempDir, release_dir: &std::path::Path) -> String {
    let dataset_path = work.path().join("lance");
    let dataset_path = dataset_path.to_str().unwrap().to_string();

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

    create_documents_table(&dataset_path, 512, &rows).await;
    let conn = lancedb::connect(&dataset_path).execute().await.unwrap();
    let version = conn
        .open_table("documents")
        .execute()
        .await
        .unwrap()
        .version()
        .await
        .unwrap();

    let cfg_json = serde_json::json!({
        "dataset_path": dataset_path,
        "table_version": version,
        "corpus_type": "legal",
        "embedding_profile": { "model_id": "jina-v5-nano/512", "dim": 512 }
    })
    .to_string();
    let cfg_path = work.path().join("config.json");
    std::fs::write(&cfg_path, cfg_json).unwrap();

    run_static_build([
        "--config",
        cfg_path.to_str().unwrap(),
        "--output",
        release_dir.to_str().unwrap(),
    ])
    .await
    .expect("static build must succeed");

    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(release_dir.join("release_manifest.json")).unwrap(),
    )
    .unwrap();
    manifest["release_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn run_static_activate_installs_and_flips_pointer() {
    let work = TempDir::new().unwrap();
    let root = work.path().join("root");
    let release = work.path().join("release");

    let release_id = build_v3_release_via_cli(&work, &release).await;

    let summary = run_static_activate([
        "--release",
        release.to_str().unwrap(),
        "--root",
        root.to_str().unwrap(),
    ])
    .await
    .expect("static activate must succeed");

    // 摘要携带 activated + release_id。
    assert!(summary.contains("activated"), "summary: {summary}");
    assert!(summary.contains(&release_id), "summary: {summary}");

    // 受管存储落位:<root>/static/releases/<id>/release_manifest.json 存在。
    let installed_manifest = root
        .join("static/releases")
        .join(&release_id)
        .join("release_manifest.json");
    assert!(
        installed_manifest.exists(),
        "installed manifest must exist at {}",
        installed_manifest.display()
    );

    // SQLite static_release_head 行存在、可解析,且 release_id 与 manifest 一致。
    let db = SqliteDb::open(root.join("ltsearch.db")).unwrap();
    let storage = LocalPublishStorage::new(db, &root);
    let head_object = storage
        .read(STATIC_HEAD_KEY)
        .await
        .unwrap()
        .expect("static/_head pointer row must exist after activation");
    let head = StaticReleaseHead::from_json(&head_object.bytes)
        .expect("stored static/_head must parse");
    assert_eq!(head.release_id, release_id);
}
