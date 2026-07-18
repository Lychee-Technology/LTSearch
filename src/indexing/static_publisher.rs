//! Static release activation orchestration: verify a built release directory,
//! install it into the local managed store, and CAS the `static/_head` pointer.
//!
//! These three concerns are deliberately separate functions so a caller can
//! verify without installing, install without activating, or drive all three in
//! sequence. Everything here depends only on the [`PublishStorage`] trait plus
//! the `index`/`storage` modules — never on `adapters` or any `#[cfg(aws)]`
//! item, so activation stays in the AWS-free dependency graph.

use std::fs;
use std::path::Path;

use crate::error::PublishError;
use crate::index::{
    derive_release_id, sha256_hex, MmapIndex, ReleaseManifest, RELEASE_MANIFEST_FILE,
};
use crate::storage::{static_release_dir_key, StaticReleaseHead, STATIC_HEAD_KEY};

use super::publisher::PublishStorage;

/// The TurboQuant version a static release must declare.
const EXPECTED_TURBO_VERSION: u32 = 3;
/// The manifest schema version this activation understands.
const EXPECTED_MANIFEST_SCHEMA_VERSION: u32 = 1;
/// The only embedding dimension supported by the v3 static codec.
const EXPECTED_EMBEDDING_DIM: u32 = 512;

/// Failure modes of the static release activation orchestration.
#[derive(Debug)]
pub enum StaticActivateError {
    /// The release directory failed one of the eight self-consistency checks,
    /// or the pointer being activated is itself malformed.
    Verify { message: String },
    /// The pointer CAS lost to a concurrent writer.
    LostCas { release_id: String },
    /// The underlying publish storage returned an error.
    Storage(PublishError),
    /// A local filesystem operation failed during install.
    Io { message: String },
}

/// The outcome of a successful pointer activation.
#[derive(Debug)]
pub struct StaticActivationResult {
    pub release_id: String,
    pub previous_release_id: Option<String>,
}

/// Verifies that `dir` holds a self-consistent v3 static release, returning the
/// parsed [`ReleaseManifest`] on success.
///
/// The eight steps (from the plan's 设计要点 2) each independently reject a
/// tampered or mismatched release:
///
/// 1. Parse `release_manifest.json` into a [`ReleaseManifest`].
/// 2. `manifest_schema_version == 1 && turbo_version == 3`.
/// 3. Recompute every `outputs[]` entry's `sha256` + `size_bytes` from disk and
///    compare field-by-field.
/// 4. Recompute `derive_release_id(..)` and require it to equal `release_id`.
/// 5. Lance provenance is coherent (`kind == "lance"`, non-empty `dataset_path`,
///    positive `table_version`, `table_row_count == doc_count`).
/// 6. Embedding profile matches the codec and the requested `dim` == 512, with a
///    non-empty `model_id`; honor optional `expect_model_id` / `expect_dim`.
/// 7. `MmapIndex::load(dir)` succeeds with `version() == 3` and
///    `record_count() == table_row_count`.
/// 8. Return the verified manifest.
pub fn verify_release_dir(
    dir: &Path,
    expect_model_id: Option<&str>,
    expect_dim: Option<u32>,
) -> Result<ReleaseManifest, StaticActivateError> {
    // Step 1: parse release_manifest.json.
    let manifest_path = dir.join(RELEASE_MANIFEST_FILE);
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| StaticActivateError::Verify {
        message: format!(
            "failed to read release manifest {}: {error}",
            manifest_path.display()
        ),
    })?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| StaticActivateError::Verify {
            message: format!(
                "failed to parse release manifest {}: {error}",
                manifest_path.display()
            ),
        })?;

    // Step 2: schema/turbo version.
    if manifest.manifest_schema_version != EXPECTED_MANIFEST_SCHEMA_VERSION {
        return Err(verify_err(format!(
            "manifest_schema_version {} != {EXPECTED_MANIFEST_SCHEMA_VERSION}",
            manifest.manifest_schema_version
        )));
    }
    if manifest.turbo_version != EXPECTED_TURBO_VERSION {
        return Err(verify_err(format!(
            "turbo_version {} != {EXPECTED_TURBO_VERSION}",
            manifest.turbo_version
        )));
    }

    // Step 3: recompute each output's hash + size from the file on disk.
    for output in &manifest.outputs {
        let output_path = dir.join(&output.name);
        let bytes = fs::read(&output_path).map_err(|error| {
            verify_err(format!(
                "failed to read output {}: {error}",
                output_path.display()
            ))
        })?;
        let actual_size = bytes.len() as u64;
        if actual_size != output.size_bytes {
            return Err(verify_err(format!(
                "output {} size mismatch: manifest {}, disk {actual_size}",
                output.name, output.size_bytes
            )));
        }
        let actual_sha = sha256_hex(&bytes);
        if actual_sha != output.sha256 {
            return Err(verify_err(format!(
                "output {} sha256 mismatch: manifest {}, disk {actual_sha}",
                output.name, output.sha256
            )));
        }
    }

    // Step 4: the release_id must be exactly the content-derived id.
    let recomputed_release_id = derive_release_id(
        manifest.turbo_version,
        &manifest.embedding_profile,
        &manifest.codec,
        &manifest.input_fingerprint.content_digest,
        &manifest.outputs,
    );
    if recomputed_release_id != manifest.release_id {
        return Err(verify_err(format!(
            "release_id mismatch: manifest {}, recomputed {recomputed_release_id}",
            manifest.release_id
        )));
    }

    // Step 5: Lance provenance.
    if manifest.source.kind != "lance" {
        return Err(verify_err(format!(
            "source.kind {} != \"lance\"",
            manifest.source.kind
        )));
    }
    if manifest.source.dataset_path.is_empty() {
        return Err(verify_err("source.dataset_path is empty".to_string()));
    }
    if manifest.source.table_version == 0 {
        return Err(verify_err("source.table_version must be > 0".to_string()));
    }
    if manifest.source.table_row_count != manifest.input_fingerprint.doc_count {
        return Err(verify_err(format!(
            "source.table_row_count {} != input_fingerprint.doc_count {}",
            manifest.source.table_row_count, manifest.input_fingerprint.doc_count
        )));
    }

    // Step 6: embedding profile / codec dim coherence.
    if manifest.embedding_profile.dim != EXPECTED_EMBEDDING_DIM {
        return Err(verify_err(format!(
            "embedding_profile.dim {} != {EXPECTED_EMBEDDING_DIM}",
            manifest.embedding_profile.dim
        )));
    }
    if manifest.codec.dim != EXPECTED_EMBEDDING_DIM {
        return Err(verify_err(format!(
            "codec.dim {} != {EXPECTED_EMBEDDING_DIM}",
            manifest.codec.dim
        )));
    }
    if manifest.embedding_profile.model_id.is_empty() {
        return Err(verify_err(
            "embedding_profile.model_id is empty".to_string(),
        ));
    }
    if let Some(expected_model_id) = expect_model_id {
        if manifest.embedding_profile.model_id != expected_model_id {
            return Err(verify_err(format!(
                "embedding_profile.model_id {} != expected {expected_model_id}",
                manifest.embedding_profile.model_id
            )));
        }
    }
    if let Some(expected_dim) = expect_dim {
        if manifest.embedding_profile.dim != expected_dim {
            return Err(verify_err(format!(
                "embedding_profile.dim {} != expected {expected_dim}",
                manifest.embedding_profile.dim
            )));
        }
    }

    // Step 7: the image must actually load as a v3 index with a matching count.
    let index = MmapIndex::load(dir).map_err(|error| {
        verify_err(format!("MmapIndex::load({}) failed: {error}", dir.display()))
    })?;
    if index.version() != EXPECTED_TURBO_VERSION {
        return Err(verify_err(format!(
            "loaded image version {} != {EXPECTED_TURBO_VERSION}",
            index.version()
        )));
    }
    if index.record_count() != manifest.source.table_row_count {
        return Err(verify_err(format!(
            "loaded record_count {} != source.table_row_count {}",
            index.record_count(),
            manifest.source.table_row_count
        )));
    }

    // Step 8: return the verified manifest.
    Ok(manifest)
}

/// Installs a verified release directory into the local managed store at
/// `<root>/static/releases/<release_id>/`.
///
/// Idempotent: if the target already exists, this is a no-op. Otherwise it tries
/// a plain `fs::rename` first; on any rename failure (e.g. a cross-device move)
/// it falls back to a recursive copy into a `.<release_id>-staging` sibling and
/// then renames that into place.
pub fn install_into_managed_store(
    root: &Path,
    release_id: &str,
    src_dir: &Path,
) -> Result<(), StaticActivateError> {
    let releases_dir = root.join(static_release_dir_key(release_id));
    let target = releases_dir; // static/releases/<release_id>

    if target.exists() {
        return Ok(()); // idempotent skip
    }

    let parent = target.parent().ok_or_else(|| StaticActivateError::Io {
        message: format!("target {} has no parent directory", target.display()),
    })?;
    fs::create_dir_all(parent).map_err(|error| StaticActivateError::Io {
        message: format!("failed to create {}: {error}", parent.display()),
    })?;

    // Fast path: a same-filesystem rename atomically publishes the release.
    if fs::rename(src_dir, &target).is_ok() {
        return Ok(());
    }

    // Fallback: recursive copy into a hidden staging sibling, then rename it
    // into place so the target only ever appears fully-formed.
    let staging = parent.join(format!(".{release_id}-staging"));
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|error| StaticActivateError::Io {
            message: format!(
                "failed to clear stale staging {}: {error}",
                staging.display()
            ),
        })?;
    }
    copy_dir_recursive(src_dir, &staging)?;
    fs::rename(&staging, &target).map_err(|error| StaticActivateError::Io {
        message: format!(
            "failed to rename staging {} into {}: {error}",
            staging.display(),
            target.display()
        ),
    })?;

    Ok(())
}

/// Reads the current `static/_head` pointer, then compare-and-swaps a new
/// [`StaticReleaseHead`] for `release_id` into place.
///
/// The expected ETag is the current pointer's ETag (`None` if no pointer
/// exists), and the previous release id is parsed from the existing pointer.
/// A lost CAS surfaces as [`StaticActivateError::LostCas`].
pub async fn activate_static_pointer<S: PublishStorage>(
    storage: &S,
    release_id: &str,
    updated_at: i64,
) -> Result<StaticActivationResult, StaticActivateError> {
    let current = storage
        .read(STATIC_HEAD_KEY)
        .await
        .map_err(StaticActivateError::Storage)?;

    let expected_etag = current.as_ref().map(|object| object.etag.clone());
    let previous_release_id = match &current {
        Some(object) => Some(
            StaticReleaseHead::from_json(&object.bytes)
                .map_err(|error| {
                    verify_err(format!("existing static/_head is malformed: {error}"))
                })?
                .release_id,
        ),
        None => None,
    };

    let head = StaticReleaseHead::new(release_id.to_string(), updated_at);
    head.validate()
        .map_err(|error| verify_err(format!("new static/_head is invalid: {error}")))?;
    let head_bytes = head.to_json_pretty().into_bytes();

    let swapped = storage
        .compare_and_swap(STATIC_HEAD_KEY, expected_etag.as_deref(), &head_bytes)
        .await
        .map_err(StaticActivateError::Storage)?;
    if !swapped {
        return Err(StaticActivateError::LostCas {
            release_id: release_id.to_string(),
        });
    }

    Ok(StaticActivationResult {
        release_id: release_id.to_string(),
        previous_release_id,
    })
}

fn verify_err(message: String) -> StaticActivateError {
    StaticActivateError::Verify { message }
}

/// Recursively copies `src` into a fresh directory `dst`.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), StaticActivateError> {
    fs::create_dir_all(dst).map_err(|error| StaticActivateError::Io {
        message: format!("failed to create {}: {error}", dst.display()),
    })?;
    for entry in fs::read_dir(src).map_err(|error| StaticActivateError::Io {
        message: format!("failed to read {}: {error}", src.display()),
    })? {
        let entry = entry.map_err(|error| StaticActivateError::Io {
            message: format!("failed to read entry under {}: {error}", src.display()),
        })?;
        let file_type = entry.file_type().map_err(|error| StaticActivateError::Io {
            message: format!("failed to stat {}: {error}", entry.path().display()),
        })?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|error| StaticActivateError::Io {
                message: format!(
                    "failed to copy {} to {}: {error}",
                    entry.path().display(),
                    target.display()
                ),
            })?;
        }
    }
    Ok(())
}
