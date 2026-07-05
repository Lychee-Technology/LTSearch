//! The `_head` document: the single pointer to the active index version.
//!
//! Both the publish side (`indexing::publisher`) and the read side
//! (`storage::manifest_store`) speak this contract; serialization and
//! validation live here so the two can never drift apart.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::s3_paths::version_manifest_key;

pub const MIN_PLAUSIBLE_EPOCH_MILLIS: i64 = 1_000_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestHead {
    pub version_id: u64,
    pub manifest_path: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HeadError {
    #[error("failed to parse manifest head: {message}")]
    Parse { message: String },
    #[error("version_id must be positive")]
    VersionMustBePositive,
    #[error("updated_at must be a plausible epoch millis value")]
    ImplausibleUpdatedAt,
    #[error("manifest_path must match version_id; expected {expected}")]
    ManifestPathMismatch { expected: String },
}

impl ManifestHead {
    /// Builds a head for `version_id`; the manifest path is always derived,
    /// never caller-supplied, so it cannot disagree with the version.
    pub fn new(version_id: u64, updated_at: i64) -> Self {
        Self {
            version_id,
            manifest_path: version_manifest_key(version_id),
            updated_at,
        }
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, HeadError> {
        let head: Self = serde_json::from_slice(bytes).map_err(|source| HeadError::Parse {
            message: source.to_string(),
        })?;
        head.validate()?;
        Ok(head)
    }

    pub fn to_json_pretty(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("ManifestHead serialization cannot fail")
    }

    pub fn validate(&self) -> Result<(), HeadError> {
        if self.version_id == 0 {
            return Err(HeadError::VersionMustBePositive);
        }
        if self.updated_at < MIN_PLAUSIBLE_EPOCH_MILLIS {
            return Err(HeadError::ImplausibleUpdatedAt);
        }
        let expected = version_manifest_key(self.version_id);
        if self.manifest_path != expected {
            return Err(HeadError::ManifestPathMismatch { expected });
        }

        Ok(())
    }
}
