//! Validation, path-safety, and ranking helpers shared by the three
//! retrievers (vector, keyword, static/TurboQuant).

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::error::{SearchError, ValidationError};
use crate::models::search::TOP_K_MAX;
use crate::models::SearchResult;

pub(crate) fn validate_top_k(top_k: usize) -> Result<(), SearchError> {
    if top_k == 0 || top_k > TOP_K_MAX {
        return Err(SearchError::Validation(ValidationError::RangeOutOfRange {
            field: "top_k",
            min: 1,
            max: TOP_K_MAX as u64,
        }));
    }

    Ok(())
}

pub(crate) fn validate_query_embedding(query_embedding: &[f32]) -> Result<(), SearchError> {
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

pub(crate) fn validate_embedding_dim(
    query_embedding: &[f32],
    expected_dim: usize,
) -> Result<(), SearchError> {
    if query_embedding.len() != expected_dim {
        return Err(SearchError::Validation(ValidationError::InvalidValue {
            field: "query_embedding",
        }));
    }

    Ok(())
}

/// Descending score, then ascending doc_id, so equal-score results have a
/// deterministic order everywhere.
pub(crate) fn compare_search_results(left: &SearchResult, right: &SearchResult) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.doc_id.cmp(&right.doc_id))
}

/// Keeps the best-scoring result per doc_id, sorts, and truncates to `top_k`.
pub(crate) fn dedupe_best_by_doc_id(
    results: impl IntoIterator<Item = SearchResult>,
    top_k: usize,
) -> Vec<SearchResult> {
    let mut deduped: HashMap<String, SearchResult> = HashMap::new();

    for result in results {
        match deduped.get(&result.doc_id) {
            Some(existing) if existing.score >= result.score => {}
            _ => {
                deduped.insert(result.doc_id.clone(), result);
            }
        }
    }

    let mut results: Vec<_> = deduped.into_values().collect();
    results.sort_by(compare_search_results);
    results.truncate(top_k);
    results
}

/// Maps a manifest `s3://bucket/key` artifact path onto the local artifact
/// root, rejecting traversal components and canonicalized escapes (including
/// via symlinks). `kind` names the artifact family in error messages
/// (e.g. "local LanceDB", "Tantivy").
pub(crate) fn resolve_artifact_path(
    artifact_root: &Path,
    artifact_path: &str,
    kind: &str,
) -> Result<PathBuf, SearchError> {
    let (_, key) = artifact_path
        .strip_prefix("s3://")
        .and_then(|value| value.split_once('/'))
        .ok_or_else(|| SearchError::Execution {
            message: format!("invalid {kind} artifact path: {artifact_path}"),
        })?;

    let key_path = Path::new(key);
    if key_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(SearchError::Execution {
            message: format!("invalid {kind} artifact path: {artifact_path}"),
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
                    "failed to canonicalize {kind} artifact path {}: {source}",
                    resolved_path.display()
                ),
            })?;

    if !canonical_resolved_path.starts_with(canonical_artifact_root) {
        return Err(SearchError::Execution {
            message: format!(
                "resolved {kind} artifact path escapes artifact root: {}",
                canonical_resolved_path.display()
            ),
        });
    }

    Ok(canonical_resolved_path)
}
