use std::collections::HashMap;
use std::fs;
use std::marker::PhantomData;
use std::path::Path;

use serde_json::Value;

use crate::embedding::{EmbeddingError, EmbeddingGenerator};
use crate::error::IndexError;
use crate::models::CorpusType;
use crate::storage::staged_publish::{append_cleanup_failure, StagedDir};

use super::{
    encode_vector, CentroidTable, MetaRecord, ProjectionMatrix, TurboHeader, TurboRecord512,
    META_RECORD_SIZE,
};

// Shared with the v3 `StaticReleaseBuilder` so both writers derive the same
// codec assets and doc_id hashes from a single source of truth.
pub(crate) const CENTROIDS_PER_DIM: u32 = 4;
pub(crate) const CENTROIDS_SEED: u64 = 7;
pub(crate) const PROJECTION_SEED: u64 = 11;
pub(crate) const SUPPORTED_TYPED_DIM: u32 = 512;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Debug, Clone, PartialEq)]
pub struct StaticChunk {
    pub doc_id: String,
    pub text: String,
    pub metadata: HashMap<String, Value>,
    pub corpus_type: CorpusType,
}

impl Default for StaticChunk {
    fn default() -> Self {
        Self {
            doc_id: String::new(),
            text: String::new(),
            metadata: HashMap::new(),
            corpus_type: CorpusType::Legal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticIndexBuildResult {
    pub record_count: u64,
    pub embedding_dim: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticIndexBuilder<E = ()> {
    _encoder: PhantomData<E>,
}

impl<E> Default for StaticIndexBuilder<E> {
    fn default() -> Self {
        Self {
            _encoder: PhantomData,
        }
    }
}

impl StaticIndexBuilder<()> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<E> StaticIndexBuilder<E> {
    pub fn build<G>(
        &self,
        output_dir: &Path,
        chunks: &[StaticChunk],
        embeddings: &[Option<Vec<f32>>],
        generator: &G,
    ) -> Result<StaticIndexBuildResult, IndexError>
    where
        G: EmbeddingGenerator,
    {
        if chunks.len() != embeddings.len() {
            return Err(IndexError::Operation {
                message: format!(
                    "static chunk count {} does not match embedding count {}",
                    chunks.len(),
                    embeddings.len()
                ),
            });
        }

        let resolved_embeddings = resolve_embeddings(chunks, embeddings, generator)?;
        let embedding_dim = resolved_embeddings
            .first()
            .map(|embedding| embedding.len() as u32)
            .ok_or_else(|| IndexError::Operation {
                message: "static builder requires at least one chunk".into(),
            })?;
        if embedding_dim != SUPPORTED_TYPED_DIM {
            return Err(IndexError::Operation {
                message: format!(
                    "static builder only supports typed turbo layout for {}-dim embeddings, got {}",
                    SUPPORTED_TYPED_DIM, embedding_dim
                ),
            });
        }

        fs::create_dir_all(output_dir).map_err(|error| IndexError::Operation {
            message: format!(
                "failed to create static output directory {}: {error}",
                output_dir.display()
            ),
        })?;

        let centroids = CentroidTable::generate(embedding_dim, CENTROIDS_PER_DIM, CENTROIDS_SEED);
        let projection = ProjectionMatrix::generate(embedding_dim, embedding_dim, PROJECTION_SEED);
        let header = TurboHeader::new(embedding_dim, chunks.len() as u64);

        let mut turbo_static = header.to_bytes();
        let mut turbo_static_meta = Vec::with_capacity(chunks.len() * META_RECORD_SIZE);
        let mut turbo_static_text = Vec::new();
        let mut turbo_static_title = Vec::new();

        for (chunk, embedding) in chunks.iter().zip(resolved_embeddings.iter()) {
            let doc_id = parse_doc_id(&chunk.doc_id)?;
            let record =
                encode_turbo_record(doc_id, embedding, &centroids, &projection, &chunk.doc_id)?;
            turbo_static.extend_from_slice(turbo_record_bytes(&record));

            let text_offset = turbo_static_text.len() as u64;
            turbo_static_text.extend_from_slice(chunk.text.as_bytes());

            // Title mirrors the text blob: a chunk without a non-empty
            // `metadata["title"]` records `title_len == 0`, which reads back as
            // `None` and degrades to the bare `[法规 #1]` context label.
            let title = chunk
                .metadata
                .get("title")
                .and_then(Value::as_str)
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
                doc_id,
                corpus_type: corpus_type_id(&chunk.corpus_type),
                _pad: [0; 7],
                text_offset,
                text_len: chunk.text.len() as u32,
                title_offset,
                title_len,
            };
            turbo_static_meta.extend_from_slice(meta_record_bytes(&meta));
        }

        // Stage next to the output directory (same filesystem), then swap
        // the whole directory: a failed rebuild never leaves the previous
        // static index partially deleted.
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

        let write_result = write_static_files(
            staged.path(),
            &centroids.to_bytes(),
            &projection.to_bytes(),
            &turbo_static,
            &turbo_static_meta,
            &turbo_static_text,
            &turbo_static_title,
        );
        if let Err(error) = write_result {
            return Err(append_cleanup_failure(error, staged.abort()));
        }

        staged.commit_replace_dir(output_dir)?;

        Ok(StaticIndexBuildResult {
            record_count: chunks.len() as u64,
            embedding_dim,
        })
    }
}

fn resolve_embeddings<G>(
    chunks: &[StaticChunk],
    embeddings: &[Option<Vec<f32>>],
    generator: &G,
) -> Result<Vec<Vec<f32>>, IndexError>
where
    G: EmbeddingGenerator,
{
    let mut resolved = Vec::with_capacity(chunks.len());
    let mut expected_dim = None;

    for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
        let embedding = match embedding {
            Some(embedding) => embedding.clone(),
            None => generator
                .generate(&chunk.text)
                .map_err(|error| match error {
                    EmbeddingError::Generation { message } => IndexError::Operation {
                        message: format!(
                            "failed to generate embedding for static chunk {}: {message}",
                            chunk.doc_id
                        ),
                    },
                })?,
        };

        if embedding.is_empty() {
            return Err(IndexError::Operation {
                message: format!("static chunk {} produced an empty embedding", chunk.doc_id),
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

        match expected_dim {
            Some(dim) if dim != embedding.len() => {
                return Err(IndexError::Operation {
                    message: format!(
                        "static chunk {} embedding dimension {} does not match expected {}",
                        chunk.doc_id,
                        embedding.len(),
                        dim
                    ),
                })
            }
            None => expected_dim = Some(embedding.len()),
            _ => {}
        }

        resolved.push(embedding);
    }

    Ok(resolved)
}

fn write_static_files(
    staged_dir: &Path,
    centroids: &[u8],
    projection: &[u8],
    turbo_static: &[u8],
    turbo_static_meta: &[u8],
    turbo_static_text: &[u8],
    turbo_static_title: &[u8],
) -> Result<(), IndexError> {
    write_file(&staged_dir.join("centroids.bin"), centroids)?;
    write_file(&staged_dir.join("projection.bin"), projection)?;
    write_file(&staged_dir.join("turbo_static.bin"), turbo_static)?;
    write_file(&staged_dir.join("turbo_static_meta.bin"), turbo_static_meta)?;
    write_file(&staged_dir.join("turbo_static_text.bin"), turbo_static_text)?;
    write_file(
        &staged_dir.join("turbo_static_title.bin"),
        turbo_static_title,
    )
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), IndexError> {
    fs::write(path, bytes).map_err(|error| IndexError::Operation {
        message: format!("failed to write {}: {error}", path.display()),
    })
}

fn parse_doc_id(doc_id: &str) -> Result<u64, IndexError> {
    if doc_id.is_empty() {
        return Err(IndexError::Operation {
            message: "static chunk doc_id must not be empty".into(),
        });
    }

    Ok(stable_hash_doc_id(doc_id))
}

pub(crate) fn stable_hash_doc_id(doc_id: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in doc_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Encodes one embedding into a `TurboRecord512`. Shared by the v2
/// `StaticIndexBuilder` and the v3 `StaticReleaseBuilder` so both writers
/// produce byte-identical records for the same input. `label` is only used in
/// error messages (the caller's original doc_id string).
pub(crate) fn encode_turbo_record(
    doc_id: u64,
    embedding: &[f32],
    centroids: &CentroidTable,
    projection: &ProjectionMatrix,
    label: &str,
) -> Result<TurboRecord512, IndexError> {
    let encoded =
        encode_vector(embedding, centroids, projection).map_err(|error| IndexError::Operation {
            message: format!("failed to encode static chunk {label}: {error}"),
        })?;

    Ok(TurboRecord512 {
        doc_id,
        idx: encoded
            .idx
            .clone()
            .try_into()
            .map_err(|_| IndexError::Operation {
                message: format!(
                    "static chunk {label} produced idx payload with unexpected length {}",
                    encoded.idx.len()
                ),
            })?,
        qjl: encoded
            .qjl
            .clone()
            .try_into()
            .map_err(|_| IndexError::Operation {
                message: format!(
                    "static chunk {label} produced qjl payload with unexpected length {}",
                    encoded.qjl.len()
                ),
            })?,
        gamma: encoded.gamma,
        _reserved: [0; 4],
    })
}

/// Views a `TurboRecord512` as its raw `repr(C)` bytes for serialization.
pub(crate) fn turbo_record_bytes(record: &TurboRecord512) -> &[u8] {
    // Safety: `TurboRecord512` is `#[repr(C)]` with no padding beyond its
    // declared fields; reading it as bytes is sound and matches the layout the
    // mmap reader reconstructs.
    unsafe {
        std::slice::from_raw_parts(
            record as *const TurboRecord512 as *const u8,
            std::mem::size_of::<TurboRecord512>(),
        )
    }
}

/// Views a `MetaRecord` as its raw `repr(C)` bytes for serialization.
pub(crate) fn meta_record_bytes(meta: &MetaRecord) -> &[u8] {
    // Safety: `MetaRecord` is `#[repr(C)]` sized to exactly `META_RECORD_SIZE`.
    unsafe { std::slice::from_raw_parts(meta as *const MetaRecord as *const u8, META_RECORD_SIZE) }
}

pub(crate) fn corpus_type_id(corpus_type: &CorpusType) -> u8 {
    match corpus_type {
        CorpusType::Legal => 0,
        CorpusType::Contract => 1,
        CorpusType::Rfc => 2,
        CorpusType::Other(id) => *id,
    }
}
