use std::collections::HashMap;
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::embedding::{EmbeddingError, EmbeddingGenerator};
use crate::error::IndexError;
use crate::models::CorpusType;

use super::{
    encode_vector, CentroidTable, MetaRecord, ProjectionMatrix, TurboHeader, META_RECORD_SIZE,
};

const CENTROIDS_PER_DIM: u32 = 4;
const CENTROIDS_SEED: u64 = 7;
const PROJECTION_SEED: u64 = 11;
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

        fs::create_dir_all(output_dir).map_err(|error| IndexError::Operation {
            message: format!(
                "failed to create static output directory {}: {error}",
                output_dir.display()
            ),
        })?;

        let staged = staged_output_dir(output_dir)?;
        fs::create_dir_all(&staged).map_err(|error| IndexError::Operation {
            message: format!(
                "failed to create staged static output directory {}: {error}",
                staged.display()
            ),
        })?;

        let centroids = CentroidTable::generate(embedding_dim, CENTROIDS_PER_DIM, CENTROIDS_SEED);
        let projection = ProjectionMatrix::generate(embedding_dim, embedding_dim, PROJECTION_SEED);
        let header = TurboHeader::new(embedding_dim, chunks.len() as u64);

        let mut turbo_static = header.to_bytes();
        let mut turbo_static_meta = Vec::with_capacity(chunks.len() * META_RECORD_SIZE);
        let mut turbo_static_text = Vec::new();

        for (chunk, embedding) in chunks.iter().zip(resolved_embeddings.iter()) {
            let encoded = encode_vector(embedding, &centroids, &projection).map_err(|error| {
                IndexError::Operation {
                    message: format!("failed to encode static chunk {}: {error}", chunk.doc_id),
                }
            })?;

            let mut record = vec![0u8; header.record_stride()];
            let doc_id = parse_doc_id(&chunk.doc_id)?;
            record[0..8].copy_from_slice(&doc_id.to_le_bytes());
            record[header.idx_offset()..header.idx_offset() + encoded.idx.len()]
                .copy_from_slice(&encoded.idx);
            record[header.qjl_offset()..header.qjl_offset() + encoded.qjl.len()]
                .copy_from_slice(&encoded.qjl);
            record[header.gamma_offset()..header.gamma_offset() + 4]
                .copy_from_slice(&encoded.gamma.to_le_bytes());
            turbo_static.extend_from_slice(&record);

            let text_offset = turbo_static_text.len() as u64;
            turbo_static_text.extend_from_slice(chunk.text.as_bytes());
            let meta = MetaRecord {
                doc_id,
                corpus_type: corpus_type_id(&chunk.corpus_type),
                _pad: [0; 3],
                text_offset,
                text_len: chunk.text.len() as u32,
            };
            let meta_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    &meta as *const MetaRecord as *const u8,
                    META_RECORD_SIZE,
                )
            };
            turbo_static_meta.extend_from_slice(meta_bytes);
        }

        write_file(&staged.join("centroids.bin"), &centroids.to_bytes())?;
        write_file(&staged.join("projection.bin"), &projection.to_bytes())?;
        write_file(&staged.join("turbo_static.bin"), &turbo_static)?;
        write_file(&staged.join("turbo_static_meta.bin"), &turbo_static_meta)?;
        write_file(&staged.join("turbo_static_text.bin"), &turbo_static_text)?;

        publish_staged_output(output_dir, &staged)?;

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

fn stable_hash_doc_id(doc_id: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in doc_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn corpus_type_id(corpus_type: &CorpusType) -> u8 {
    match corpus_type {
        CorpusType::Legal => 0,
        CorpusType::Contract => 1,
        CorpusType::Rfc => 2,
        CorpusType::Other(id) => *id,
    }
}

fn staged_output_dir(output_dir: &Path) -> Result<PathBuf, IndexError> {
    let parent = output_dir.parent().ok_or_else(|| IndexError::Operation {
        message: format!("path {} has no parent", output_dir.display()),
    })?;
    let name = output_dir
        .file_name()
        .ok_or_else(|| IndexError::Operation {
            message: format!("path {} has no file name", output_dir.display()),
        })?
        .to_string_lossy();

    Ok(parent.join(format!(".{name}.staging")))
}

fn publish_staged_output(output_dir: &Path, staged: &Path) -> Result<(), IndexError> {
    for file_name in [
        "centroids.bin",
        "projection.bin",
        "turbo_static.bin",
        "turbo_static_meta.bin",
        "turbo_static_text.bin",
    ] {
        let source = staged.join(file_name);
        let destination = output_dir.join(file_name);
        fs::rename(&source, &destination).map_err(|error| IndexError::Operation {
            message: format!(
                "failed to publish staged static artifact {} to {}: {error}",
                source.display(),
                destination.display()
            ),
        })?;
    }

    let _ = fs::remove_dir_all(staged);
    Ok(())
}
