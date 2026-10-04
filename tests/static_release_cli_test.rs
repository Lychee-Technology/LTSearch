//! Task 7: `run_static_build` CLI 重接线——从 pin 版本的 Lance 快照源产出 v3 release;
//! 配置带 `"release_format": "v4"` 时产出 v4 release(#166)。
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
use ltsearch::index::{MmapIndex, V3_RELEASE_OUTPUT_FILES, V4_RELEASE_OUTPUT_FILES};
use ltsearch::indexing::verify_release_dir;
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

/// 建两行的 Lance fixture 并写出 static-build 配置;`release_format` 为 `None` 时配置里
/// 不带该字段(即 #166 之前的配置原样)。返回 (config 路径, 输出目录)。
async fn write_fixture_and_config(dir: &TempDir, release_format: Option<&str>) -> (String, String) {
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

    let mut cfg = serde_json::json!({
        "dataset_path": dataset_path,
        "table_version": version,
        "corpus_type": "legal",
        "embedding_profile": { "model_id": "jina-v5-nano/512", "dim": 512 }
    });
    if let Some(release_format) = release_format {
        cfg["release_format"] = serde_json::json!(release_format);
    }
    let cfg_path = dir.path().join("config.json");
    std::fs::write(&cfg_path, cfg.to_string()).unwrap();
    let cfg_path = cfg_path.to_str().unwrap().to_string();

    (cfg_path, out_dir)
}

/// 目录下的文件名,升序。
fn file_names(dir: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn run_static_build_builds_v3_release_from_lance_dataset() {
    let dir = TempDir::new().unwrap();
    // 不带 release_format:缺省格式仍是 v3(改缺省属于 #169)。
    let (cfg_path, out_dir) = write_fixture_and_config(&dir, None).await;

    let summary = run_static_build(["--config", &cfg_path, "--output", &out_dir])
        .await
        .expect("static build must succeed");

    // 输出目录含全部 10 个文件(9 .bin + release_manifest.json)。
    let mut expected: Vec<&str> = V3_RELEASE_OUTPUT_FILES.to_vec();
    expected.push("release_manifest.json");
    expected.sort_unstable();
    assert_eq!(file_names(&out_dir), expected);
    assert_eq!(
        expected.len(),
        10,
        "expected 9 .bin + release_manifest.json"
    );
    let manifest_path = std::path::Path::new(&out_dir).join("release_manifest.json");

    // 摘要必须携带非空、且与 manifest 一致的 release_id。
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let release_id = manifest["release_id"].as_str().unwrap();
    assert!(!release_id.is_empty(), "release_id must be non-empty");
    assert!(
        summary.contains(release_id),
        "summary must contain release_id {release_id}: {summary}"
    );
    assert!(summary.contains("format v3"), "summary: {summary}");
    assert_eq!(manifest["turbo_version"], 3);

    // 产物是可加载的 v3 image。
    let index = MmapIndex::load(std::path::Path::new(&out_dir)).expect("v3 image must load");
    assert_eq!(index.version(), 3);
    assert_eq!(index.record_count(), 2);
}

#[tokio::test]
async fn run_static_build_builds_v4_release_when_the_config_selects_it() {
    let dir = TempDir::new().unwrap();
    let (cfg_path, out_dir) = write_fixture_and_config(&dir, Some("v4")).await;

    let summary = run_static_build(["--config", &cfg_path, "--output", &out_dir])
        .await
        .expect("static build must succeed");

    // 输出目录含全部 11 个文件(10 .bin + release_manifest.json)。
    let mut expected: Vec<&str> = V4_RELEASE_OUTPUT_FILES.to_vec();
    expected.push("release_manifest.json");
    expected.sort_unstable();
    assert_eq!(file_names(&out_dir), expected);
    assert_eq!(
        expected.len(),
        11,
        "expected 10 .bin + release_manifest.json"
    );

    // 产物通过 activate 用的同一套校验,manifest 记的是 v4 与完整 codec 配置。
    let manifest = verify_release_dir(
        std::path::Path::new(&out_dir),
        Some("jina-v5-nano/512"),
        Some(512),
    )
    .expect("v4 release must verify");
    assert_eq!(manifest.turbo_version, 4);
    assert!(
        summary.contains(&manifest.release_id),
        "summary must contain release_id {}: {summary}",
        manifest.release_id
    );
    assert!(summary.contains("format v4"), "summary: {summary}");

    // 产物是可加载的 v4 image。
    let index = MmapIndex::load(std::path::Path::new(&out_dir)).expect("v4 image must load");
    assert_eq!(index.version(), 4);
    assert_eq!(index.record_count(), 2);
}

#[tokio::test]
async fn run_static_build_rejects_an_unknown_release_format() {
    let dir = TempDir::new().unwrap();
    let (cfg_path, out_dir) = write_fixture_and_config(&dir, Some("v5")).await;

    let error = run_static_build(["--config", &cfg_path, "--output", &out_dir])
        .await
        .expect_err("an unknown release_format must not fall back to a default")
        .to_string();
    assert!(
        error.contains(r#"release_format must be "v3" or "v4", got "v5""#),
        "{error}"
    );
    // 配置在读数据源之前就被拒绝,什么都没写出。
    assert!(!std::path::Path::new(&out_dir).exists());
}
