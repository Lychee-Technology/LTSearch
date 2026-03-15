use std::thread;
use std::time::Instant;

use crate::embedding::EmbeddingGenerator;
use crate::error::SearchError;
use crate::models::{SearchRequest, SearchResponse, SearchResult};
use crate::storage::{ActiveManifest, ManifestStore};

use super::filter::{apply_filters, strip_metadata};
use super::{HybridRanker, KeywordSearcher, VectorSearcher};

const EMBEDDING_GENERATION_MAX_ATTEMPTS: usize = 2;

pub trait KeywordRetriever: Send + Sync {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

pub trait VectorRetriever: Send + Sync {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

#[derive(Debug, Clone)]
pub struct QueryRouter<M, E, K, V> {
    manifest_store: M,
    embedding_generator: E,
    keyword_retriever: K,
    vector_retriever: V,
    ranker: HybridRanker,
}

impl<M, E, K, V> QueryRouter<M, E, K, V>
where
    M: ManifestStore + Send + Sync,
    E: EmbeddingGenerator + Send + Sync,
    K: KeywordRetriever,
    V: VectorRetriever,
{
    pub fn new(
        manifest_store: M,
        embedding_generator: E,
        keyword_retriever: K,
        vector_retriever: V,
    ) -> Self {
        Self {
            manifest_store,
            embedding_generator,
            keyword_retriever,
            vector_retriever,
            ranker: HybridRanker::new(60.0),
        }
    }

    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse, SearchError> {
        request.validate()?;

        let started_at = Instant::now();
        let active_manifest = self
            .manifest_store
            .load_active_manifest()
            .map_err(|source| SearchError::Execution {
                message: source.to_string(),
            })?;
        let index_version = active_manifest.head.version_id;

        let results = match generate_embedding_with_retry(&self.embedding_generator, &request.query)
        {
            Ok(query_embedding) => {
                let (keyword_results, vector_results) = thread::scope(|scope| {
                    let keyword_handle = scope.spawn(|| {
                        self.keyword_retriever.search(
                            &active_manifest,
                            &request.query,
                            request.top_k,
                        )
                    });
                    let vector_handle = scope.spawn(|| {
                        self.vector_retriever.search(
                            &active_manifest,
                            query_embedding.as_slice(),
                            request.top_k,
                        )
                    });

                    let keyword_results =
                        keyword_handle
                            .join()
                            .map_err(|payload| SearchError::Execution {
                                message: panic_payload_message("keyword retrieval", payload),
                            })?;
                    let vector_results =
                        vector_handle
                            .join()
                            .map_err(|payload| SearchError::Execution {
                                message: panic_payload_message("vector retrieval", payload),
                            })?;

                    Ok::<_, SearchError>((keyword_results?, vector_results?))
                })?;
                validate_results(&keyword_results)?;
                validate_results(&vector_results)?;

                self.ranker.fuse(vector_results, keyword_results)
            }
            Err(_) => {
                let keyword_results = self.keyword_retriever.search(
                    &active_manifest,
                    &request.query,
                    request.top_k,
                )?;
                validate_results(&keyword_results)?;
                keyword_results
            }
        };

        let mut results = apply_filters(results, request.filters.as_ref());
        results.truncate(request.top_k);
        if !request.include_metadata {
            results = strip_metadata(results);
        }
        let total_count = results.len();

        let response = SearchResponse {
            results,
            total_count,
            latency_ms: started_at.elapsed().as_millis() as u64,
            index_version,
        };
        response.validate(request.top_k)?;

        Ok(response)
    }
}

fn panic_payload_message(
    context: &str,
    payload: Box<dyn std::any::Any + Send + 'static>,
) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return format!("{context} panicked: {message}");
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return format!("{context} panicked: {message}");
    }

    format!("{context} panicked with non-string payload")
}

fn generate_embedding_with_retry<E>(
    embedding_generator: &E,
    query: &str,
) -> Result<Vec<f32>, crate::embedding::EmbeddingError>
where
    E: EmbeddingGenerator,
{
    let mut last_error = None;

    for _ in 0..EMBEDDING_GENERATION_MAX_ATTEMPTS {
        match embedding_generator.generate(query) {
            Ok(embedding) => return Ok(embedding),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.expect("embedding retry attempts must be positive"))
}

fn validate_results(results: &[SearchResult]) -> Result<(), SearchError> {
    for result in results {
        result.validate()?;
    }

    Ok(())
}

impl<M> KeywordRetriever for KeywordSearcher<M>
where
    M: ManifestStore + Send + Sync,
{
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.search_active_manifest(active_manifest, query, top_k)
    }
}

impl<M> VectorRetriever for VectorSearcher<M>
where
    M: ManifestStore + Send + Sync,
{
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        self.search_active_manifest(active_manifest, query_embedding, top_k)
    }
}
