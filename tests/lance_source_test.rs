//! Task 6: Lance 快照源(pin table version + 确定性全表扫描)。
//!
//! Fixture 使用 lancedb 写 `documents` 表(schema 抄 `src/indexing/builder.rs`),
//! 动态捕获 `table.version()`(不硬编码版本号),验证:
//! - pin 指定版本、确定性 doc_id 升序、复用已存 512 维 embeddings(零重嵌);
//! - pin 的版本忽略后续写入;
//! - 缺失 embedding / 维度不符 / metadata 非合法 JSON object 均整体 fail。

use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{
    FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, RecordBatchReader,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use ltsearch::index::{load_lance_snapshot, EmbeddingProfile, LanceStaticSourceConfig};
use ltsearch::models::CorpusType;
use tempfile::TempDir;

/// 一行 fixture 数据。`embedding == None` 表示该行 embedding 列为 null。
struct FixtureRow {
    doc_id: &'static str,
    text: &'static str,
    /// 原样写入 metadata 列的字符串(允许非法 JSON 以覆盖 malformed 用例)。
    metadata: String,
    embedding: Option<Vec<f32>>,
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
        rows.iter().map(|r| {
            r.embedding
                .as_ref()
                .map(|e| e.iter().copied().map(Some).collect::<Vec<_>>())
        }),
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

/// 建 `documents` 表并写入初始行,返回打开的 `Table`(供 add / version 使用)。
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

/// 决定性 512 维向量:每个 doc 用不同的 base 偏移,保证逐位可比。
fn embedding_for(base: f32) -> Vec<f32> {
    (0..512).map(|i| base + (i as f32) * 0.0009765625).collect()
}

fn profile(dim: u32) -> EmbeddingProfile {
    EmbeddingProfile {
        model_id: "jina-embeddings-v2".to_string(),
        dim,
    }
}

fn config(
    dataset_path: &str,
    table_version: u64,
    embedding_profile: EmbeddingProfile,
) -> LanceStaticSourceConfig {
    LanceStaticSourceConfig {
        dataset_path: dataset_path.to_string(),
        table_version,
        corpus_type: CorpusType::Legal,
        embedding_profile,
    }
}

#[tokio::test]
async fn lance_source_reads_pinned_version_and_reuses_embeddings() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_str().unwrap();

    // 乱序 doc_id;真实、各不相同的 512 维向量。
    let emb_c = embedding_for(0.1);
    let emb_a = embedding_for(0.5);
    let emb_b = embedding_for(0.9);
    let rows = vec![
        FixtureRow {
            doc_id: "doc-c",
            text: "gamma",
            metadata: r#"{"title":"C"}"#.to_string(),
            embedding: Some(emb_c.clone()),
        },
        FixtureRow {
            doc_id: "doc-a",
            text: "alpha",
            metadata: r#"{"title":"A"}"#.to_string(),
            embedding: Some(emb_a.clone()),
        },
        FixtureRow {
            doc_id: "doc-b",
            text: "beta",
            metadata: r#"{"title":"B"}"#.to_string(),
            embedding: Some(emb_b.clone()),
        },
    ];

    let table = create_documents_table(path, 512, &rows).await;
    let version = table.version().await.unwrap();

    let snapshot = load_lance_snapshot(&config(path, version, profile(512)))
        .await
        .expect("load must succeed");

    assert_eq!(snapshot.row_count, 3);
    assert_eq!(snapshot.table_version, version);

    // chunks 按 doc_id 升序。
    let ids: Vec<&str> = snapshot.chunks.iter().map(|c| c.doc_id.as_str()).collect();
    assert_eq!(ids, vec!["doc-a", "doc-b", "doc-c"]);

    // embeddings 与 chunks 平行,且与写入的逐位相等(证明零重嵌)。
    assert_eq!(snapshot.embeddings.len(), 3);
    assert_eq!(snapshot.embeddings[0], emb_a);
    assert_eq!(snapshot.embeddings[1], emb_b);
    assert_eq!(snapshot.embeddings[2], emb_c);

    // corpus_type 全部取 cfg。
    assert!(snapshot
        .chunks
        .iter()
        .all(|c| c.corpus_type == CorpusType::Legal));
}

#[tokio::test]
async fn lance_source_pins_version_and_ignores_later_writes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_str().unwrap();

    let rows = vec![
        FixtureRow {
            doc_id: "doc-a",
            text: "alpha",
            metadata: "{}".to_string(),
            embedding: Some(embedding_for(0.1)),
        },
        FixtureRow {
            doc_id: "doc-b",
            text: "beta",
            metadata: "{}".to_string(),
            embedding: Some(embedding_for(0.2)),
        },
        FixtureRow {
            doc_id: "doc-c",
            text: "gamma",
            metadata: "{}".to_string(),
            embedding: Some(embedding_for(0.3)),
        },
    ];

    let table = create_documents_table(path, 512, &rows).await;
    let v_old = table.version().await.unwrap();

    // 追加第 4 行,版本推进。
    let schema = make_schema(512);
    let extra = vec![FixtureRow {
        doc_id: "doc-d",
        text: "delta",
        metadata: "{}".to_string(),
        embedding: Some(embedding_for(0.4)),
    }];
    let batch = make_batch(schema.clone(), 512, &extra);
    let batches: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
        vec![Ok(batch)].into_iter(),
        schema,
    ));
    table.add(batches).execute().await.unwrap();
    let v_new = table.version().await.unwrap();
    assert!(v_new > v_old, "add must advance version");

    // 用旧版本加载,只应见到原始 3 行。
    let snapshot = load_lance_snapshot(&config(path, v_old, profile(512)))
        .await
        .expect("load pinned version must succeed");

    assert_eq!(snapshot.row_count, 3);
    let ids: Vec<&str> = snapshot.chunks.iter().map(|c| c.doc_id.as_str()).collect();
    assert_eq!(ids, vec!["doc-a", "doc-b", "doc-c"]);
    assert_eq!(snapshot.table_version, v_old);
}

#[tokio::test]
async fn lance_source_rejects_missing_embedding_row() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_str().unwrap();

    let rows = vec![
        FixtureRow {
            doc_id: "doc-a",
            text: "alpha",
            metadata: "{}".to_string(),
            embedding: Some(embedding_for(0.1)),
        },
        FixtureRow {
            doc_id: "doc-b",
            text: "beta",
            metadata: "{}".to_string(),
            embedding: None, // null embedding
        },
    ];

    let table = create_documents_table(path, 512, &rows).await;
    let version = table.version().await.unwrap();

    let result = load_lance_snapshot(&config(path, version, profile(512))).await;
    assert!(result.is_err(), "null embedding row must fail the build");
}

#[tokio::test]
async fn lance_source_rejects_wrong_dim() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_str().unwrap();

    // 列写 256 维,但 profile.dim=512 → 维度不符。
    let rows = vec![FixtureRow {
        doc_id: "doc-a",
        text: "alpha",
        metadata: "{}".to_string(),
        embedding: Some((0..256).map(|i| i as f32).collect()),
    }];

    let table = create_documents_table(path, 256, &rows).await;
    let version = table.version().await.unwrap();

    let result = load_lance_snapshot(&config(path, version, profile(512))).await;
    assert!(
        result.is_err(),
        "embedding dim mismatch must fail the build"
    );
}

#[tokio::test]
async fn lance_source_rejects_malformed_metadata_json() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_str().unwrap();

    let rows = vec![FixtureRow {
        doc_id: "doc-a",
        text: "alpha",
        metadata: "not json".to_string(), // 非合法 JSON
        embedding: Some(embedding_for(0.1)),
    }];

    let table = create_documents_table(path, 512, &rows).await;
    let version = table.version().await.unwrap();

    let result = load_lance_snapshot(&config(path, version, profile(512))).await;
    assert!(
        result.is_err(),
        "malformed metadata JSON must fail the build"
    );
}
