use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::embedding::{
    fixed_generator_from_env, required_provider_from_env, EmbeddingGenerator, EmbeddingProvider,
};
use crate::error::SearchError;
use crate::models::{SearchRequest, SearchResponse};
use crate::query::{KeywordSearcher, QueryRouter, VectorSearcher};
use crate::storage::{
    ActiveManifest, LocalManifestStore, ManifestHead, ManifestStore, ManifestStoreError,
};

pub type QueryRequestHandler =
    Box<dyn Fn(SearchRequest) -> Result<SearchResponse, SearchError> + Send + Sync + 'static>;
pub type SharedQueryRequestHandler = Arc<QueryRequestHandler>;

const ACTIVE_VERSION_CHANGED_DURING_BOOTSTRAP: &str =
    "active manifest version changed during bootstrap";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryLambdaError {
    pub error_type: String,
    pub message: String,
}

impl From<SearchError> for QueryLambdaError {
    fn from(error: SearchError) -> Self {
        match error {
            SearchError::Validation(source) => Self {
                error_type: "validation_error".into(),
                message: source.to_string(),
            },
            SearchError::Execution { message } => Self {
                error_type: "execution_error".into(),
                message: SearchError::Execution { message }.to_string(),
            },
        }
    }
}

pub fn bootstrap_query_handler_from_env() -> Result<QueryRequestHandler, QueryLambdaError> {
    match required_provider_from_env("LTSEARCH_QUERY_EMBEDDING_PROVIDER") {
        Ok(EmbeddingProvider::Fixed) => bootstrap_fixed_embedding_handler(None),
        Err(error) => Err(bootstrap_error(error.to_string())),
    }
}

pub fn bootstrap_query_handler_for_version_from_env(
    expected_version: u64,
) -> Result<QueryRequestHandler, QueryLambdaError> {
    match required_provider_from_env("LTSEARCH_QUERY_EMBEDDING_PROVIDER") {
        Ok(EmbeddingProvider::Fixed) => bootstrap_fixed_embedding_handler(Some(expected_version)),
        Err(error) => Err(bootstrap_error(error.to_string())),
    }
}

pub fn load_active_query_version_from_env() -> Result<u64, QueryLambdaError> {
    let artifact_root = env::var("LTSEARCH_QUERY_ARTIFACT_ROOT")
        .map(PathBuf::from)
        .map_err(|_| bootstrap_error("missing LTSEARCH_QUERY_ARTIFACT_ROOT"))?;
    let manifest_store = LocalManifestStore::new(&artifact_root);

    manifest_store
        .load_active_version()
        .map_err(|source| bootstrap_error(format!("failed to load active version: {source}")))
}

pub fn is_retriable_bootstrap_version_change(error: &QueryLambdaError) -> bool {
    error.error_type == "execution_error"
        && error
            .message
            .contains(ACTIVE_VERSION_CHANGED_DURING_BOOTSTRAP)
}

fn bootstrap_fixed_embedding_handler(
    expected_version: Option<u64>,
) -> Result<QueryRequestHandler, QueryLambdaError> {
    let artifact_root = env::var("LTSEARCH_QUERY_ARTIFACT_ROOT")
        .map(PathBuf::from)
        .map_err(|_| bootstrap_error("missing LTSEARCH_QUERY_ARTIFACT_ROOT"))?;
    let manifest_store = LocalManifestStore::new(&artifact_root);
    let active_manifest = manifest_store
        .load_active_manifest()
        .map_err(|source| bootstrap_error(format!("failed to load active manifest: {source}")))?;
    if let Some(expected_version) = expected_version {
        if active_manifest.head.version_id != expected_version {
            return Err(bootstrap_error(format!(
                "{ACTIVE_VERSION_CHANGED_DURING_BOOTSTRAP}: expected {expected_version}, got {}",
                active_manifest.head.version_id,
            )));
        }
    }
    let embedding_generator = fixed_generator_from_env("LTSEARCH_QUERY_FIXED_EMBEDDING", None)
        .map_err(|error| bootstrap_error(error.to_string()))?;
    let embedding = embedding_generator
        .generate("ignored")
        .map_err(|error| bootstrap_error(error.to_string()))?;

    if embedding.len() != active_manifest.manifest.embedding_dim {
        return Err(bootstrap_error(format!(
            "LTSEARCH_QUERY_FIXED_EMBEDDING dimension {} does not match manifest embedding_dim {}",
            embedding.len(),
            active_manifest.manifest.embedding_dim,
        )));
    }

    let manifest_store = FixedManifestStore::new(active_manifest.clone());
    let router = QueryRouter::new(
        manifest_store.clone(),
        embedding_generator,
        KeywordSearcher::new(manifest_store.clone(), &artifact_root),
        VectorSearcher::new(manifest_store, &artifact_root),
    );

    Ok(Box::new(move |request| router.search(&request)))
}

pub fn handle_search_request<H>(
    handler: H,
    request: SearchRequest,
) -> Result<SearchResponse, QueryLambdaError>
where
    H: FnOnce(SearchRequest) -> Result<SearchResponse, SearchError>,
{
    handler(request).map_err(QueryLambdaError::from)
}

fn bootstrap_error(message: impl Into<String>) -> QueryLambdaError {
    QueryLambdaError {
        error_type: "execution_error".into(),
        message: format!("query lambda bootstrap failed: {}", message.into()),
    }
}

#[derive(Debug, Clone)]
struct FixedManifestStore {
    active_manifest: ActiveManifest,
}

impl FixedManifestStore {
    fn new(active_manifest: ActiveManifest) -> Self {
        Self { active_manifest }
    }
}

impl ManifestStore for FixedManifestStore {
    fn load_head(&self) -> Result<ManifestHead, ManifestStoreError> {
        Ok(self.active_manifest.head.clone())
    }

    fn load_active_manifest(&self) -> Result<ActiveManifest, ManifestStoreError> {
        Ok(self.active_manifest.clone())
    }
}
