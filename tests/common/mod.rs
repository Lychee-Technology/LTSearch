//! HTTP/query 集成测试共享的磁盘 fixture 构建器。
//!
//! 与 `tests/query_lambda_test.rs` 中的 fixture 搭建方式保持一致：写出
//! `_head`、版本化 manifest、tantivy 关键词索引与 lance 向量分片，供 router
//! 级端到端测试直接驱动 `bootstrap_query_handler_from_env` 的成功路径。
//!
//! 该模块被多个测试二进制以 `mod common;` 各自编译，不同二进制只用到其中一
//! 部分构建器，故整体豁免 `dead_code`。
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arrow_array::types::Float32Type;
use arrow_array::{FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use ltsearch::storage::version_manifest_key;
use serde_json::json;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{Index, IndexWriter};
use tokio::runtime::Runtime;

/// 进程内 env 串行化锁：同一测试二进制内的用例并行执行，读写进程级
/// env 的用例必须持锁，避免相互污染（与 query_lambda_test.rs 同模式）。
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn temp_fixture_dir(test_name: &str) -> PathBuf {
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

pub fn write_fixture(root: &Path, relative_path: &str, contents: &str) {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

pub fn write_index(root: &Path, relative_path: &str, documents: &[(&str, &str)]) {
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

pub fn write_lance_fixture(root: &Path, relative_path: &str, rows: &[serde_json::Value]) {
    write_lance_fixture_with_dim(root, relative_path, rows, 3);
}

pub fn write_lance_fixture_with_dim(
    root: &Path,
    relative_path: &str,
    rows: &[serde_json::Value],
    embedding_dim: i32,
) {
    let shard_dir = root.join(relative_path);
    fs::create_dir_all(&shard_dir).unwrap();

    let shard_dir_string = shard_dir.to_str().unwrap().to_string();
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("doc_id", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("metadata", DataType::Utf8, false),
        Field::new("timestamp", DataType::Int64, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                embedding_dim,
            ),
            true,
        ),
    ]));

    let doc_ids = StringArray::from(
        rows.iter()
            .map(|row| row["doc_id"].as_str())
            .collect::<Vec<_>>(),
    );
    let texts = StringArray::from(
        rows.iter()
            .map(|row| row["text"].as_str())
            .collect::<Vec<_>>(),
    );
    let metadata = StringArray::from(
        rows.iter()
            .map(|row| serde_json::to_string(row.get("metadata").unwrap_or(&json!({}))).unwrap())
            .collect::<Vec<_>>(),
    );
    let timestamps = Int64Array::from(vec![0_i64; rows.len()]);
    let embeddings = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter().map(|row| {
            row["embedding"].as_array().map(|embedding| {
                embedding
                    .iter()
                    .map(|value| Some(value.as_f64().unwrap() as f32))
                    .collect::<Vec<_>>()
            })
        }),
        embedding_dim,
    );

    // 在独立 OS 线程上建自己的运行时：调用方可能已处于 tokio 运行时
    // （如 `#[tokio::test]`），此时直接 `block_on` 会 panic。
    let arrays: Vec<Arc<dyn arrow_array::Array>> = vec![
        Arc::new(doc_ids),
        Arc::new(texts),
        Arc::new(metadata),
        Arc::new(timestamps),
        Arc::new(embeddings),
    ];
    std::thread::spawn(move || {
        Runtime::new().unwrap().block_on(async move {
            let conn = lancedb::connect(&shard_dir_string).execute().await.unwrap();
            let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
            let batches = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), schema);

            conn.create_table("documents", batches)
                .execute()
                .await
                .unwrap();
        });
    })
    .join()
    .unwrap();
}

pub fn sample_head_json(version_id: u64) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "manifest_path": "{}",
  "updated_at": 1700000005000
}}"#,
        version_manifest_key(version_id)
    )
}

pub fn sample_manifest_json(version_id: u64) -> String {
    sample_manifest_json_with_dim(version_id, 3)
}

pub fn sample_manifest_json_with_dim(version_id: u64, embedding_dim: usize) -> String {
    format!(
        r#"{{
  "version_id": {version_id},
  "created_at": 1700000000000,
  "embedding_dim": {embedding_dim},
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
