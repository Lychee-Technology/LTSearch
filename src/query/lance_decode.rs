//! Decoding of LanceDB/Arrow query results into `SearchResult`s.

use std::collections::HashMap;
use std::path::Path;

use arrow_array::{Array, Float32Array, Float64Array, RecordBatch, StringArray};
use serde_json::Value;

use crate::error::SearchError;
use crate::models::{ChunkSource, Citation, SearchResult, SearchSource};

const DISTANCE_COLUMN_NAME: &str = "_distance";
const DOC_ID_COLUMN_NAME: &str = "doc_id";
const TEXT_COLUMN_NAME: &str = "text";
const METADATA_COLUMN_NAME: &str = "metadata";

pub(crate) fn decode_lancedb_batches(
    batches: &[RecordBatch],
    shard_path: &Path,
) -> Result<Vec<SearchResult>, SearchError> {
    let mut results = Vec::new();

    for batch in batches {
        let doc_ids = downcast_string_column(batch, DOC_ID_COLUMN_NAME, shard_path)?;
        let texts = downcast_string_column(batch, TEXT_COLUMN_NAME, shard_path)?;
        let metadata = downcast_string_column(batch, METADATA_COLUMN_NAME, shard_path)?;
        let distances =
            batch
                .column_by_name(DISTANCE_COLUMN_NAME)
                .ok_or_else(|| SearchError::Execution {
                    message: format!(
                        "local LanceDB query did not return {} column at {}",
                        DISTANCE_COLUMN_NAME,
                        shard_path.display()
                    ),
                })?;

        for index in 0..batch.num_rows() {
            let score =
                lancedb_distance_to_score(distance_value(distances.as_ref(), index, shard_path)?)?;
            let metadata = parse_metadata_json(metadata.value(index), shard_path)?;
            let citation = metadata.as_ref().and_then(Citation::from_metadata);
            let result = SearchResult {
                doc_id: doc_ids.value(index).to_string(),
                score,
                text: texts.value(index).to_string(),
                metadata,
                source: SearchSource::Vector,
                chunk_source: ChunkSource::Dynamic,
                corpus_type: None,
                citation,
            };

            result.validate()?;
            results.push(result);
        }
    }

    Ok(results)
}

fn downcast_string_column<'a>(
    batch: &'a RecordBatch,
    column_name: &str,
    shard_path: &Path,
) -> Result<&'a StringArray, SearchError> {
    batch
        .column_by_name(column_name)
        .ok_or_else(|| SearchError::Execution {
            message: format!(
                "local LanceDB query did not return {} column at {}",
                column_name,
                shard_path.display()
            ),
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| SearchError::Execution {
            message: format!(
                "local LanceDB column {} had unexpected type at {}",
                column_name,
                shard_path.display()
            ),
        })
}

fn distance_value(
    distance_column: &dyn Array,
    index: usize,
    shard_path: &Path,
) -> Result<f32, SearchError> {
    if let Some(values) = distance_column.as_any().downcast_ref::<Float32Array>() {
        return Ok(values.value(index));
    }
    if let Some(values) = distance_column.as_any().downcast_ref::<Float64Array>() {
        return Ok(values.value(index) as f32);
    }

    Err(SearchError::Execution {
        message: format!(
            "local LanceDB distance column had unexpected type at {}",
            shard_path.display()
        ),
    })
}

fn lancedb_distance_to_score(distance: f32) -> Result<f32, SearchError> {
    if !distance.is_finite() {
        return Err(SearchError::Execution {
            message: "local LanceDB query returned non-finite distance".into(),
        });
    }

    Ok((1.0 - distance).clamp(0.0, 1.0))
}

fn parse_metadata_json(
    metadata_json: &str,
    shard_path: &Path,
) -> Result<Option<HashMap<String, Value>>, SearchError> {
    let metadata =
        serde_json::from_str::<HashMap<String, Value>>(metadata_json).map_err(|source| {
            SearchError::Execution {
                message: format!(
                    "failed to parse metadata from local LanceDB documents table at {}: {source}",
                    shard_path.display()
                ),
            }
        })?;

    if metadata.is_empty() {
        Ok(None)
    } else {
        Ok(Some(metadata))
    }
}
