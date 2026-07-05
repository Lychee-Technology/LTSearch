//! Stage-then-rename filesystem transaction shared by the index builders.
//!
//! Artifacts are written under a hidden per-process staging directory and
//! renamed into their final locations only once everything succeeded; on a
//! failed rename the already-moved destinations and the staging root are
//! removed so no partial publish is left behind.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::IndexError;

#[derive(Debug)]
pub struct StagedDir {
    root: PathBuf,
}

impl StagedDir {
    /// Creates `.{label}-staging-{pid}-{nonce}` under `base`.
    pub(crate) fn create(base: &Path, label: &str) -> Result<Self, IndexError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|source| IndexError::Operation {
                message: format!("failed to calculate staging timestamp: {source}"),
            })?
            .as_nanos();
        let root = base.join(format!(".{label}-staging-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).map_err(|source| IndexError::Operation {
            message: format!(
                "failed to create staging directory {}: {source}",
                root.display()
            ),
        })?;

        Ok(Self { root })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    /// Renames each `(source, destination)` pair into place, then removes the
    /// staging root. If a rename fails, destinations moved so far and the
    /// staging root are removed, and any cleanup failure is appended to the
    /// original error.
    pub(crate) fn commit(self, moves: Vec<(PathBuf, PathBuf)>) -> Result<(), IndexError> {
        let mut moved: Vec<PathBuf> = Vec::with_capacity(moves.len());

        for (source, destination) in moves {
            if let Err(error) = move_into_place(&source, &destination) {
                let mut cleanups: Vec<Result<(), IndexError>> = moved
                    .iter()
                    .map(|destination| remove_path_if_exists(destination))
                    .collect();
                cleanups.push(remove_dir_all_if_exists(&self.root));
                return Err(append_cleanup_failure(
                    error,
                    combine_cleanup_results(cleanups),
                ));
            }
            moved.push(destination);
        }

        remove_dir_all_if_exists(&self.root)
    }

    /// Discards the staging directory without publishing anything.
    pub(crate) fn abort(self) -> Result<(), IndexError> {
        remove_dir_all_if_exists(&self.root)
    }
}

pub(crate) fn append_cleanup_failure(
    error: IndexError,
    cleanup: Result<(), IndexError>,
) -> IndexError {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => IndexError::Operation {
            message: format!("{error}; cleanup failed: {cleanup_error}"),
        },
    }
}

fn move_into_place(source: &Path, destination: &Path) -> Result<(), IndexError> {
    let parent = destination.parent().ok_or_else(|| IndexError::Operation {
        message: format!("path {} has no parent", destination.display()),
    })?;
    fs::create_dir_all(parent).map_err(|source_error| IndexError::Operation {
        message: format!(
            "failed to create directory {}: {source_error}",
            parent.display()
        ),
    })?;
    fs::rename(source, destination).map_err(|source_error| IndexError::Operation {
        message: format!(
            "failed to publish staged artifact from {} to {}: {source_error}",
            source.display(),
            destination.display()
        ),
    })
}

fn remove_dir_all_if_exists(path: &Path) -> Result<(), IndexError> {
    if !path.exists() {
        return Ok(());
    }

    fs::remove_dir_all(path).map_err(|source| IndexError::Operation {
        message: format!("failed to remove directory {}: {source}", path.display()),
    })
}

fn remove_path_if_exists(path: &Path) -> Result<(), IndexError> {
    if !path.exists() {
        return Ok(());
    }

    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|source| IndexError::Operation {
            message: format!("failed to remove directory {}: {source}", path.display()),
        })
    } else {
        fs::remove_file(path).map_err(|source| IndexError::Operation {
            message: format!("failed to remove file {}: {source}", path.display()),
        })
    }
}

fn combine_cleanup_results(results: Vec<Result<(), IndexError>>) -> Result<(), IndexError> {
    let errors = results
        .into_iter()
        .filter_map(Result::err)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(IndexError::Operation {
            message: errors.join("; "),
        })
    }
}
