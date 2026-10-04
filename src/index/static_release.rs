//! Static-release writer.
//!
//! [`StaticReleaseBuilder`] turns pre-embedded, doc_id-sorted chunks into a
//! self-describing TurboQuant static release: the records, the codec's
//! assets, the text and title blobs, the sidecars (original doc_id,
//! canonicalized metadata JSON, `MetaExtRecord`) and a deterministic
//! [`ReleaseManifest`].
//!
//! A [`StaticReleaseFormat`] picks the codec and with it the record layout
//! and the asset files; everything else is the same in both formats:
//!
//! | format | codec              | records              | assets ([file list](release_output_files)) |
//! |--------|--------------------|----------------------|--------------------------------------------|
//! | v3     | `Legacy3BitV1`     | `TurboRecord512`     | `centroids.bin`, `projection.bin`          |
//! | v4     | `TurboQuantProdV1` | `TurboProdRecord512` | `codebook.bin`, `qjl.bin`, `rotation.bin`  |
//!
//! Embeddings arrive as a plain `&[Vec<f32>]` with no `Option`: a missing
//! embedding is unrepresentable here, so re-embedding cannot leak into the
//! release path. The v3 format and the v2 writer both take their codec
//! parameters from [`TurboQuantConfig::legacy_v1`], so both encode
//! identically.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::error::IndexError;
use crate::storage::staged_publish::{append_cleanup_failure, StagedDir};

use super::release_manifest::{
    canonical_metadata_json, content_digest, derive_release_id, sha256_hex, CanonicalRow,
    EmbeddingProfile, InputFingerprint, ManifestCodec, OutputFile, ReleaseManifest, ReleaseSource,
    RELEASE_MANIFEST_FILE,
};
use super::static_builder::{
    corpus_type_id, encode_turbo_record, legacy_codec_assets, meta_record_bytes,
    stable_hash_doc_id, turbo_record_bytes, StaticChunk,
};
use super::{
    CentroidTable, HeaderCodec, MetaExtRecord, MetaRecord, ProjectionMatrix, TurboHeader,
    TurboProdRecord512, TurboQuantConfig, TurboQuantProdV1, CODEBOOK_FILE, QJL_FILE, ROTATION_FILE,
    TURBO_VERSION_V3, TURBO_VERSION_V4,
};

/// The exact set of `.bin` artifact file names a v3 static release must
/// contain, in ascending `name` order.
///
/// This is the single source of truth for both the writer (which hashes
/// exactly these files into `outputs[]`) and the verify layer (which rejects
/// any manifest whose `outputs[]` names deviate from this set). Keeping one
/// const prevents the two sides from drifting.
pub const V3_RELEASE_OUTPUT_FILES: [&str; 9] = [
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

/// The `.bin` artifact file names of a v4 static release, in ascending `name`
/// order: [`V3_RELEASE_OUTPUT_FILES`] with the legacy codec's two assets
/// replaced by the three of `TurboQuantProdV1`.
pub const V4_RELEASE_OUTPUT_FILES: [&str; 10] = [
    CODEBOOK_FILE,
    QJL_FILE,
    ROTATION_FILE,
    "turbo_static.bin",
    "turbo_static_docid.bin",
    "turbo_static_meta.bin",
    "turbo_static_meta_ext.bin",
    "turbo_static_meta_json.bin",
    "turbo_static_text.bin",
    "turbo_static_title.bin",
];

/// The `.bin` files a release of `turbo_version` consists of, in ascending
/// name order; `None` for a version that isn't a release format. The writer,
/// the verify layer and the uploader all take their file list from here.
pub fn release_output_files(turbo_version: u32) -> Option<&'static [&'static str]> {
    match turbo_version {
        TURBO_VERSION_V3 => Some(&V3_RELEASE_OUTPUT_FILES),
        TURBO_VERSION_V4 => Some(&V4_RELEASE_OUTPUT_FILES),
        _ => None,
    }
}

/// The format a static release is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StaticReleaseFormat {
    /// `turbo_version` 3: the `Legacy3BitV1` codec of
    /// [`TurboQuantConfig::legacy_v1`].
    #[default]
    V3,
    /// `turbo_version` 4: the `TurboQuantProdV1` codec this config names.
    V4(TurboQuantConfig),
}

impl StaticReleaseFormat {
    /// The format `name` selects in a `static-build` config: `"v3"`, or
    /// `"v4"` for [`TurboQuantConfig::prod_v1`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "v3" => Some(Self::V3),
            "v4" => Some(Self::V4(TurboQuantConfig::prod_v1())),
            _ => None,
        }
    }

    pub const fn turbo_version(&self) -> u32 {
        match self {
            Self::V3 => TURBO_VERSION_V3,
            Self::V4(_) => TURBO_VERSION_V4,
        }
    }

    /// The config of the codec the format's records are encoded with.
    pub const fn codec_config(&self) -> TurboQuantConfig {
        match self {
            Self::V3 => TurboQuantConfig::legacy_v1(),
            Self::V4(config) => *config,
        }
    }
}

/// A release's codec with the assets generated for it: everything that
/// differs between the formats while a release is written.
enum ReleaseCodec {
    V3 {
        config: TurboQuantConfig,
        centroids: CentroidTable,
        projection: ProjectionMatrix,
    },
    V4(TurboQuantProdV1),
}

impl ReleaseCodec {
    fn generate(format: StaticReleaseFormat) -> Result<Self, IndexError> {
        match format {
            StaticReleaseFormat::V3 => {
                let config = format.codec_config();
                let (centroids, projection) = legacy_codec_assets(&config);
                Ok(Self::V3 {
                    config,
                    centroids,
                    projection,
                })
            }
            StaticReleaseFormat::V4(config) => {
                let codec =
                    TurboQuantProdV1::generate(config).map_err(|error| IndexError::Operation {
                        message: format!("invalid v4 codec config: {error}"),
                    })?;
                if !TurboProdRecord512::holds_codes_of(&codec) {
                    return Err(IndexError::Operation {
                        message: format!(
                            "v4 records hold 128 index bytes and 64 sign bytes, but the codec \
                             config produces {} and {}",
                            codec.idx_len(),
                            codec.signs_len()
                        ),
                    });
                }
                Ok(Self::V4(codec))
            }
        }
    }

    /// The codec's asset files, by release file name.
    fn asset_files(&self) -> Vec<(&'static str, Vec<u8>)> {
        match self {
            Self::V3 {
                centroids,
                projection,
                ..
            } => vec![
                ("centroids.bin", centroids.to_bytes()),
                ("projection.bin", projection.to_bytes()),
            ],
            Self::V4(codec) => vec![
                (CODEBOOK_FILE, codec.codebook().to_bytes()),
                (QJL_FILE, codec.qjl().to_bytes()),
                (ROTATION_FILE, codec.rotation().to_bytes()),
            ],
        }
    }

    /// The `turbo_static.bin` header. A v4 header carries the fingerprint of
    /// `asset_files`, which ties the records to these assets.
    fn header(&self, record_count: u64, asset_files: &[(&'static str, Vec<u8>)]) -> TurboHeader {
        match self {
            Self::V3 { config, .. } => TurboHeader::new_v3(config.dim, record_count),
            Self::V4(codec) => {
                let config = codec.config();
                let files: Vec<(&str, &[u8])> = asset_files
                    .iter()
                    .map(|(name, bytes)| (*name, bytes.as_slice()))
                    .collect();
                TurboHeader::new_v4(
                    config.dim,
                    record_count,
                    HeaderCodec {
                        codec_id: config.codec_id,
                        fingerprint: config.fingerprint(&files),
                    },
                )
            }
        }
    }

    /// Encodes `embedding` and appends its record to `out`. `label` is the
    /// chunk's doc_id, for error messages.
    fn append_record(
        &self,
        out: &mut Vec<u8>,
        doc_hash: u64,
        embedding: &[f32],
        label: &str,
    ) -> Result<(), IndexError> {
        match self {
            Self::V3 {
                centroids,
                projection,
                ..
            } => {
                let record =
                    encode_turbo_record(doc_hash, embedding, centroids, projection, label)?;
                out.extend_from_slice(turbo_record_bytes(&record));
            }
            Self::V4(codec) => {
                let encoded = codec
                    .encode(embedding)
                    .map_err(|error| IndexError::Operation {
                        message: format!("failed to encode static chunk {label}: {error}"),
                    })?;
                // `generate` checked that the codec's codes fit the record.
                let record = TurboProdRecord512::new(doc_hash, &encoded).ok_or_else(|| {
                    IndexError::Operation {
                        message: format!(
                            "static chunk {label} produced a code that does not fit a v4 record"
                        ),
                    }
                })?;
                out.extend_from_slice(record.as_bytes());
            }
        }
        Ok(())
    }

    fn manifest_codec(&self) -> Result<ManifestCodec, IndexError> {
        match self {
            Self::V3 { config, .. } => config
                .to_v3_codec_metadata()
                .map(ManifestCodec::V3)
                .map_err(|error| IndexError::Operation {
                    message: format!("invalid v3 codec config: {error}"),
                }),
            Self::V4(codec) => codec
                .config()
                .to_v4_codec_metadata()
                .map(ManifestCodec::V4)
                .map_err(|error| IndexError::Operation {
                    message: format!("invalid v4 codec config: {error}"),
                }),
        }
    }
}

/// Writes self-describing TurboQuant static releases in one
/// [`StaticReleaseFormat`]. `default()` writes the default format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StaticReleaseBuilder {
    format: StaticReleaseFormat,
}

impl StaticReleaseBuilder {
    pub const fn new(format: StaticReleaseFormat) -> Self {
        Self { format }
    }

    pub const fn format(&self) -> StaticReleaseFormat {
        self.format
    }

    /// Builds a static release into `output_dir`, atomically replacing any
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
        let turbo_version = self.format.turbo_version();
        let codec_config = self.format.codec_config();

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
        if profile.dim != codec_config.dim {
            return Err(IndexError::Operation {
                message: format!(
                    "static release only supports typed turbo layout for {}-dim embeddings, profile declares {}",
                    codec_config.dim, profile.dim
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

        // --- Step 2: codec assets (v3: identical seeds/params to the v2 writer) -
        let codec = ReleaseCodec::generate(self.format)?;
        let asset_files = codec.asset_files();

        // --- Step 3: single-pass byte construction (order == chunk order) ------
        let mut turbo_static = codec.header(chunks.len() as u64, &asset_files).to_bytes();
        let mut turbo_static_meta = Vec::new();
        let mut turbo_static_text = Vec::new();
        let mut turbo_static_title = Vec::new();
        let mut turbo_static_meta_ext = Vec::new();
        let mut turbo_static_docid = Vec::new();
        let mut turbo_static_meta_json = Vec::new();
        let mut canonical_rows = Vec::with_capacity(chunks.len());

        for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
            let doc_hash = stable_hash_doc_id(&chunk.doc_id);
            codec.append_record(&mut turbo_static, doc_hash, embedding, &chunk.doc_id)?;

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

        // --- Step 4: stage-and-write the .bin artifacts ------------------------
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

        let mut files: Vec<(&str, &[u8])> = asset_files
            .iter()
            .map(|(name, bytes)| (*name, bytes.as_slice()))
            .collect();
        files.extend([
            ("turbo_static.bin", turbo_static.as_slice()),
            ("turbo_static_meta.bin", turbo_static_meta.as_slice()),
            ("turbo_static_text.bin", turbo_static_text.as_slice()),
            ("turbo_static_title.bin", turbo_static_title.as_slice()),
            (
                "turbo_static_meta_ext.bin",
                turbo_static_meta_ext.as_slice(),
            ),
            ("turbo_static_docid.bin", turbo_static_docid.as_slice()),
            (
                "turbo_static_meta_json.bin",
                turbo_static_meta_json.as_slice(),
            ),
        ]);
        let write_result = files
            .iter()
            .try_for_each(|(name, bytes)| write_file(&staged.path().join(name), bytes));
        if let Err(error) = write_result {
            return Err(append_cleanup_failure(error, staged.abort()));
        }

        // --- Step 5: hash the staged files (name-ascending, manifest excluded) -
        // Reading the format's own file list back fails the build if the
        // files written above ever stop matching it.
        let output_files =
            release_output_files(turbo_version).expect("every release format has a file list");
        let outputs = match collect_outputs(staged.path(), output_files) {
            Ok(outputs) => outputs,
            Err(error) => return Err(append_cleanup_failure(error, staged.abort())),
        };

        // --- Step 6: content fingerprint + codec metadata + release_id ---------
        let input_fingerprint = InputFingerprint {
            doc_count: chunks.len() as u64,
            content_digest: content_digest(&canonical_rows),
        };
        let codec = match codec.manifest_codec() {
            Ok(codec) => codec,
            Err(error) => return Err(append_cleanup_failure(error, staged.abort())),
        };
        let release_id = derive_release_id(
            turbo_version,
            profile,
            &codec,
            &input_fingerprint.content_digest,
            &outputs,
        );

        // --- Step 7: assemble + serialize the manifest (compact, deterministic)
        let manifest = ReleaseManifest {
            manifest_schema_version: 1,
            turbo_version,
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
        if let Err(error) = write_file(&staged.path().join(RELEASE_MANIFEST_FILE), &manifest_bytes)
        {
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

/// Hashes the `names` files in the staged directory into `OutputFile`s sorted
/// by name ascending. `release_manifest.json` is never among them: it is
/// written after and cannot describe its own hash.
fn collect_outputs(staged_dir: &Path, names: &[&str]) -> Result<Vec<OutputFile>, IndexError> {
    // `names` is authored in ascending order; assert it so a future edit that
    // breaks the ordering fails loudly instead of silently changing release_id.
    debug_assert!(names.windows(2).all(|pair| pair[0] < pair[1]));

    let mut outputs = Vec::with_capacity(names.len());
    for &name in names {
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
