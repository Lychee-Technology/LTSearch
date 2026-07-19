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
use arrow_array::{
    FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, RecordBatchReader,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use ltsearch::index::{
    encode_vector, CentroidTable, MetaRecord, ProjectionMatrix, TurboHeader, TurboRecord512,
    META_RECORD_SIZE,
};
use ltsearch::storage::{static_release_dir_key, version_manifest_key};
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
            let batches: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
                vec![Ok(batch)].into_iter(),
                schema,
            ));

            conn.create_table("documents", batches)
                .execute()
                .await
                .unwrap();
        });
    })
    .join()
    .unwrap();
}

/// TurboQuant 静态 release fixture 的单文档描述。
pub struct StaticFixtureDoc<'a> {
    pub doc_id: u64,
    pub corpus_type: u8,
    pub text: &'a str,
    pub embedding: Vec<f32>,
}

/// 把长度 ≤512 的前缀补零成 512 维向量：静态索引固定按 512 维编码。
pub fn padded_embedding(prefix: &[f32]) -> Vec<f32> {
    let mut embedding = vec![0.0; 512];
    embedding[..prefix.len()].copy_from_slice(prefix);
    embedding
}

fn centroid_table(dim: u32, centroids_per_dim: u32, values: &[f32]) -> CentroidTable {
    let mut bytes = Vec::with_capacity(8 + values.len() * 4);
    bytes.extend_from_slice(&dim.to_le_bytes());
    bytes.extend_from_slice(&centroids_per_dim.to_le_bytes());
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    CentroidTable::from_bytes(&bytes).unwrap()
}

fn identity_projection(dim: usize) -> ProjectionMatrix {
    let mut rows = Vec::with_capacity(dim);
    for row_index in 0..dim {
        let mut row = vec![0.0; dim];
        row[row_index] = 1.0;
        rows.push(row);
    }
    ProjectionMatrix::from_rows(rows)
}

/// 写出一个内容寻址的静态 release fixture 到 `<root>/static/releases/<release_id>/`
/// （与 [`static_release_dir_key`] 布局一致），供查询侧按指针装载 TurboQuant 静态
/// 索引。调用方另需种 `static/_head` 指针指向同一 `release_id`。
pub fn write_static_release_fixture(root: &Path, release_id: &str, docs: &[StaticFixtureDoc<'_>]) {
    let static_dir = root.join(static_release_dir_key(release_id));
    fs::create_dir_all(&static_dir).unwrap();

    let dim = 512;
    let mut centroid_values = Vec::with_capacity(dim as usize * 4);
    for _ in 0..dim {
        centroid_values.extend_from_slice(&[-1.0, 0.0, 1.0, 2.0]);
    }
    let centroids = centroid_table(dim, 4, &centroid_values);
    let projection = identity_projection(dim as usize);
    let header = TurboHeader::new(dim, docs.len() as u64);

    let mut bin_data = header.to_bytes();
    let mut meta_data = Vec::new();
    let mut text_blob = Vec::new();

    for doc in docs {
        let encoded = encode_vector(&doc.embedding, &centroids, &projection).unwrap();
        let record = TurboRecord512 {
            doc_id: doc.doc_id,
            idx: encoded.idx.clone().try_into().unwrap(),
            qjl: encoded.qjl.clone().try_into().unwrap(),
            gamma: encoded.gamma,
            _reserved: [0; 4],
        };
        let record_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &record as *const TurboRecord512 as *const u8,
                std::mem::size_of::<TurboRecord512>(),
            )
        };
        bin_data.extend_from_slice(record_bytes);

        let text_offset = text_blob.len() as u64;
        text_blob.extend_from_slice(doc.text.as_bytes());
        let meta = MetaRecord {
            doc_id: doc.doc_id,
            corpus_type: doc.corpus_type,
            _pad: [0; 7],
            title_offset: 0,
            title_len: 0,
            text_offset,
            text_len: doc.text.len() as u32,
        };
        let meta_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(&meta as *const MetaRecord as *const u8, META_RECORD_SIZE)
        };
        meta_data.extend_from_slice(meta_bytes);
    }

    fs::write(static_dir.join("turbo_static.bin"), &bin_data).unwrap();
    fs::write(static_dir.join("turbo_static_meta.bin"), &meta_data).unwrap();
    fs::write(static_dir.join("turbo_static_text.bin"), &text_blob).unwrap();
    fs::write(static_dir.join("turbo_static_title.bin"), []).unwrap();
    fs::write(static_dir.join("centroids.bin"), centroids.to_bytes()).unwrap();
    fs::write(static_dir.join("projection.bin"), projection.to_bytes()).unwrap();
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
