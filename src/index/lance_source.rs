//! Lance 快照源:从独立的 static Lance 数据集读取,喂给 `StaticReleaseBuilder`。
//!
//! 关键约束:
//! - **pin 一个 Lance table version**(`checkout`),后续写入不可见 —— 构建可复现;
//! - **确定性全表扫描**:不带 `nearest_to` 的 plain 扫描,收集后由本模块按
//!   `doc_id` 升序排序(不信任 Lance fragment 顺序);
//! - **复用已存的 512 维 embeddings,绝不重嵌**:直接从 `embedding` 列解码。
//!
//! 无 feature 门控 —— lancedb 在 local 图中 AWS-free,本模块不 import 任何 aws crate。

use std::collections::HashMap;

use arrow_array::{Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::query::ExecutableQuery;
use serde::Deserialize;
use serde_json::Value;

use super::EmbeddingProfile;
use crate::error::IndexError;
use crate::index::StaticChunk;
use crate::models::CorpusType;

const LANCE_TABLE_NAME: &str = "documents";
const DOC_ID_COLUMN: &str = "doc_id";
const TEXT_COLUMN: &str = "text";
const METADATA_COLUMN: &str = "metadata";
const EMBEDDING_COLUMN: &str = "embedding";

/// 仅支持 512 维 typed turbo 布局(与 `StaticReleaseBuilder` 对齐)。
const SUPPORTED_DIM: u32 = 512;

/// 一个独立 static Lance 数据集快照源的配置。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LanceStaticSourceConfig {
    pub dataset_path: String,
    pub table_version: u64,
    pub corpus_type: CorpusType,
    pub embedding_profile: EmbeddingProfile,
}

/// 从 pin 版本全表扫描得到的、已确定性排序的快照。
pub struct LanceSnapshot {
    /// 已按 `doc_id` 字符串升序排序。
    pub chunks: Vec<StaticChunk>,
    /// 与 `chunks` 平行的、复用自 Lance 的 embeddings(零重嵌)。
    pub embeddings: Vec<Vec<f32>>,
    /// 实际 checkout 到的版本(写 provenance)。
    pub table_version: u64,
    pub row_count: u64,
}

fn op(message: impl Into<String>) -> IndexError {
    IndexError::Operation {
        message: message.into(),
    }
}

/// pin `cfg.table_version` → 确定性全表扫描 → 复用 embeddings → 按 doc_id 升序。
///
/// 任一行校验失败(doc_id/text 空、metadata 非合法 JSON object、embedding 缺失 /
/// 维度不符 / 含非有限值)即 fail 整个 build。
pub async fn load_lance_snapshot(
    cfg: &LanceStaticSourceConfig,
) -> Result<LanceSnapshot, IndexError> {
    // profile 维度必须是本模块支持的 512(否则 typed 布局无从谈起)。
    if cfg.embedding_profile.dim != SUPPORTED_DIM {
        return Err(op(format!(
            "lance snapshot only supports {}-dim embeddings, profile declared {}",
            SUPPORTED_DIM, cfg.embedding_profile.dim
        )));
    }
    let expected_dim = cfg.embedding_profile.dim as usize;

    let conn = lancedb::connect(&cfg.dataset_path)
        .execute()
        .await
        .map_err(|source| {
            op(format!(
                "failed to connect Lance dataset at {}: {source}",
                cfg.dataset_path
            ))
        })?;

    let table = conn
        .open_table(LANCE_TABLE_NAME)
        .execute()
        .await
        .map_err(|source| {
            op(format!(
                "failed to open Lance table '{}' at {}: {source}",
                LANCE_TABLE_NAME, cfg.dataset_path
            ))
        })?;

    // Pin 到指定版本;不存在的版本 → checkout 报错即 IndexError。
    table.checkout(cfg.table_version).await.map_err(|source| {
        op(format!(
            "failed to checkout Lance table version {} at {}: {source}",
            cfg.table_version, cfg.dataset_path
        ))
    })?;

    // Plain 全表扫描(无 nearest_to),流式收集所有 batch。
    let batches: Vec<RecordBatch> = table
        .query()
        .execute()
        .await
        .map_err(|source| {
            op(format!(
                "failed to execute full-table scan on Lance table at {}: {source}",
                cfg.dataset_path
            ))
        })?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|source| {
            op(format!(
                "failed to collect Lance scan batches at {}: {source}",
                cfg.dataset_path
            ))
        })?;

    let mut rows: Vec<(StaticChunk, Vec<f32>)> = Vec::new();
    for batch in &batches {
        let doc_ids = downcast_string(batch, DOC_ID_COLUMN)?;
        let texts = downcast_string(batch, TEXT_COLUMN)?;
        let metadata = downcast_string(batch, METADATA_COLUMN)?;
        let embeddings = downcast_fixed_size_list(batch, EMBEDDING_COLUMN)?;

        for row in 0..batch.num_rows() {
            let doc_id = doc_ids.value(row).to_string();
            if doc_id.is_empty() {
                return Err(op("Lance row has empty doc_id"));
            }

            let text = texts.value(row).to_string();
            if text.is_empty() {
                return Err(op(format!("Lance row {doc_id} has empty text")));
            }

            let parsed_metadata = parse_metadata_object(metadata.value(row), &doc_id)?;

            let embedding = decode_embedding(embeddings, row, expected_dim, &doc_id)?;

            rows.push((
                StaticChunk {
                    doc_id,
                    text,
                    metadata: parsed_metadata,
                    corpus_type: cfg.corpus_type.clone(),
                },
                embedding,
            ));
        }
    }

    // 确定性由本模块拥有:按 doc_id 字符串升序排序(不信任 fragment 顺序)。
    rows.sort_by(|a, b| a.0.doc_id.cmp(&b.0.doc_id));

    let row_count = rows.len() as u64;
    let mut chunks = Vec::with_capacity(rows.len());
    let mut embeddings = Vec::with_capacity(rows.len());
    for (chunk, embedding) in rows {
        chunks.push(chunk);
        embeddings.push(embedding);
    }

    // 读回实际 checkout 到的版本(pin 后稳定),写入 provenance。
    let table_version = table.version().await.map_err(|source| {
        op(format!(
            "failed to read Lance table version at {}: {source}",
            cfg.dataset_path
        ))
    })?;

    Ok(LanceSnapshot {
        chunks,
        embeddings,
        table_version,
        row_count,
    })
}

fn downcast_string<'a>(
    batch: &'a RecordBatch,
    column: &str,
) -> Result<&'a StringArray, IndexError> {
    batch
        .column_by_name(column)
        .ok_or_else(|| op(format!("Lance scan missing '{column}' column")))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| op(format!("Lance column '{column}' is not Utf8")))
}

fn downcast_fixed_size_list<'a>(
    batch: &'a RecordBatch,
    column: &str,
) -> Result<&'a FixedSizeListArray, IndexError> {
    batch
        .column_by_name(column)
        .ok_or_else(|| op(format!("Lance scan missing '{column}' column")))?
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .ok_or_else(|| op(format!("Lance column '{column}' is not a FixedSizeList")))
}

fn parse_metadata_object(raw: &str, doc_id: &str) -> Result<HashMap<String, Value>, IndexError> {
    let value = serde_json::from_str::<Value>(raw).map_err(|source| {
        op(format!(
            "Lance row {doc_id} has non-JSON metadata: {source}"
        ))
    })?;
    match value {
        Value::Object(map) => Ok(map.into_iter().collect()),
        _ => Err(op(format!(
            "Lance row {doc_id} metadata is not a JSON object"
        ))),
    }
}

fn decode_embedding(
    embeddings: &FixedSizeListArray,
    row: usize,
    expected_dim: usize,
    doc_id: &str,
) -> Result<Vec<f32>, IndexError> {
    if embeddings.is_null(row) {
        return Err(op(format!("Lance row {doc_id} has a null embedding")));
    }

    let values = embeddings.value(row);
    let floats = values
        .as_any()
        .downcast_ref::<Float32Array>()
        .ok_or_else(|| op(format!("Lance row {doc_id} embedding is not Float32")))?;

    if floats.len() != expected_dim {
        return Err(op(format!(
            "Lance row {doc_id} embedding dim {} does not match expected {}",
            floats.len(),
            expected_dim
        )));
    }

    let embedding = floats.values().to_vec();
    if embedding.iter().any(|value| !value.is_finite()) {
        return Err(op(format!(
            "Lance row {doc_id} embedding contains a non-finite value"
        )));
    }

    Ok(embedding)
}
