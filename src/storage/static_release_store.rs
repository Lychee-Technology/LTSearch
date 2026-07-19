//! Read side of the `static/_head` pointer: [`StaticReleaseStore`] resolves the
//! currently-active [`StaticReleaseHead`], or `Ok(None)` when no static release
//! has ever been activated (fresh install / static feature unused).
//!
//! Mirrors [`super::manifest_store::ManifestStore`] in shape and discipline, but
//! folds "pointer absent" into `Ok(None)` instead of an error: an unset static
//! pointer is a normal steady state, not a failure. The write side CAS-updates
//! this same pointer under [`STATIC_HEAD_KEY`] (SQLite row or `static/_head`
//! file); consumers here never derive the manifest path themselves —
//! [`StaticReleaseHead`] already carries (and self-validates) it.
//!
//! [`STATIC_HEAD_KEY`]: super::s3_paths::STATIC_HEAD_KEY

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::s3_paths::STATIC_HEAD_KEY;
use super::static_head::{StaticHeadError, StaticReleaseHead};

#[derive(Debug, Error)]
pub enum StaticReleaseStoreError {
    #[error("static release head is invalid: {message}")]
    Invalid { message: String },
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Maps a [`StaticHeadError`] (parse / validation failure of the pointer bytes)
/// onto [`StaticReleaseStoreError::Invalid`], preserving serde's parse message.
/// Shared by every backend so a corrupt pointer reads the same regardless of
/// where the bytes lived.
pub(crate) fn invalid_head(error: StaticHeadError) -> StaticReleaseStoreError {
    StaticReleaseStoreError::Invalid {
        message: match error {
            StaticHeadError::Parse { message } => message,
            other => other.to_string(),
        },
    }
}

pub trait StaticReleaseStore {
    /// Loads the active static release pointer, or `Ok(None)` if unset. A
    /// present-but-corrupt pointer is an error ([`StaticReleaseStoreError::Invalid`]).
    fn load_active_release(&self) -> Result<Option<StaticReleaseHead>, StaticReleaseStoreError>;
}

/// File-backed pointer store: reads `<root>/static/_head`. Used on deployments
/// where the pointer lives as a plain file (AWS syncs it down from S3, or a
/// pure-file install); the SQLite-backed store supersedes it wherever a local
/// control-plane db exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStaticReleaseStore {
    root: PathBuf,
}

impl LocalStaticReleaseStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn head_path(&self) -> PathBuf {
        self.root.join(STATIC_HEAD_KEY)
    }
}

impl StaticReleaseStore for LocalStaticReleaseStore {
    fn load_active_release(&self) -> Result<Option<StaticReleaseHead>, StaticReleaseStoreError> {
        let path = self.head_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StaticReleaseStoreError::Io { path, source }),
        };
        StaticReleaseHead::from_json(&bytes)
            .map(Some)
            .map_err(invalid_head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_head(root: &Path, head: &StaticReleaseHead) {
        let path = root.join(STATIC_HEAD_KEY);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, head.to_json_pretty()).unwrap();
    }

    #[test]
    fn missing_head_file_folds_to_none() {
        let root = temp_root();
        let store = LocalStaticReleaseStore::new(root.path());
        assert_eq!(store.load_active_release().unwrap(), None);
    }

    #[test]
    fn writes_then_reads_back_head() {
        let root = temp_root();
        let head = StaticReleaseHead::new("a".repeat(64), 1_700_000_000_000);
        write_head(root.path(), &head);

        let store = LocalStaticReleaseStore::new(root.path());
        assert_eq!(store.load_active_release().unwrap(), Some(head));
    }

    #[test]
    fn corrupt_head_file_is_invalid() {
        let root = temp_root();
        let path = root.path().join(STATIC_HEAD_KEY);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not valid json").unwrap();

        let store = LocalStaticReleaseStore::new(root.path());
        assert!(matches!(
            store.load_active_release(),
            Err(StaticReleaseStoreError::Invalid { .. })
        ));
    }
}
