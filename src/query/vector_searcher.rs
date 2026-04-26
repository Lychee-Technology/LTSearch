use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use arrow_array::{Array, Float32Array, Float64Array, RecordBatch, StringArray};
use arrow_schema::DataType;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;
use serde_json::Value;

use crate::error::{SearchError, ValidationError};
use crate::models::{
    CacheStats, ChunkSource, SearchRequest, SearchResult, SearchSource, ShardManifest,
};
use crate::storage::{ActiveManifest, ManifestStore};

const LANCE_TABLE_NAME: &str = "documents";
const DISTANCE_COLUMN_NAME: &str = "_distance";
const DOC_ID_COLUMN_NAME: &str = "doc_id";
const TEXT_COLUMN_NAME: &str = "text";
const METADATA_COLUMN_NAME: &str = "metadata";
const EMBEDDING_COLUMN_NAME: &str = "embedding";
const TOP_K_MAX: usize = 100;

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

        let mut deduped: HashMap<String, SearchResult> = HashMap::new();

        for shard in &active_manifest.manifest.shards {
            for result in self.query_shard(
                shard,
                active_manifest.head.version_id,
                query_embedding,
                active_manifest.manifest.embedding_dim,
                top_k,
            )? {
                match deduped.get(&result.doc_id) {
                    Some(existing) if existing.score >= result.score => {}
                    _ => {
                        deduped.insert(result.doc_id.clone(), result);
                    }
                }
            }
        }

        let mut results: Vec<_> = deduped.into_values().collect();
        results.sort_by(compare_search_results);
        results.truncate(top_k);

        Ok(results)
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
        let shard_path = resolve_local_lancedb_path(&self.artifact_root, &shard.lance_path)?;
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

#[derive(Debug, Default)]
struct LocalLanceCache {
    seen_shards: HashMap<PathBuf, u64>,
    hit_count: u64,
    miss_count: u64,
    attempted_version: Option<u64>,
    current_version: Option<u64>,
    bytes_used: u64,
}

impl LocalLanceCache {
    fn reset_for_attempt(&mut self, version_id: u64) {
        self.seen_shards.clear();
        self.hit_count = 0;
        self.miss_count = 0;
        self.bytes_used = 0;
        self.attempted_version = Some(version_id);
        self.current_version = None;
    }

    fn publish_version(&mut self, version_id: u64) {
        self.attempted_version = Some(version_id);
        self.current_version = Some(version_id);
    }
}

fn validate_query_embedding(query_embedding: &[f32]) -> Result<(), SearchError> {
    if query_embedding.is_empty() {
        return Err(SearchError::Validation(ValidationError::Required {
            field: "query_embedding",
        }));
    }
    if query_embedding.iter().any(|value| !value.is_finite()) {
        return Err(SearchError::Validation(ValidationError::InvalidValue {
            field: "query_embedding",
        }));
    }

    Ok(())
}

fn validate_top_k(top_k: usize) -> Result<(), SearchError> {
    if top_k == 0 || top_k > TOP_K_MAX {
        return Err(SearchError::Validation(ValidationError::RangeOutOfRange {
            field: "top_k",
            min: 1,
            max: TOP_K_MAX as u64,
        }));
    }

    Ok(())
}

fn compare_search_results(left: &SearchResult, right: &SearchResult) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.doc_id.cmp(&right.doc_id))
}

fn resolve_local_lancedb_path(
    artifact_root: &Path,
    artifact_path: &str,
) -> Result<PathBuf, SearchError> {
    let (_, key) = artifact_path
        .strip_prefix("s3://")
        .and_then(|value| value.split_once('/'))
        .ok_or_else(|| SearchError::Execution {
            message: format!("invalid local LanceDB artifact path: {artifact_path}"),
        })?;

    let key_path = Path::new(key);
    if key_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(SearchError::Execution {
            message: format!("invalid local LanceDB artifact path: {artifact_path}"),
        });
    }

    let canonical_artifact_root =
        artifact_root
            .canonicalize()
            .map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to canonicalize artifact root {}: {source}",
                    artifact_root.display()
                ),
            })?;
    let resolved_path = artifact_root.join(key);
    let canonical_resolved_path =
        resolved_path
            .canonicalize()
            .map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to canonicalize local LanceDB artifact path {}: {source}",
                    resolved_path.display()
                ),
            })?;

    if !canonical_resolved_path.starts_with(canonical_artifact_root) {
        return Err(SearchError::Execution {
            message: format!(
                "resolved local LanceDB artifact path escapes artifact root: {}",
                canonical_resolved_path.display()
            ),
        });
    }

    Ok(canonical_resolved_path)
}

fn inspect_path_tree_within_artifact_root(
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

fn decode_lancedb_batches(
    batches: &[RecordBatch],
    shard_path: &Path,
) -> Result<Vec<SearchResult>, SearchError> {
    let mut results = Vec::new();

    for batch in batches {
        let doc_ids = downcast_string_column(batch, DOC_ID_COLUMN_NAME, shard_path)?;
        let texts = downcast_string_column(batch, TEXT_COLUMN_NAME, shard_path)?;
        let metadata = downcast_string_column(batch, METADATA_COLUMN_NAME, shard_path)?;
        let distances =
            batch
                .column_by_name(DISTANCE_COLUMN_NAME)
                .ok_or_else(|| SearchError::Execution {
                    message: format!(
                        "local LanceDB query did not return {} column at {}",
                        DISTANCE_COLUMN_NAME,
                        shard_path.display()
                    ),
                })?;

        for index in 0..batch.num_rows() {
            let score =
                lancedb_distance_to_score(distance_value(distances.as_ref(), index, shard_path)?)?;
            let metadata = parse_metadata_json(metadata.value(index), shard_path)?;
            let result = SearchResult {
                doc_id: doc_ids.value(index).to_string(),
                score,
                text: texts.value(index).to_string(),
                metadata,
                source: SearchSource::Vector,
                chunk_source: ChunkSource::Dynamic,
                corpus_type: None,
            };

            result.validate()?;
            results.push(result);
        }
    }

    Ok(results)
}

fn downcast_string_column<'a>(
    batch: &'a RecordBatch,
    column_name: &str,
    shard_path: &Path,
) -> Result<&'a StringArray, SearchError> {
    batch
        .column_by_name(column_name)
        .ok_or_else(|| SearchError::Execution {
            message: format!(
                "local LanceDB query did not return {} column at {}",
                column_name,
                shard_path.display()
            ),
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| SearchError::Execution {
            message: format!(
                "local LanceDB column {} had unexpected type at {}",
                column_name,
                shard_path.display()
            ),
        })
}

fn distance_value(
    distance_column: &dyn Array,
    index: usize,
    shard_path: &Path,
) -> Result<f32, SearchError> {
    if let Some(values) = distance_column.as_any().downcast_ref::<Float32Array>() {
        return Ok(values.value(index));
    }
    if let Some(values) = distance_column.as_any().downcast_ref::<Float64Array>() {
        return Ok(values.value(index) as f32);
    }

    Err(SearchError::Execution {
        message: format!(
            "local LanceDB distance column had unexpected type at {}",
            shard_path.display()
        ),
    })
}

fn lancedb_distance_to_score(distance: f32) -> Result<f32, SearchError> {
    if !distance.is_finite() {
        return Err(SearchError::Execution {
            message: "local LanceDB query returned non-finite distance".into(),
        });
    }

    Ok((1.0 - distance).clamp(0.0, 1.0))
}

fn top_k_for_shard(row_count: usize, requested_top_k: usize) -> usize {
    row_count.min(requested_top_k)
}

fn parse_metadata_json(
    metadata_json: &str,
    shard_path: &Path,
) -> Result<Option<HashMap<String, Value>>, SearchError> {
    let metadata =
        serde_json::from_str::<HashMap<String, Value>>(metadata_json).map_err(|source| {
            SearchError::Execution {
                message: format!(
                    "failed to parse metadata from local LanceDB documents table at {}: {source}",
                    shard_path.display()
                ),
            }
        })?;

    if metadata.is_empty() {
        Ok(None)
    } else {
        Ok(Some(metadata))
    }
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
