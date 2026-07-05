//! Warm-container cache accounting for local LanceDB shard directories.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::SearchError;

#[derive(Debug, Default)]
pub(crate) struct LocalLanceCache {
    pub(crate) seen_shards: HashMap<PathBuf, u64>,
    pub(crate) hit_count: u64,
    pub(crate) miss_count: u64,
    pub(crate) attempted_version: Option<u64>,
    pub(crate) current_version: Option<u64>,
    pub(crate) bytes_used: u64,
}

impl LocalLanceCache {
    pub(crate) fn reset_for_attempt(&mut self, version_id: u64) {
        self.seen_shards.clear();
        self.hit_count = 0;
        self.miss_count = 0;
        self.bytes_used = 0;
        self.attempted_version = Some(version_id);
        self.current_version = None;
    }

    pub(crate) fn publish_version(&mut self, version_id: u64) {
        self.attempted_version = Some(version_id);
        self.current_version = Some(version_id);
    }
}

/// Sizes a shard directory while re-verifying that every file it reaches
/// (including through symlinks) stays inside the artifact root, and that no
/// symlink cycle exists.
pub(crate) fn inspect_path_tree_within_artifact_root(
    artifact_root: &Path,
    root: &Path,
) -> Result<u64, SearchError> {
    let canonical_artifact_root =
        artifact_root
            .canonicalize()
            .map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to canonicalize artifact root {}: {source}",
                    artifact_root.display()
                ),
            })?;
    let mut pending = vec![root.to_path_buf()];
    let mut visited_dirs = HashSet::new();
    let mut total_bytes = 0;

    while let Some(path) = pending.pop() {
        let canonical_path = path
            .canonicalize()
            .map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to canonicalize local LanceDB documents path {}: {source}",
                    path.display()
                ),
            })?;

        if !canonical_path.starts_with(&canonical_artifact_root) {
            return Err(SearchError::Execution {
                message: format!(
                    "resolved local LanceDB documents path escapes artifact root: {}",
                    canonical_path.display()
                ),
            });
        }

        let metadata = fs::metadata(&canonical_path).map_err(|source| SearchError::Execution {
            message: format!(
                "failed to inspect local LanceDB documents path {}: {source}",
                canonical_path.display()
            ),
        })?;

        if metadata.is_file() {
            total_bytes += metadata.len();
            continue;
        }

        if metadata.is_dir() {
            if !visited_dirs.insert(canonical_path.clone()) {
                return Err(SearchError::Execution {
                    message: format!(
                        "local LanceDB artifact path contains a symlink cycle under artifact root: {}",
                        canonical_path.display()
                    ),
                });
            }

            for entry in fs::read_dir(&canonical_path).map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to read local LanceDB artifact directory {}: {source}",
                    canonical_path.display()
                ),
            })? {
                pending.push(
                    entry
                        .map_err(|source| SearchError::Execution {
                            message: format!(
                        "failed to read local LanceDB artifact directory entry in {}: {source}",
                        canonical_path.display()
                    ),
                        })?
                        .path(),
                );
            }
        }
    }

    Ok(total_bytes)
}
