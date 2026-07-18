//! The `static/_head` document: the single pointer to the active static
//! release. Later tasks CAS this JSON into SQLite/S3 under [`STATIC_HEAD_KEY`];
//! serialization and validation live here so producers and consumers of the
//! static-release pointer can never drift apart.
//!
//! Mirrors [`super::head::ManifestHead`] in shape and discipline: the manifest
//! path is always derived from the release id, never caller-supplied.
//!
//! [`STATIC_HEAD_KEY`]: super::s3_paths::STATIC_HEAD_KEY

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::head::MIN_PLAUSIBLE_EPOCH_MILLIS;
use super::s3_paths::static_release_manifest_key;

/// Length of a release id, which is a lowercase hex SHA-256 digest.
const RELEASE_ID_HEX_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticReleaseHead {
    pub release_id: String,
    pub manifest_path: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StaticHeadError {
    #[error("failed to parse static release head: {message}")]
    Parse { message: String },
    #[error("release_id must be exactly 64 lowercase hex characters")]
    InvalidReleaseId,
    #[error("updated_at must be a plausible epoch millis value")]
    ImplausibleUpdatedAt,
    #[error("manifest_path must match release_id; expected {expected}")]
    ManifestPathMismatch { expected: String },
}

impl StaticReleaseHead {
    /// Builds a head for `release_id`; the manifest path is always derived,
    /// never caller-supplied, so it cannot disagree with the release.
    pub fn new(release_id: String, updated_at: i64) -> Self {
        let manifest_path = static_release_manifest_key(&release_id);
        Self {
            release_id,
            manifest_path,
            updated_at,
        }
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, StaticHeadError> {
        let head: Self =
            serde_json::from_slice(bytes).map_err(|source| StaticHeadError::Parse {
                message: source.to_string(),
            })?;
        head.validate()?;
        Ok(head)
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("StaticReleaseHead serialization cannot fail")
    }

    pub fn validate(&self) -> Result<(), StaticHeadError> {
        if !is_release_id(&self.release_id) {
            return Err(StaticHeadError::InvalidReleaseId);
        }
        if self.updated_at < MIN_PLAUSIBLE_EPOCH_MILLIS {
            return Err(StaticHeadError::ImplausibleUpdatedAt);
        }
        let expected = static_release_manifest_key(&self.release_id);
        if self.manifest_path != expected {
            return Err(StaticHeadError::ManifestPathMismatch { expected });
        }

        Ok(())
    }
}

fn is_release_id(release_id: &str) -> bool {
    release_id.len() == RELEASE_ID_HEX_LEN
        && release_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::super::s3_paths::static_release_manifest_key;
    use super::*;

    #[test]
    fn static_head_roundtrips_and_derives_manifest_path() {
        let head = StaticReleaseHead::new("a".repeat(64), 1_700_000_000_000);
        assert_eq!(
            head.manifest_path,
            format!("static/releases/{}/release_manifest.json", "a".repeat(64))
        );
        let parsed = StaticReleaseHead::from_json(head.to_json_pretty().as_bytes()).unwrap();
        assert_eq!(parsed, head);
    }

    #[test]
    fn static_head_rejects_non_hex_release_id() {
        let bad = StaticReleaseHead {
            release_id: "not-hex".into(),
            manifest_path: static_release_manifest_key("not-hex"),
            updated_at: 1_700_000_000_000,
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn static_head_rejects_manifest_path_mismatch() {
        let head = StaticReleaseHead {
            release_id: "a".repeat(64),
            manifest_path: "static/releases/wrong/release_manifest.json".into(),
            updated_at: 1_700_000_000_000,
        };
        assert!(head.validate().is_err());
    }
}
