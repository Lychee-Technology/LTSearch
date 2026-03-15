use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{SearchError, ValidationError};
use crate::models::{CacheStats, SearchRequest, SearchResult, SearchSource, ShardManifest};
use crate::storage::{ActiveManifest, ManifestStore};

// Local-only MVP stand-in for Lance/LanceDB integration.
// Each resolved lance_path directory must contain a rows.json file with an
// array of { doc_id, text, embedding } objects.
const LOCAL_ROWS_JSON_SHIM_FILE_NAME: &str = "rows.json";
const TOP_K_MAX: usize = 100;

#[derive(Debug, Clone)]
pub struct VectorSearcher<M> {
    manifest_store: M,
    artifact_root: PathBuf,
    cache: Arc<Mutex<LocalRowsShimCache>>,
}

impl<M> VectorSearcher<M>
where
    M: ManifestStore,
{
    pub fn new(manifest_store: M, artifact_root: impl AsRef<Path>) -> Self {
        Self {
            manifest_store,
            artifact_root: artifact_root.as_ref().to_path_buf(),
            cache: Arc::new(Mutex::new(LocalRowsShimCache::default())),
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
            for row in self.load_shard_rows(shard, active_manifest.head.version_id)? {
                if row.embedding.len() != active_manifest.manifest.embedding_dim {
                    return Err(SearchError::Execution {
                        message: format!(
                            "row embedding dimension {} does not match manifest embedding dimension {}",
                            row.embedding.len(),
                            active_manifest.manifest.embedding_dim
                        ),
                    });
                }

                validate_local_row(&row)?;
                validate_local_row_embedding(&row.embedding)?;

                let raw_score = dot_product(query_embedding, &row.embedding);
                if !raw_score.is_finite() {
                    return Err(SearchError::Execution {
                        message: format!(
                            "computed non-finite score from local rows.json vector shim for doc_id {}",
                            row.doc_id
                        ),
                    });
                }
                let score = raw_score.clamp(0.0, 1.0);

                let result = SearchResult {
                    doc_id: row.doc_id,
                    score,
                    text: row.text,
                    metadata: row.metadata,
                    source: SearchSource::Vector,
                };

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

    fn load_shard_rows(
        &self,
        shard: &ShardManifest,
        version_id: u64,
    ) -> Result<Vec<VectorRow>, SearchError> {
        let shard_path = resolve_local_rows_json_shim_path(&self.artifact_root, &shard.lance_path)?;
        let rows_path = resolve_local_rows_json_shim_file(&self.artifact_root, &shard_path)?;
        let contents =
            std::fs::read_to_string(&rows_path).map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to read local rows.json vector shim artifact at {}: {source}",
                    rows_path.display()
                ),
            })?;

        let rows = serde_json::from_str(&contents).map_err(|source| SearchError::Execution {
            message: format!(
                "failed to parse local rows.json vector shim artifact at {}: {source}",
                rows_path.display()
            ),
        })?;

        self.record_rows_file_access(rows_path.as_path(), version_id)?;

        Ok(rows)
    }

    fn record_rows_file_access(
        &self,
        rows_path: &Path,
        version_id: u64,
    ) -> Result<(), SearchError> {
        let file_size = std::fs::metadata(rows_path)
            .map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to read local rows.json vector shim metadata at {}: {source}",
                    rows_path.display()
                ),
            })?
            .len();

        let mut cache = self
            .cache
            .lock()
            .expect("vector searcher cache lock poisoned");
        cache.publish_version(version_id);

        match cache.seen_files.get(rows_path) {
            Some(_) => {
                cache.hit_count += 1;
            }
            None => {
                cache.miss_count += 1;
                cache.bytes_used += file_size;
                cache.seen_files.insert(rows_path.to_path_buf(), file_size);
            }
        }

        Ok(())
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

#[derive(Debug, Deserialize)]
struct VectorRow {
    doc_id: String,
    text: String,
    embedding: Vec<f32>,
    metadata: Option<HashMap<String, Value>>,
}

#[derive(Debug, Default)]
struct LocalRowsShimCache {
    seen_files: HashMap<PathBuf, u64>,
    hit_count: u64,
    miss_count: u64,
    attempted_version: Option<u64>,
    current_version: Option<u64>,
    bytes_used: u64,
}

impl LocalRowsShimCache {
    fn reset_for_attempt(&mut self, version_id: u64) {
        self.seen_files.clear();
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

fn validate_local_row(row: &VectorRow) -> Result<(), SearchError> {
    if row.doc_id.is_empty() {
        return Err(SearchError::Validation(ValidationError::Required {
            field: "doc_id",
        }));
    }

    Ok(())
}

fn validate_local_row_embedding(embedding: &[f32]) -> Result<(), SearchError> {
    if embedding.iter().any(|value| !value.is_finite()) {
        return Err(SearchError::Execution {
            message: "local rows.json vector shim row embedding contains non-finite values".into(),
        });
    }

    Ok(())
}

fn resolve_local_rows_json_shim_file(
    artifact_root: &Path,
    shard_path: &Path,
) -> Result<PathBuf, SearchError> {
    let canonical_artifact_root =
        artifact_root
            .canonicalize()
            .map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to canonicalize artifact root {}: {source}",
                    artifact_root.display()
                ),
            })?;
    let rows_path = shard_path.join(LOCAL_ROWS_JSON_SHIM_FILE_NAME);
    let canonical_rows_path =
        rows_path
            .canonicalize()
            .map_err(|source| SearchError::Execution {
                message: format!(
                    "failed to canonicalize local rows.json vector shim path {}: {source}",
                    rows_path.display()
                ),
            })?;

    if !canonical_rows_path.starts_with(canonical_artifact_root) {
        return Err(SearchError::Execution {
            message: format!(
                "resolved local rows.json vector shim path escapes artifact root: {}",
                canonical_rows_path.display()
            ),
        });
    }

    Ok(canonical_rows_path)
}

fn compare_search_results(left: &SearchResult, right: &SearchResult) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.doc_id.cmp(&right.doc_id))
}

fn resolve_local_rows_json_shim_path(
    artifact_root: &Path,
    artifact_path: &str,
) -> Result<PathBuf, SearchError> {
    let (_, key) = artifact_path
        .strip_prefix("s3://")
        .and_then(|value| value.split_once('/'))
        .ok_or_else(|| SearchError::Execution {
            message: format!("invalid local rows.json vector shim path: {artifact_path}"),
        })?;

    let key_path = Path::new(key);
    if key_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(SearchError::Execution {
            message: format!("invalid local rows.json vector shim path: {artifact_path}"),
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
                    "failed to canonicalize local rows.json vector shim path {}: {source}",
                    resolved_path.display()
                ),
            })?;

    if !canonical_resolved_path.starts_with(canonical_artifact_root) {
        return Err(SearchError::Execution {
            message: format!(
                "resolved local rows.json vector shim path escapes artifact root: {}",
                canonical_resolved_path.display()
            ),
        });
    }

    Ok(canonical_resolved_path)
}

fn dot_product(query_embedding: &[f32], candidate_embedding: &[f32]) -> f32 {
    query_embedding
        .iter()
        .zip(candidate_embedding.iter())
        .map(|(left, right)| left * right)
        .sum()
}
