use std::thread;
use std::time::Instant;

use crate::embedding::EmbeddingGenerator;
use crate::error::SearchError;
use crate::models::{SearchRequest, SearchResponse, SearchResult};
use crate::storage::{ActiveManifest, ManifestStore};

use super::filter::{apply_filters, strip_metadata};
use super::{HybridRanker, KeywordSearcher, VectorSearcher};

const EMBEDDING_GENERATION_MAX_ATTEMPTS: usize = 2;
const SEARCH_WINDOW_MAX: usize = 100;

pub trait WarningSink: Send + Sync {
    fn warn(&self, message: String);
}

#[derive(Debug, Clone, Default)]
pub struct NoopWarningSink;

impl WarningSink for NoopWarningSink {
    fn warn(&self, _message: String) {}
}

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

pub trait StaticRetriever: Send + Sync {
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

impl<T> StaticRetriever for Box<T>
where
    T: StaticRetriever + ?Sized,
{
    fn search(
        &self,
        active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        (**self).search(active_manifest, query_embedding, top_k)
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoopStaticRetriever;

impl StaticRetriever for NoopStaticRetriever {
    fn search(
        &self,
        _active_manifest: &ActiveManifest,
        _query_embedding: &[f32],
        _top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        Ok(vec![])
    }
}

#[derive(Debug, Clone)]
struct GroupedSearchResults {
    static_chunks: Vec<SearchResult>,
    dynamic_chunks: Vec<SearchResult>,
}

#[derive(Debug, Clone)]
pub struct QueryRouter<M, E, K, V, S = NoopStaticRetriever, W = NoopWarningSink> {
    manifest_store: M,
    embedding_generator: E,
    keyword_retriever: K,
    vector_retriever: V,
    static_retriever: S,
    warning_sink: W,
    ranker: HybridRanker,
}

impl<M, E, K, V> QueryRouter<M, E, K, V, NoopStaticRetriever, NoopWarningSink>
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
            static_retriever: NoopStaticRetriever,
            warning_sink: NoopWarningSink,
            ranker: HybridRanker::new(60.0),
        }
    }
}

impl<M, E, K, V, S, W> QueryRouter<M, E, K, V, S, W>
where
    M: ManifestStore + Send + Sync,
    E: EmbeddingGenerator + Send + Sync,
    K: KeywordRetriever,
    V: VectorRetriever,
    S: StaticRetriever,
    W: WarningSink,
{
    pub fn with_static_retriever<S2>(self, static_retriever: S2) -> QueryRouter<M, E, K, V, S2, W>
    where
        S2: StaticRetriever,
    {
        QueryRouter {
            manifest_store: self.manifest_store,
            embedding_generator: self.embedding_generator,
            keyword_retriever: self.keyword_retriever,
            vector_retriever: self.vector_retriever,
            static_retriever,
            warning_sink: self.warning_sink,
            ranker: self.ranker,
        }
    }

    pub fn with_warning_sink<W2>(self, warning_sink: W2) -> QueryRouter<M, E, K, V, S, W2>
    where
        W2: WarningSink,
    {
        QueryRouter {
            manifest_store: self.manifest_store,
            embedding_generator: self.embedding_generator,
            keyword_retriever: self.keyword_retriever,
            vector_retriever: self.vector_retriever,
            static_retriever: self.static_retriever,
            warning_sink,
            ranker: self.ranker,
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
        let query_embedding =
            generate_embedding_with_retry(&self.embedding_generator, &request.query);

        let results = if query_requires_iterative_filtering(request) {
            self.search_with_iterative_filtering(request, &active_manifest, &query_embedding)?
        } else {
            self.search_single_pass(request, &active_manifest, &query_embedding)?
        };

        let mut static_chunks = apply_filters(results.static_chunks, request.filters.as_ref());
        let mut dynamic_chunks = apply_filters(results.dynamic_chunks, request.filters.as_ref());
        static_chunks.truncate(request.top_k);
        dynamic_chunks.truncate(request.top_k);
        if !request.include_metadata {
            static_chunks = strip_metadata(static_chunks);
            dynamic_chunks = strip_metadata(dynamic_chunks);
        }
        let static_count = static_chunks.len();
        let dynamic_count = dynamic_chunks.len();

        let response = SearchResponse {
            static_chunks,
            static_count,
            dynamic_chunks,
            dynamic_count,
            latency_ms: started_at.elapsed().as_millis() as u64,
            index_version,
        };
        response.validate(request.top_k)?;

        Ok(response)
    }

    fn search_single_pass(
        &self,
        request: &SearchRequest,
        active_manifest: &ActiveManifest,
        query_embedding: &Result<Vec<f32>, crate::embedding::EmbeddingError>,
    ) -> Result<GroupedSearchResults, SearchError> {
        match query_embedding {
            Ok(query_embedding) => self.search_grouped(
                active_manifest,
                &request.query,
                query_embedding.as_slice(),
                retrieval_window(request.top_k),
            ),
            Err(error) => {
                self.warning_sink.warn(format!(
                    "embedding generation failed after {EMBEDDING_GENERATION_MAX_ATTEMPTS} attempts; falling back to keyword-only retrieval: query={}, top_k={}, error={}",
                    request.query, request.top_k, error
                ));
                Ok(GroupedSearchResults {
                    static_chunks: vec![],
                    dynamic_chunks: self.search_keyword_only(
                        active_manifest,
                        &request.query,
                        retrieval_window(request.top_k),
                    )?,
                })
            }
        }
    }

    fn search_with_iterative_filtering(
        &self,
        request: &SearchRequest,
        active_manifest: &ActiveManifest,
        query_embedding: &Result<Vec<f32>, crate::embedding::EmbeddingError>,
    ) -> Result<GroupedSearchResults, SearchError> {
        let mut retrieval_top_k = request.top_k.max(1);
        let mut warned_on_fallback = false;

        loop {
            let results = match query_embedding {
                Ok(query_embedding) => self.search_grouped(
                    active_manifest,
                    &request.query,
                    query_embedding.as_slice(),
                    retrieval_window(retrieval_top_k),
                )?,
                Err(error) => {
                    if !warned_on_fallback {
                        self.warning_sink.warn(format!(
                            "embedding generation failed after {EMBEDDING_GENERATION_MAX_ATTEMPTS} attempts; falling back to keyword-only retrieval: query={}, top_k={}, error={}",
                            request.query, request.top_k, error
                        ));
                        warned_on_fallback = true;
                    }
                    GroupedSearchResults {
                        static_chunks: vec![],
                        dynamic_chunks: self.search_keyword_only(
                            active_manifest,
                            &request.query,
                            retrieval_window(retrieval_top_k),
                        )?,
                    }
                }
            };

            let filtered_dynamic =
                apply_filters(results.dynamic_chunks.clone(), request.filters.as_ref()).len();
            if filtered_dynamic >= request.top_k || retrieval_top_k >= SEARCH_WINDOW_MAX {
                return Ok(results);
            }

            let next_top_k = (retrieval_top_k.saturating_mul(2)).min(SEARCH_WINDOW_MAX);
            if next_top_k == retrieval_top_k {
                return Ok(results);
            }
            retrieval_top_k = next_top_k;
        }
    }

    fn search_grouped(
        &self,
        active_manifest: &ActiveManifest,
        query: &str,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<GroupedSearchResults, SearchError> {
        let (static_results, keyword_results, vector_results) = thread::scope(|scope| {
            let static_handle = scope.spawn(|| {
                self.static_retriever
                    .search(active_manifest, query_embedding, top_k)
            });
            let keyword_handle =
                scope.spawn(|| self.keyword_retriever.search(active_manifest, query, top_k));
            let vector_handle = scope.spawn(|| {
                self.vector_retriever
                    .search(active_manifest, query_embedding, top_k)
            });

            let static_results =
                static_handle
                    .join()
                    .map_err(|payload| SearchError::Execution {
                        message: panic_payload_message("static retrieval", payload),
                    })?;
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

            Ok::<_, SearchError>((static_results?, keyword_results?, vector_results?))
        })?;
        validate_results(&static_results)?;
        validate_results(&keyword_results)?;
        validate_results(&vector_results)?;

        Ok(GroupedSearchResults {
            static_chunks: static_results,
            dynamic_chunks: self.ranker.fuse(vector_results, keyword_results),
        })
    }

    fn search_keyword_only(
        &self,
        active_manifest: &ActiveManifest,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let keyword_results = self
            .keyword_retriever
            .search(active_manifest, query, top_k)?;
        validate_results(&keyword_results)?;
        Ok(keyword_results)
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

fn query_requires_iterative_filtering(request: &SearchRequest) -> bool {
    request
        .filters
        .as_ref()
        .is_some_and(|filters| !filters.is_empty())
}

fn validate_results(results: &[SearchResult]) -> Result<(), SearchError> {
    for result in results {
        result.validate()?;
    }

    Ok(())
}

fn retrieval_window(top_k: usize) -> usize {
    top_k.max(1).saturating_mul(3).min(SEARCH_WINDOW_MAX)
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

#[cfg(test)]
mod turbo_router_tests {
    use super::*;

    // This test just verifies the type system accepts a QueryRouter with all type params.
    #[test]
    fn router_accepts_static_retriever_type_param() {
        fn _accept<M, E, K, V, S, W>(_: &QueryRouter<M, E, K, V, S, W>)
        where
            M: ManifestStore + Send + Sync,
            E: crate::embedding::EmbeddingGenerator + Send + Sync,
            K: KeywordRetriever,
            V: VectorRetriever,
            S: StaticRetriever,
            W: WarningSink,
        {
        }
        // Compiles = pass.
    }
}
