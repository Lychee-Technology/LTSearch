//! v3 static-release writer.
//!
//! [`StaticReleaseBuilder`] turns pre-embedded, doc_id-sorted chunks into a
//! self-describing TurboQuant **v3** static release: the six v2 blobs plus the
//! v3 sidecars (original doc_id, canonicalized metadata JSON, `MetaExtRecord`)
//! and a deterministic [`ReleaseManifest`].
//!
//! Embeddings arrive as a plain `&[Vec<f32>]` with no `Option`: a missing
//! embedding is unrepresentable here, so re-embedding cannot leak into the
//! release path. Codec seeds/constants are shared with the v2 writer, so both
//! versions encode identically.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::error::IndexError;
use crate::storage::staged_publish::{append_cleanup_failure, StagedDir};

use super::release_manifest::{
    canonical_metadata_json, content_digest, derive_release_id, sha256_hex, CanonicalRow,
    CodecMetadata, EmbeddingProfile, InputFingerprint, OutputFile, ReleaseManifest, ReleaseSource,
    RELEASE_MANIFEST_FILE,
};
use super::static_builder::{
    corpus_type_id, encode_turbo_record, meta_record_bytes, stable_hash_doc_id, turbo_record_bytes,
    StaticChunk, CENTROIDS_PER_DIM, CENTROIDS_SEED, PROJECTION_SEED, SUPPORTED_TYPED_DIM,
};
use super::{CentroidTable, MetaExtRecord, MetaRecord, ProjectionMatrix, TurboHeader};

/// Writes self-describing TurboQuant v3 static releases.
pub struct StaticReleaseBuilder;

impl StaticReleaseBuilder {
    /// Builds a v3 static release into `output_dir`, atomically replacing any
    /// previous contents, and returns the deterministic [`ReleaseManifest`].
    ///
    /// `chunks` must already be sorted by `doc_id` (Task 6 guarantees this) and
    /// `embeddings[i]` is the vector for `chunks[i]`. There is no generator
    /// parameter: every chunk must arrive already embedded.
    pub fn build_release(
        &self,
        output_dir: &Path,
        chunks: &[StaticChunk],
        embeddings: &[Vec<f32>],
        profile: &EmbeddingProfile,
        source: &ReleaseSource,
    ) -> Result<ReleaseManifest, IndexError> {
        // --- Step 1: validation ------------------------------------------------
        if chunks.len() != embeddings.len() {
            return Err(IndexError::Operation {
                message: format!(
                    "static chunk count {} does not match embedding count {}",
                    chunks.len(),
                    embeddings.len()
                ),
            });
        }
        if chunks.is_empty() {
            return Err(IndexError::Operation {
                message: "static release requires at least one chunk".into(),
            });
        }
        if profile.dim != SUPPORTED_TYPED_DIM {
            return Err(IndexError::Operation {
                message: format!(
                    "static release only supports typed turbo layout for {}-dim embeddings, profile declares {}",
                    SUPPORTED_TYPED_DIM, profile.dim
                ),
            });
        }
        for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
            if embedding.len() != profile.dim as usize {
                return Err(IndexError::Operation {
                    message: format!(
                        "static chunk {} embedding dimension {} does not match profile dim {}",
                        chunk.doc_id,
                        embedding.len(),
                        profile.dim
                    ),
                });
            }
            if embedding.iter().any(|value| !value.is_finite()) {
                return Err(IndexError::Operation {
                    message: format!(
                        "static chunk {} produced a non-finite embedding",
                        chunk.doc_id
                    ),
                });
            }
        }
        detect_duplicate_doc_ids(chunks)?;
        let hashed: Vec<(String, u64)> = chunks
            .iter()
            .map(|chunk| (chunk.doc_id.clone(), stable_hash_doc_id(&chunk.doc_id)))
            .collect();
        detect_hash_collisions(&hashed)?;

        // --- Step 2: codec assets (identical seeds/params to the v2 writer) ----
        let dim = SUPPORTED_TYPED_DIM;
        let centroids = CentroidTable::generate(dim, CENTROIDS_PER_DIM, CENTROIDS_SEED);
        let projection = ProjectionMatrix::generate(dim, dim, PROJECTION_SEED);

        // --- Step 3: single-pass byte construction (order == chunk order) ------
        let mut turbo_static = TurboHeader::new_v3(dim, chunks.len() as u64).to_bytes();
        let mut turbo_static_meta = Vec::new();
        let mut turbo_static_text = Vec::new();
        let mut turbo_static_title = Vec::new();
        let mut turbo_static_meta_ext = Vec::new();
        let mut turbo_static_docid = Vec::new();
        let mut turbo_static_meta_json = Vec::new();
        let mut canonical_rows = Vec::with_capacity(chunks.len());

        for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
            let doc_hash = stable_hash_doc_id(&chunk.doc_id);
            let record =
                encode_turbo_record(doc_hash, embedding, &centroids, &projection, &chunk.doc_id)?;
            turbo_static.extend_from_slice(turbo_record_bytes(&record));

            let text_offset = turbo_static_text.len() as u64;
            turbo_static_text.extend_from_slice(chunk.text.as_bytes());

            // Title mirrors the v2 writer: a chunk without a non-empty
            // `metadata["title"]` records `title_len == 0`, which reads back as
            // `None`.
            let title = chunk
                .metadata
                .get("title")
                .and_then(serde_json::Value::as_str)
                .filter(|title| !title.is_empty());
            let title_offset = turbo_static_title.len() as u64;
            let title_len = match title {
                Some(title) => {
                    turbo_static_title.extend_from_slice(title.as_bytes());
                    title.len() as u32
                }
                None => 0,
            };

            let meta = MetaRecord {
                doc_id: doc_hash,
                corpus_type: corpus_type_id(&chunk.corpus_type),
                _pad: [0; 7],
                text_offset,
                text_len: chunk.text.len() as u32,
                title_offset,
                title_len,
            };
            turbo_static_meta.extend_from_slice(meta_record_bytes(&meta));

            // Canonicalize metadata exactly once and fan it out to the sidecars
            // and the content-digest row.
            let canonical_meta_json = canonical_metadata_json(&chunk.metadata);

            let docid_offset = turbo_static_docid.len() as u64;
            turbo_static_docid.extend_from_slice(chunk.doc_id.as_bytes());
            let docid_len = chunk.doc_id.len() as u32;

            let meta_json_offset = turbo_static_meta_json.len() as u64;
            turbo_static_meta_json.extend_from_slice(&canonical_meta_json);
            let meta_json_len = canonical_meta_json.len() as u32;

            let meta_ext = MetaExtRecord {
                docid_offset,
                meta_json_offset,
                docid_len,
                meta_json_len,
            };
            turbo_static_meta_ext.extend_from_slice(meta_ext_record_bytes(&meta_ext));

            canonical_rows.push(CanonicalRow {
                doc_id: chunk.doc_id.clone(),
                embedding: embedding.clone(),
                text: chunk.text.clone(),
                canonical_meta_json,
            });
        }

        // --- Step 4: stage-and-write the nine .bin artifacts -------------------
        let staging_base = output_dir.parent().ok_or_else(|| IndexError::Operation {
            message: format!("path {} has no parent", output_dir.display()),
        })?;
        let staging_label = output_dir
            .file_name()
            .ok_or_else(|| IndexError::Operation {
                message: format!("path {} has no file name", output_dir.display()),
            })?
            .to_string_lossy()
            .into_owned();
        let staged = StagedDir::create(staging_base, &staging_label)?;

        let write_result = write_release_files(
            staged.path(),
            &centroids.to_bytes(),
            &projection.to_bytes(),
            &turbo_static,
            &turbo_static_meta,
            &turbo_static_text,
            &turbo_static_title,
            &turbo_static_meta_ext,
            &turbo_static_docid,
            &turbo_static_meta_json,
        );
        if let Err(error) = write_result {
            return Err(append_cleanup_failure(error, staged.abort()));
        }

        // --- Step 5: hash the staged files (name-ascending, manifest excluded) -
        let outputs = match collect_outputs(staged.path()) {
            Ok(outputs) => outputs,
            Err(error) => return Err(append_cleanup_failure(error, staged.abort())),
        };

        // --- Step 6: content fingerprint + codec metadata + release_id ---------
        let input_fingerprint = InputFingerprint {
            doc_count: chunks.len() as u64,
            content_digest: content_digest(&canonical_rows),
        };
        let codec = CodecMetadata {
            dim,
            centroids_per_dim: CENTROIDS_PER_DIM,
            centroids_seed: CENTROIDS_SEED,
            projection_seed: PROJECTION_SEED,
        };
        let release_id = derive_release_id(
            3,
            profile,
            &codec,
            &input_fingerprint.content_digest,
            &outputs,
        );

        // --- Step 7: assemble + serialize the manifest (compact, deterministic)
        let manifest = ReleaseManifest {
            manifest_schema_version: 1,
            turbo_version: 3,
            release_id,
            source: source.clone(),
            embedding_profile: profile.clone(),
            input_fingerprint,
            codec,
            outputs,
        };
        let manifest_bytes = match serde_json::to_vec(&manifest) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(append_cleanup_failure(
                    IndexError::Operation {
                        message: format!("failed to serialize release manifest: {error}"),
                    },
                    staged.abort(),
                ))
            }
        };
        if let Err(error) = write_file(&staged.path().join(RELEASE_MANIFEST_FILE), &manifest_bytes) {
            return Err(append_cleanup_failure(error, staged.abort()));
        }

        // --- Step 8: atomically publish ----------------------------------------
        staged.commit_replace_dir(output_dir)?;

        Ok(manifest)
    }
}

/// Rejects two chunks that share a `doc_id`.
pub(crate) fn detect_duplicate_doc_ids(chunks: &[StaticChunk]) -> Result<(), IndexError> {
    let mut seen: HashSet<&str> = HashSet::with_capacity(chunks.len());
    for chunk in chunks {
        if !seen.insert(chunk.doc_id.as_str()) {
            return Err(IndexError::Operation {
                message: format!("duplicate static chunk doc_id {}", chunk.doc_id),
            });
        }
    }
    Ok(())
}

/// Rejects two *distinct* doc_ids that hash to the same `stable_hash_doc_id`.
///
/// Real FNV-1a collisions cannot be brute-forced, so this guards against a
/// theoretical hash clash that would silently overwrite one record's identity.
pub(crate) fn detect_hash_collisions(hashed: &[(String, u64)]) -> Result<(), IndexError> {
    let mut by_hash: HashMap<u64, &str> = HashMap::with_capacity(hashed.len());
    for (doc_id, hash) in hashed {
        match by_hash.insert(*hash, doc_id.as_str()) {
            Some(existing) if existing != doc_id.as_str() => {
                return Err(IndexError::Operation {
                    message: format!(
                        "doc_id hash collision: {existing} and {doc_id} share hash {hash}"
                    ),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Views a `MetaExtRecord` as its raw `repr(C)` bytes for serialization.
fn meta_ext_record_bytes(record: &MetaExtRecord) -> &[u8] {
    // Safety: `MetaExtRecord` is `#[repr(C)]` sized to exactly
    // `META_EXT_RECORD_SIZE`, matching the mmap reader's layout.
    unsafe {
        std::slice::from_raw_parts(
            record as *const MetaExtRecord as *const u8,
            super::META_EXT_RECORD_SIZE,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn write_release_files(
    staged_dir: &Path,
    centroids: &[u8],
    projection: &[u8],
    turbo_static: &[u8],
    turbo_static_meta: &[u8],
    turbo_static_text: &[u8],
    turbo_static_title: &[u8],
    turbo_static_meta_ext: &[u8],
    turbo_static_docid: &[u8],
    turbo_static_meta_json: &[u8],
) -> Result<(), IndexError> {
    write_file(&staged_dir.join("centroids.bin"), centroids)?;
    write_file(&staged_dir.join("projection.bin"), projection)?;
    write_file(&staged_dir.join("turbo_static.bin"), turbo_static)?;
    write_file(&staged_dir.join("turbo_static_meta.bin"), turbo_static_meta)?;
    write_file(&staged_dir.join("turbo_static_text.bin"), turbo_static_text)?;
    write_file(&staged_dir.join("turbo_static_title.bin"), turbo_static_title)?;
    write_file(
        &staged_dir.join("turbo_static_meta_ext.bin"),
        turbo_static_meta_ext,
    )?;
    write_file(&staged_dir.join("turbo_static_docid.bin"), turbo_static_docid)?;
    write_file(
        &staged_dir.join("turbo_static_meta_json.bin"),
        turbo_static_meta_json,
    )
}

/// Hashes every `.bin` in the staged directory into `OutputFile`s sorted by
/// name ascending. Excludes `release_manifest.json`, which is written after and
/// cannot describe its own hash.
fn collect_outputs(staged_dir: &Path) -> Result<Vec<OutputFile>, IndexError> {
    let names = [
        "centroids.bin",
        "projection.bin",
        "turbo_static.bin",
        "turbo_static_docid.bin",
        "turbo_static_meta.bin",
        "turbo_static_meta_ext.bin",
        "turbo_static_meta_json.bin",
        "turbo_static_text.bin",
        "turbo_static_title.bin",
    ];
    // `names` is authored in ascending order; assert it so a future edit that
    // breaks the ordering fails loudly instead of silently changing release_id.
    debug_assert!(names.windows(2).all(|pair| pair[0] < pair[1]));

    let mut outputs = Vec::with_capacity(names.len());
    for name in names {
        let bytes = fs::read(staged_dir.join(name)).map_err(|error| IndexError::Operation {
            message: format!("failed to read staged output {name}: {error}"),
        })?;
        outputs.push(OutputFile {
            name: name.to_string(),
            sha256: sha256_hex(&bytes),
            size_bytes: bytes.len() as u64,
        });
    }
    Ok(outputs)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), IndexError> {
    fs::write(path, bytes).map_err(|error| IndexError::Operation {
        message: format!("failed to write {}: {error}", path.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_hash_collisions_flags_two_docids_sharing_a_hash() {
        // Synthetic collision: two distinct doc_ids mapped to the same hash.
        let collision = [("a".to_string(), 7u64), ("b".to_string(), 7u64)];
        assert!(
            detect_hash_collisions(&collision).is_err(),
            "two distinct doc_ids sharing a hash must be rejected"
        );

        // Distinct hashes are fine.
        let clean = [("a".to_string(), 1u64), ("b".to_string(), 2u64)];
        assert!(
            detect_hash_collisions(&clean).is_ok(),
            "distinct hashes must be accepted"
        );

        // The same doc_id repeating a hash is not a collision (duplicate-doc_id
        // detection owns that case); this stays Ok.
        let same_doc = [("a".to_string(), 7u64), ("a".to_string(), 7u64)];
        assert!(
            detect_hash_collisions(&same_doc).is_ok(),
            "same doc_id repeated is not a hash collision"
        );
    }
}
