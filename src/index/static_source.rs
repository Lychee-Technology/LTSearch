use std::collections::HashMap;

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

        let text = std::str::from_utf8(body.as_ref()).map_err(|error| IndexError::Operation {
            message: format!(
                "static source s3://{}/{} was not valid utf-8: {error}",
                source.bucket, source.key
            ),
        })?;

        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            let chunk: StaticSourceLine =
                serde_json::from_str(line).map_err(|error| IndexError::Operation {
                    message: format!(
                        "failed to parse static source line {} from s3://{}/{}: {error}",
                        line_number + 1,
                        source.bucket,
                        source.key
                    ),
                })?;

            chunks.push(StaticChunk {
                doc_id: chunk.doc_id,
                text: chunk.text,
                metadata: chunk.metadata,
                corpus_type: source.corpus_type.clone(),
            });
        }
    }

    Ok(chunks)
}
