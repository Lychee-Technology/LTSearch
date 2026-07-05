use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use arrow_schema::DataType;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;

use crate::error::{SearchError, ValidationError};
use crate::models::{CacheStats, SearchRequest, SearchResult, ShardManifest};
use crate::storage::{ActiveManifest, ManifestStore};

use super::lance_cache::{inspect_path_tree_within_artifact_root, LocalLanceCache};
use super::lance_decode::decode_lancedb_batches;
use super::retrieval_common::{
    dedupe_best_by_doc_id, resolve_artifact_path, validate_query_embedding, validate_top_k,
};

const LANCE_TABLE_NAME: &str = "documents";
const EMBEDDING_COLUMN_NAME: &str = "embedding";
const ARTIFACT_KIND: &str = "local LanceDB";

#[derive(Debug, Clone)]
pub struct VectorSearcher<M> {
    manifest_store: M,
    artifact_root: PathBuf,
    cache: Arc<Mutex<LocalLanceCache>>,
}

impl<M> VectorSearcher<M>
where
    M: ManifestStore,
{
    pub fn new(manifest_store: M, artifact_root: impl AsRef<Path>) -> Self {
        Self {
            manifest_store,
            artifact_root: artifact_root.as_ref().to_path_buf(),
            cache: Arc::new(Mutex::new(LocalLanceCache::default())),
        }
    }

    pub fn cache_stats(&self) -> CacheStats {
        let cache = self
            .cache
            .lock()
            .expect("vector searcher cache lock poisoned");
        CacheStats {
            hit_count: cache.hit_count,
            miss_count: cache.miss_count,
            current_version: cache.current_version,
            bytes_used: cache.bytes_used,
        }
    }

    pub fn search(
        &self,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        validate_query_embedding(query_embedding)?;
        validate_top_k(top_k)?;

        let active_manifest = self
            .manifest_store
            .load_active_manifest()
            .map_err(|source| SearchError::Execution {
                message: source.to_string(),
            })?;

        self.search_active_manifest(&active_manifest, query_embedding, top_k)
    }

    pub fn search_active_manifest(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        validate_query_embedding(query_embedding)?;
        validate_top_k(top_k)?;

        if query_embedding.len() != active_manifest.manifest.embedding_dim {
            return Err(SearchError::Validation(ValidationError::InvalidValue {
                field: "query_embedding",
            }));
        }

        self.begin_version_attempt(active_manifest.head.version_id);

        let mut all_results = Vec::new();
        for shard in &active_manifest.manifest.shards {
            all_results.extend(self.query_shard(
                shard,
                active_manifest.head.version_id,
                query_embedding,
                active_manifest.manifest.embedding_dim,
                top_k,
            )?);
        }

        Ok(dedupe_best_by_doc_id(all_results, top_k))
    }

    pub fn search_request(
        &self,
        request: &SearchRequest,
        query_embedding: &[f32],
    ) -> Result<Vec<SearchResult>, SearchError> {
        request.validate()?;

        if request.filters.is_some() {
            return Err(SearchError::Execution {
                message: "filters are unsupported for vector search".into(),
            });
        }
        if request.include_metadata {
            return Err(SearchError::Execution {
                message: "include_metadata is unsupported for vector search".into(),
            });
        }

        self.search(query_embedding, request.top_k)
    }

    fn query_shard(
        &self,
        shard: &ShardManifest,
        version_id: u64,
        query_embedding: &[f32],
        embedding_dim: usize,
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let shard_path =
            resolve_artifact_path(&self.artifact_root, &shard.lance_path, ARTIFACT_KIND)?;
        let shard_size = self.shard_size_bytes(version_id, &shard_path)?;

        let shard_path_for_query = shard_path.clone();
        let shard_path_for_error = shard_path.clone();
        let shard_dir_string = shard_path.to_string_lossy().into_owned();
        let query_embedding = query_embedding.to_vec();

        let results = run_lance_query(async move {
            let conn = lancedb::connect(&shard_dir_string)
                .execute()
                .await
                .map_err(|source| SearchError::Execution {
                    message: format!(
                        "failed to connect local LanceDB artifact at {}: {source}",
                        shard_path_for_error.display()
                    ),
                })?;
            let table = conn
                .open_table(LANCE_TABLE_NAME)
                .execute()
                .await
                .map_err(|source| SearchError::Execution {
                    message: format!(
                        "failed to open local LanceDB documents table at {}: {source}",
                        shard_path_for_error.display()
                    ),
                })?;

            let schema = table
                .schema()
                .await
                .map_err(|source| SearchError::Execution {
                    message: format!(
                        "failed to read local LanceDB schema at {}: {source}",
                        shard_path_for_error.display()
                    ),
                })?;
            let embedding_field =
                schema
                    .field_with_name(EMBEDDING_COLUMN_NAME)
                    .map_err(|source| SearchError::Execution {
                        message: format!(
                            "failed to locate LanceDB embedding column at {}: {source}",
                            shard_path_for_error.display()
                        ),
                    })?;

            match embedding_field.data_type() {
                DataType::FixedSizeList(_, size) if *size as usize == embedding_dim => {}
                DataType::FixedSizeList(_, size) => {
                    return Err(SearchError::Execution {
                        message: format!(
                            "LanceDB embedding dimension {} does not match manifest embedding dimension {} at {}",
                            *size,
                            embedding_dim,
                            shard_path_for_error.display()
                        ),
                    });
                }
                other => {
                    return Err(SearchError::Execution {
                        message: format!(
                            "LanceDB embedding column has unexpected type {other:?} at {}",
                            shard_path_for_error.display()
                        ),
                    });
                }
            }

            let row_count =
                table
                    .count_rows(None)
                    .await
                    .map_err(|source| SearchError::Execution {
                        message: format!(
                            "failed to count rows in local LanceDB documents table at {}: {source}",
                            shard_path_for_error.display()
                        ),
                    })? as usize;

            if row_count == 0 {
                return Ok(Vec::new());
            }

            let batches = table
                .query()
                .nearest_to(query_embedding.as_slice())
                .map_err(|source| SearchError::Execution {
                    message: format!(
                        "failed to build local LanceDB nearest-neighbor query at {}: {source}",
                        shard_path_for_error.display()
                    ),
                })?
                .distance_type(DistanceType::Dot)
                .limit(top_k_for_shard(row_count, top_k))
                .execute()
                .await
                .map_err(|source| SearchError::Execution {
                    message: format!(
                        "failed to execute local LanceDB nearest-neighbor query at {}: {source}",
                        shard_path_for_error.display()
                    ),
                })?
                .try_collect::<Vec<_>>()
                .await
                .map_err(|source| SearchError::Execution {
                    message: format!(
                        "failed to collect local LanceDB query results at {}: {source}",
                        shard_path_for_error.display()
                    ),
                })?;

            decode_lancedb_batches(&batches, &shard_path_for_query)
        })?;

        self.record_shard_access(&shard_path, version_id, shard_size);
        Ok(results)
    }

    fn shard_size_bytes(&self, version_id: u64, shard_path: &Path) -> Result<u64, SearchError> {
        {
            let cache = self
                .cache
                .lock()
                .expect("vector searcher cache lock poisoned");
            if cache.attempted_version == Some(version_id) {
                if let Some(size_bytes) = cache.seen_shards.get(shard_path) {
                    return Ok(*size_bytes);
                }
            }
        }

        inspect_path_tree_within_artifact_root(&self.artifact_root, shard_path)
    }

    fn record_shard_access(&self, shard_path: &Path, version_id: u64, shard_size: u64) {
        let mut cache = self
            .cache
            .lock()
            .expect("vector searcher cache lock poisoned");
        cache.publish_version(version_id);

        match cache.seen_shards.get(shard_path) {
            Some(_) => {
                cache.hit_count += 1;
            }
            None => {
                cache.miss_count += 1;
                cache.bytes_used += shard_size;
                cache
                    .seen_shards
                    .insert(shard_path.to_path_buf(), shard_size);
            }
        }
    }

    fn begin_version_attempt(&self, version_id: u64) {
        let mut cache = self
            .cache
            .lock()
            .expect("vector searcher cache lock poisoned");
        if cache.attempted_version != Some(version_id) {
            cache.reset_for_attempt(version_id);
        }
    }
}

fn top_k_for_shard(row_count: usize, requested_top_k: usize) -> usize {
    row_count.min(requested_top_k)
}

fn run_lance_query<F>(future: F) -> Result<Vec<SearchResult>, SearchError>
where
    F: std::future::Future<Output = Result<Vec<SearchResult>, SearchError>> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|source| SearchError::Execution {
                    message: format!("failed to create tokio runtime for LanceDB query: {source}"),
                })?
                .block_on(future)
        })
        .join()
        .map_err(|panic| SearchError::Execution {
            message: format!("LanceDB query thread panicked: {}", panic_message(panic)),
        })?
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|source| SearchError::Execution {
                message: format!("failed to create tokio runtime for LanceDB query: {source}"),
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

#[cfg(test)]
mod tests {
    use super::top_k_for_shard;

    #[test]
    fn top_k_for_shard_respects_requested_top_k() {
        assert_eq!(top_k_for_shard(200, 3), 3);
        assert_eq!(top_k_for_shard(2, 3), 2);
    }
}
