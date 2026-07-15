use std::collections::HashMap;

#[cfg(feature = "aws")]
use aws_sdk_s3::Client as S3Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::IndexError;
use crate::models::CorpusType;

use super::StaticChunk;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticSourceConfig {
    pub bucket: String,
    pub key: String,
    pub corpus_type: CorpusType,
}

impl Default for StaticSourceConfig {
    fn default() -> Self {
        Self {
            bucket: String::new(),
            key: String::new(),
            corpus_type: CorpusType::Legal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TurboBuildConfig {
    #[serde(default)]
    pub sources: Vec<StaticSourceConfig>,
}

#[derive(Debug, Deserialize)]
struct StaticSourceLine {
    doc_id: String,
    text: String,
    #[serde(default)]
    metadata: HashMap<String, Value>,
}

/// 解析 JSONL 静态源字节为 `StaticChunk`；`origin` 仅用于错误信息。无 AWS 依赖，
/// 供本地构建与 S3 构建共用。
pub fn parse_static_source_lines(
    bytes: &[u8],
    corpus_type: &CorpusType,
    origin: &str,
) -> Result<Vec<StaticChunk>, IndexError> {
    let text = std::str::from_utf8(bytes).map_err(|error| IndexError::Operation {
        message: format!("static source {origin} was not valid utf-8: {error}"),
    })?;
    let mut chunks = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let chunk: StaticSourceLine =
            serde_json::from_str(line).map_err(|error| IndexError::Operation {
                message: format!(
                    "failed to parse static source line {} from {origin}: {error}",
                    line_number + 1
                ),
            })?;
        chunks.push(StaticChunk {
            doc_id: chunk.doc_id,
            text: chunk.text,
            metadata: chunk.metadata,
            corpus_type: corpus_type.clone(),
        });
    }
    Ok(chunks)
}

#[cfg(feature = "aws")]
pub async fn load_static_chunks_from_s3(
    client: &S3Client,
    sources: &[StaticSourceConfig],
) -> Result<Vec<StaticChunk>, IndexError> {
    let mut chunks = Vec::new();

    for source in sources {
        let object = client
            .get_object()
            .bucket(&source.bucket)
            .key(&source.key)
            .send()
            .await
            .map_err(|error| IndexError::Operation {
                message: format!(
                    "failed to load static source s3://{}/{}: {error}",
                    source.bucket, source.key
                ),
            })?;

        let body = object
            .body
            .collect()
            .await
            .map_err(|error| IndexError::Operation {
                message: format!(
                    "failed to read static source body s3://{}/{}: {error}",
                    source.bucket, source.key
                ),
            })?
            .into_bytes();

        let mut source_chunks = parse_static_source_lines(
            body.as_ref(),
            &source.corpus_type,
            &format!("s3://{}/{}", source.bucket, source.key),
        )?;
        chunks.append(&mut source_chunks);
    }

    Ok(chunks)
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parse_skips_blank_lines_and_applies_corpus_type() {
        let jsonl = b"{\"doc_id\":\"d1\",\"text\":\"hello\"}\n\n{\"doc_id\":\"d2\",\"text\":\"world\"}\n";
        let chunks =
            parse_static_source_lines(jsonl, &CorpusType::Legal, "s3://bucket/key").unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].doc_id, "d1");
        assert_eq!(chunks[1].corpus_type, CorpusType::Legal);
    }
}
