use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use arrow_array::types::Float32Type;
use arrow_array::{
    FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, RecordBatchReader,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use serde::Serialize;
use tantivy::doc;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{Index, IndexWriter};

use crate::embedding::EmbeddingGenerator;
use crate::error::IndexError;
use crate::models::{Document, IndexManifest, ShardManifest, WalOperation, WalRecord};
use crate::storage::staged_publish::{append_cleanup_failure, StagedDir};
use crate::storage::version_manifest_key;

const ARTIFACT_BUCKET: &str = "local-artifacts";
const SHARD_ID: u32 = 0;
const DOC_ID_FIELD: &str = "doc_id";
const TEXT_FIELD: &str = "text";
const METADATA_FIELD: &str = "metadata";
const TIMESTAMP_FIELD: &str = "timestamp";
const EMBEDDING_FIELD: &str = "embedding";
const LANCE_TABLE_NAME: &str = "documents";

type BeforePublishHook = Arc<dyn Fn() -> Result<(), IndexError> + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub struct BuildIndexRequest {
    pub version_id: u64,
    pub created_at: i64,
    pub embedding_dim: usize,
    pub records: Vec<WalRecord>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BuildIndexResult {
    pub manifest: IndexManifest,
    pub documents: Vec<Document>,
}

#[derive(Clone)]
pub struct LocalIndexBuilder<E> {
    artifact_root: PathBuf,
    embedding_generator: E,
    before_publish_hook: Option<BeforePublishHook>,
}

impl<E> LocalIndexBuilder<E>
where
    E: EmbeddingGenerator,
{
    pub fn new(artifact_root: impl AsRef<Path>, embedding_generator: E) -> Self {
        Self {
            artifact_root: artifact_root.as_ref().to_path_buf(),
            embedding_generator,
            before_publish_hook: None,
        }
    }

    pub fn with_before_publish_hook<F>(mut self, hook: F) -> Self
    where
        F: Fn() -> Result<(), IndexError> + Send + Sync + 'static,
    {
        self.before_publish_hook = Some(Arc::new(hook));
        self
    }

    pub fn build(&self, request: &BuildIndexRequest) -> Result<BuildIndexResult, IndexError> {
        if request.version_id == 0 {
            return Err(IndexError::Operation {
                message: "version_id must be positive".into(),
            });
        }

        let mut documents = materialize_latest_snapshot(&request.records)?;
        for document in &mut documents {
            if document.embedding.is_none() {
                let embedding =
                    self.embedding_generator
                        .generate(&document.text)
                        .map_err(|source| IndexError::Operation {
                            message: source.to_string(),
                        })?;
                document.embedding = Some(embedding);
            }

            document.validate_for_embedding_dim(request.embedding_dim)?;
        }

        let manifest = build_manifest(request, documents.len())?;
        let staged_build = self.stage_build(request.version_id, &documents, &manifest)?;
        self.publish_staged_build(staged_build)?;

        Ok(BuildIndexResult {
            manifest,
            documents,
        })
    }

    fn stage_build(
        &self,
        version_id: u64,
        documents: &[Document],
        manifest: &IndexManifest,
    ) -> Result<StagedBuild, IndexError> {
        let staged_build = StagedBuild::new(&self.artifact_root, version_id)?;

        match self.write_staged_build(&staged_build, documents, manifest) {
            Ok(()) => Ok(staged_build),
            Err(error) => Err(append_cleanup_failure(error, staged_build.dir.abort())),
        }
    }

    fn write_staged_build(
        &self,
        staged_build: &StagedBuild,
        documents: &[Document],
        manifest: &IndexManifest,
    ) -> Result<(), IndexError> {
        ensure_target_is_publishable(&staged_build.final_lance_dir)?;
        ensure_target_is_publishable(&staged_build.final_index_dir)?;
        ensure_target_is_publishable(&staged_build.final_manifest_path)?;

        self.write_lance_artifact(
            &staged_build.staged_lance_dir,
            documents,
            manifest.embedding_dim,
        )?;
        self.write_keyword_index(&staged_build.staged_index_dir, documents)?;
        write_json_file(&staged_build.staged_manifest_path, manifest)
    }

    fn publish_staged_build(&self, staged_build: StagedBuild) -> Result<(), IndexError> {
        if let Some(hook) = &self.before_publish_hook {
            if let Err(error) = hook() {
                return Err(append_cleanup_failure(error, staged_build.dir.abort()));
            }
        }

        let StagedBuild {
            dir,
            staged_lance_dir,
            staged_index_dir,
            staged_manifest_path,
            final_lance_dir,
            final_index_dir,
            final_manifest_path,
        } = staged_build;

        dir.commit(vec![
            (staged_lance_dir, final_lance_dir),
            (staged_index_dir, final_index_dir),
            (staged_manifest_path, final_manifest_path),
        ])
    }

    fn write_lance_artifact(
        &self,
        shard_dir: &Path,
        documents: &[Document],
        embedding_dim: usize,
    ) -> Result<(), IndexError> {
        fs::create_dir_all(shard_dir).map_err(|source| IndexError::Operation {
            message: format!(
                "failed to create LanceDB artifact directory {}: {source}",
                shard_dir.display()
            ),
        })?;

        let shard_dir = shard_dir.to_path_buf();
        let shard_dir_string = shard_dir.to_string_lossy().into_owned();
        let documents = documents.to_vec();

        run_lance_build(async move {
            let conn = lancedb::connect(&shard_dir_string)
                .execute()
                .await
                .map_err(|source| IndexError::Operation {
                    message: format!(
                        "failed to connect local LanceDB artifact at {}: {source}",
                        shard_dir.display()
                    ),
                })?;

            let schema = Arc::new(ArrowSchema::new(vec![
                Field::new(DOC_ID_FIELD, DataType::Utf8, false),
                Field::new(TEXT_FIELD, DataType::Utf8, false),
                Field::new(METADATA_FIELD, DataType::Utf8, false),
                Field::new(TIMESTAMP_FIELD, DataType::Int64, false),
                Field::new(
                    EMBEDDING_FIELD,
                    DataType::FixedSizeList(
                        Arc::new(Field::new("item", DataType::Float32, true)),
                        embedding_dim as i32,
                    ),
                    true,
                ),
            ]));

            if documents.is_empty() {
                conn.create_empty_table(LANCE_TABLE_NAME, schema)
                    .execute()
                    .await
                    .map_err(|source| IndexError::Operation {
                        message: format!(
                            "failed to create empty LanceDB table at {}: {source}",
                            shard_dir.display()
                        ),
                    })?;
                return Ok(());
            }

            let doc_ids = StringArray::from(
                documents
                    .iter()
                    .map(|document| Some(document.doc_id.as_str()))
                    .collect::<Vec<_>>(),
            );
            let texts = StringArray::from(
                documents
                    .iter()
                    .map(|document| Some(document.text.as_str()))
                    .collect::<Vec<_>>(),
            );
            let metadata_json = StringArray::from(
                documents
                    .iter()
                    .map(|document| serde_json::to_string(&document.metadata))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|source| IndexError::Operation {
                        message: format!(
                            "failed to serialize metadata for LanceDB artifact in {}: {source}",
                            shard_dir.display()
                        ),
                    })?,
            );
            let timestamps = Int64Array::from(
                documents
                    .iter()
                    .map(|document| document.timestamp)
                    .collect::<Vec<_>>(),
            );
            let embeddings = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                documents.iter().map(|document| {
                    document
                        .embedding
                        .as_ref()
                        .map(|embedding| embedding.iter().copied().map(Some).collect::<Vec<_>>())
                }),
                embedding_dim as i32,
            );

            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(doc_ids),
                    Arc::new(texts),
                    Arc::new(metadata_json),
                    Arc::new(timestamps),
                    Arc::new(embeddings),
                ],
            )
            .map_err(|source| IndexError::Operation {
                message: format!(
                    "failed to build LanceDB record batch at {}: {source}",
                    shard_dir.display()
                ),
            })?;
            // lancedb 0.31 requires `create_table` inputs to implement `Scannable`;
            // a bare `RecordBatchIterator` no longer qualifies, so box it as a
            // `RecordBatchReader` (which does) while preserving the explicit schema.
            let batches: Box<dyn RecordBatchReader + Send> = Box::new(RecordBatchIterator::new(
                vec![Ok(batch)].into_iter(),
                schema,
            ));

            conn.create_table(LANCE_TABLE_NAME, batches)
                .execute()
                .await
                .map_err(|source| IndexError::Operation {
                    message: format!(
                        "failed to create LanceDB table at {}: {source}",
                        shard_dir.display()
                    ),
                })?;

            Ok(())
        })
    }

    fn write_keyword_index(
        &self,
        index_path: &Path,
        documents: &[Document],
    ) -> Result<(), IndexError> {
        fs::create_dir_all(index_path).map_err(|source| IndexError::Operation {
            message: format!(
                "failed to create Tantivy artifact directory {}: {source}",
                index_path.display()
            ),
        })?;

        let mut schema_builder = Schema::builder();
        let doc_id = schema_builder.add_text_field(DOC_ID_FIELD, TEXT | STORED);
        let text = schema_builder.add_text_field(TEXT_FIELD, TEXT | STORED);
        let metadata = schema_builder.add_text_field(METADATA_FIELD, STORED);
        let schema = schema_builder.build();

        let index =
            Index::create_in_dir(index_path, schema).map_err(|source| IndexError::Operation {
                message: format!(
                    "failed to create Tantivy index at {}: {source}",
                    index_path.display()
                ),
            })?;
        let mut writer: IndexWriter =
            index
                .writer(15_000_000)
                .map_err(|source| IndexError::Operation {
                    message: format!(
                        "failed to open Tantivy writer at {}: {source}",
                        index_path.display()
                    ),
                })?;

        for document in documents {
            let metadata_json = serde_json::to_string(&document.metadata).map_err(|source| {
                IndexError::Operation {
                    message: format!(
                        "failed to serialize metadata for doc {}: {source}",
                        document.doc_id
                    ),
                }
            })?;

            writer
                .add_document(doc!(
                    doc_id => document.doc_id.clone(),
                    text => document.text.clone(),
                    metadata => metadata_json,
                ))
                .map_err(|source| IndexError::Operation {
                    message: format!(
                        "failed to add Tantivy document {}: {source}",
                        document.doc_id
                    ),
                })?;
        }

        writer.commit().map_err(|source| IndexError::Operation {
            message: format!(
                "failed to commit Tantivy index at {}: {source}",
                index_path.display()
            ),
        })?;

        Ok(())
    }
}

impl<E> std::fmt::Debug for LocalIndexBuilder<E>
where
    E: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalIndexBuilder")
            .field("artifact_root", &self.artifact_root)
            .field("embedding_generator", &self.embedding_generator)
            .finish_non_exhaustive()
    }
}

pub fn materialize_latest_snapshot(records: &[WalRecord]) -> Result<Vec<Document>, IndexError> {
    let mut latest_by_doc_id: HashMap<&str, &WalRecord> = HashMap::new();

    for record in records {
        record.validate()?;

        match latest_by_doc_id.get(record.doc_id.as_str()) {
            Some(current) if current.timestamp > record.timestamp => {}
            _ => {
                latest_by_doc_id.insert(record.doc_id.as_str(), record);
            }
        }
    }

    let mut documents = latest_by_doc_id
        .into_values()
        .filter_map(|record| match record.op {
            WalOperation::Upsert => record.document.clone(),
            WalOperation::Delete => None,
        })
        .collect::<Vec<_>>();
    documents.sort_by(|left, right| left.doc_id.cmp(&right.doc_id));

    Ok(documents)
}

#[derive(Debug)]
struct StagedBuild {
    dir: StagedDir,
    staged_lance_dir: PathBuf,
    staged_index_dir: PathBuf,
    staged_manifest_path: PathBuf,
    final_lance_dir: PathBuf,
    final_index_dir: PathBuf,
    final_manifest_path: PathBuf,
}

impl StagedBuild {
    fn new(artifact_root: &Path, version_id: u64) -> Result<Self, IndexError> {
        let dir = StagedDir::create(artifact_root, "index-build")?;
        let root = dir.path();

        Ok(Self {
            staged_lance_dir: root.join(format!("lance/v{version_id}/shard_0")),
            staged_index_dir: root.join(format!("index/v{version_id}/shard_0")),
            staged_manifest_path: root.join(version_manifest_key(version_id)),
            final_lance_dir: artifact_root.join(format!("lance/v{version_id}/shard_0")),
            final_index_dir: artifact_root.join(format!("index/v{version_id}/shard_0")),
            final_manifest_path: artifact_root.join(version_manifest_key(version_id)),
            dir,
        })
    }
}

fn build_manifest(
    request: &BuildIndexRequest,
    document_count: usize,
) -> Result<IndexManifest, IndexError> {
    let manifest = IndexManifest {
        version_id: request.version_id,
        created_at: request.created_at,
        embedding_dim: request.embedding_dim,
        document_count,
        num_shards: 1,
        shards: vec![ShardManifest {
            shard_id: SHARD_ID,
            document_count,
            lance_path: format!(
                "s3://{ARTIFACT_BUCKET}/lance/v{}/shard_0",
                request.version_id
            ),
            tantivy_path: format!(
                "s3://{ARTIFACT_BUCKET}/index/v{}/shard_0",
                request.version_id
            ),
        }],
    };
    manifest.validate()?;
    Ok(manifest)
}

fn write_json_file<T>(path: &Path, value: &T) -> Result<(), IndexError>
where
    T: Serialize,
{
    let parent = path.parent().ok_or_else(|| IndexError::Operation {
        message: format!("path {} has no parent", path.display()),
    })?;
    fs::create_dir_all(parent).map_err(|source| IndexError::Operation {
        message: format!("failed to create directory {}: {source}", parent.display()),
    })?;

    let contents = serde_json::to_string_pretty(value).map_err(|source| IndexError::Operation {
        message: format!("failed to serialize json for {}: {source}", path.display()),
    })?;
    fs::write(path, contents).map_err(|source| IndexError::Operation {
        message: format!("failed to write {}: {source}", path.display()),
    })?;

    Ok(())
}

fn ensure_target_is_publishable(path: &Path) -> Result<(), IndexError> {
    if path.exists() {
        return Err(IndexError::Operation {
            message: format!("publish target already exists: {}", path.display()),
        });
    }

    let parent = path.parent().ok_or_else(|| IndexError::Operation {
        message: format!("path {} has no parent", path.display()),
    })?;
    if parent.exists() && !parent.is_dir() {
        return Err(IndexError::Operation {
            message: format!(
                "publish target parent is not a directory: {}",
                parent.display()
            ),
        });
    }

    Ok(())
}

fn run_lance_build<F>(future: F) -> Result<(), IndexError>
where
    F: std::future::Future<Output = Result<(), IndexError>> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|source| IndexError::Operation {
                    message: format!("failed to create tokio runtime for LanceDB build: {source}"),
                })?
                .block_on(future)
        })
        .join()
        .map_err(|panic| IndexError::Operation {
            message: format!("LanceDB build thread panicked: {}", panic_message(panic)),
        })?
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|source| IndexError::Operation {
                message: format!("failed to create tokio runtime for LanceDB build: {source}"),
            })?
            .block_on(future)
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".into()
    }
}
