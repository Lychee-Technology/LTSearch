use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

const DOCUMENT_TEXT_MAX_BYTES: usize = 100_000;
const DOCUMENT_METADATA_MAX_BYTES: usize = 10_000;
const MAX_CACHE_SIZE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MIN_PLAUSIBLE_EPOCH_MILLIS: i64 = 1_000_000_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub doc_id: String,
    pub text: String,
    pub embedding: Option<Vec<f32>>,
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
    pub timestamp: i64,
}

impl Document {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.doc_id.is_empty() {
            return Err(ValidationError::Required { field: "doc_id" });
        }
        if self.doc_id.len() > 256 {
            return Err(ValidationError::LengthOutOfRange {
                field: "doc_id",
                min: 1,
                max: 256,
            });
        }
        if self.text.is_empty() {
            return Err(ValidationError::Required { field: "text" });
        }
        if self.text.len() > DOCUMENT_TEXT_MAX_BYTES {
            return Err(ValidationError::TooLarge {
                field: "text",
                max_bytes: DOCUMENT_TEXT_MAX_BYTES,
            });
        }
        if let Some(embedding) = &self.embedding {
            if embedding.iter().any(|value| !value.is_finite()) {
                return Err(ValidationError::InvalidValue { field: "embedding" });
            }
        }

        let metadata_len = serde_json::to_vec(&self.metadata)
            .map_err(|_| ValidationError::InvalidValue { field: "metadata" })?
            .len();
        if metadata_len > DOCUMENT_METADATA_MAX_BYTES {
            return Err(ValidationError::TooLarge {
                field: "metadata",
                max_bytes: DOCUMENT_METADATA_MAX_BYTES,
            });
        }
        if !is_plausible_epoch_millis(self.timestamp) {
            return Err(ValidationError::InvalidValue { field: "timestamp" });
        }

        Ok(())
    }

    pub fn validate_for_embedding_dim(&self, dim: usize) -> Result<(), ValidationError> {
        self.validate()?;

        if let Some(embedding) = &self.embedding {
            if embedding.len() != dim {
                return Err(ValidationError::InvalidValue { field: "embedding" });
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardManifest {
    pub shard_id: u32,
    pub document_count: usize,
    pub lance_path: String,
    pub tantivy_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexManifest {
    pub version_id: u64,
    pub created_at: i64,
    pub embedding_dim: usize,
    pub document_count: usize,
    pub num_shards: usize,
    pub shards: Vec<ShardManifest>,
}

impl IndexManifest {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.num_shards == 0 {
            return Err(ValidationError::MustBePositive {
                field: "num_shards",
            });
        }
        if self.embedding_dim == 0 {
            return Err(ValidationError::MustBePositive {
                field: "embedding_dim",
            });
        }
        if !is_plausible_epoch_millis(self.created_at) {
            return Err(ValidationError::InvalidValue {
                field: "created_at",
            });
        }
        if self.shards.len() != self.num_shards {
            return Err(ValidationError::Mismatch {
                field: "shards",
                expected: "len(shards) == num_shards",
            });
        }

        let total_documents: usize = self.shards.iter().map(|shard| shard.document_count).sum();
        if total_documents != self.document_count {
            return Err(ValidationError::Mismatch {
                field: "document_count",
                expected: "sum(shard.document_count)",
            });
        }

        for shard in &self.shards {
            validate_s3_uri(&shard.lance_path, "lance_path")?;
            validate_s3_uri(&shard.tantivy_path, "tantivy_path")?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexCache {
    pub cache_dir: PathBuf,
    pub max_size_bytes: u64,
    pub current_version: Option<u64>,
}

impl IndexCache {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if !is_under_tmp(&self.cache_dir) {
            return Err(ValidationError::InvalidValue { field: "cache_dir" });
        }
        if self.max_size_bytes > MAX_CACHE_SIZE_BYTES {
            return Err(ValidationError::TooLarge {
                field: "max_size_bytes",
                max_bytes: MAX_CACHE_SIZE_BYTES as usize,
            });
        }
        if self.current_version.is_some_and(|version| version == 0) {
            return Err(ValidationError::InvalidValue {
                field: "current_version",
            });
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    pub hit_count: u64,
    pub miss_count: u64,
    pub current_version: Option<u64>,
    pub bytes_used: u64,
}

fn is_under_tmp(path: &Path) -> bool {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::RootDir)) {
        return false;
    }

    if !matches!(components.next(), Some(Component::Normal(part)) if part == "tmp") {
        return false;
    }

    !components.any(|component| matches!(component, Component::ParentDir))
}

fn validate_s3_uri(value: &str, field: &'static str) -> Result<(), ValidationError> {
    let Some(suffix) = value.strip_prefix("s3://") else {
        return Err(ValidationError::InvalidValue { field });
    };

    let mut parts = suffix.splitn(2, '/');
    let bucket = parts.next().unwrap_or_default();
    let key = parts.next().unwrap_or_default();

    if bucket.is_empty() || key.is_empty() {
        return Err(ValidationError::InvalidValue { field });
    }

    Ok(())
}

fn is_plausible_epoch_millis(value: i64) -> bool {
    value >= MIN_PLAUSIBLE_EPOCH_MILLIS
}
