use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{TantivyDocument, Value as _};
use tantivy::{DocAddress, Index, ReloadPolicy};

use crate::error::{SearchError, ValidationError};
use crate::models::{
    ChunkSource, Citation, SearchRequest, SearchResult, SearchSource, ShardManifest,
};
use crate::storage::{ActiveManifest, ManifestStore};

use super::retrieval_common::{dedupe_best_by_doc_id, resolve_artifact_path, validate_top_k};

const DOC_ID_FIELD: &str = "doc_id";
const TEXT_FIELD: &str = "text";
const METADATA_FIELD: &str = "metadata";
const QUERY_MAX_CHARS: usize = 1_000;
const ARTIFACT_KIND: &str = "Tantivy";

#[derive(Debug, Clone)]
pub struct KeywordSearcher<M> {
    manifest_store: M,
    artifact_root: PathBuf,
}

impl<M> KeywordSearcher<M>
where
    M: ManifestStore,
{
    pub fn new(manifest_store: M, artifact_root: impl AsRef<Path>) -> Self {
        Self {
            manifest_store,
            artifact_root: artifact_root.as_ref().to_path_buf(),
        }
    }

    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<SearchResult>, SearchError> {
        validate_query(query)?;
        validate_top_k(top_k)?;

        let active_manifest = self
            .manifest_store
            .load_active_manifest()
            .map_err(|source| SearchError::Execution {
                message: source.to_string(),
            })?;

        self.search_active_manifest(&active_manifest, query, top_k)
    }

    pub fn search_active_manifest(
        &self,
        active_manifest: &ActiveManifest,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        validate_query(query)?;
        validate_top_k(top_k)?;

        if active_manifest.manifest.shards.len() != 1 {
            return Err(SearchError::Execution {
                message: format!(
                    "keyword search currently supports only single-shard manifests, found {} shards",
                    active_manifest.manifest.shards.len()
                ),
            });
        }

        self.search_shard(&active_manifest.manifest.shards[0], query, top_k)
    }

    pub fn search_request(
        &self,
        request: &SearchRequest,
    ) -> Result<Vec<SearchResult>, SearchError> {
        request.validate()?;

        if request.filters.is_some() {
            return Err(SearchError::Execution {
                message: "filters are unsupported for keyword search".into(),
            });
        }
        if request.include_metadata {
            return Err(SearchError::Execution {
                message: "include_metadata is unsupported for keyword search".into(),
            });
        }

        self.search(&request.query, request.top_k)
    }

    fn search_shard(
        &self,
        shard: &ShardManifest,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let index_path =
            resolve_artifact_path(&self.artifact_root, &shard.tantivy_path, ARTIFACT_KIND)?;
        let index = Index::open_in_dir(&index_path).map_err(|source| SearchError::Execution {
            message: format!(
                "failed to open Tantivy index at {}: {source}",
                index_path.display()
            ),
        })?;
        let schema = index.schema();
        let doc_id_field =
            schema
                .get_field(DOC_ID_FIELD)
                .map_err(|source| SearchError::Execution {
                    message: format!("missing {DOC_ID_FIELD} field: {source}"),
                })?;
        let text_field = schema
            .get_field(TEXT_FIELD)
            .map_err(|source| SearchError::Execution {
                message: format!("missing {TEXT_FIELD} field: {source}"),
            })?;
        let metadata_field = schema.get_field(METADATA_FIELD).ok();
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|source| SearchError::Execution {
                message: format!("failed to open index reader: {source}"),
            })?;
        let searcher = reader.searcher();
        let query = QueryParser::for_index(&index, vec![text_field])
            .parse_query(query)
            .map_err(|source| SearchError::Execution {
                message: format!("invalid Tantivy query: {source}"),
            })?;
        let max_docs = searcher.num_docs() as usize;
        let mut limit = top_k.min(max_docs.max(1));

        loop {
            let top_docs = searcher
                .search(&query, &TopDocs::with_limit(limit))
                .map_err(|source| SearchError::Execution {
                    message: format!("failed to execute Tantivy query: {source}"),
                })?;

            let results = dedupe_top_docs(
                &searcher,
                top_docs,
                doc_id_field,
                text_field,
                metadata_field,
                top_k,
            )?;

            if results.len() >= top_k || limit >= max_docs {
                return Ok(results);
            }

            limit = (limit * 2).min(max_docs);
        }
    }
}

fn dedupe_top_docs(
    searcher: &tantivy::Searcher,
    top_docs: Vec<(f32, DocAddress)>,
    doc_id_field: tantivy::schema::Field,
    text_field: tantivy::schema::Field,
    metadata_field: Option<tantivy::schema::Field>,
    top_k: usize,
) -> Result<Vec<SearchResult>, SearchError> {
    let mut results = Vec::with_capacity(top_docs.len());

    for (score, address) in top_docs {
        results.push(build_search_result(
            searcher,
            address,
            doc_id_field,
            text_field,
            metadata_field,
            score,
        )?);
    }

    Ok(dedupe_best_by_doc_id(results, top_k))
}

fn validate_query(query: &str) -> Result<(), SearchError> {
    let query_len = query.chars().count();
    if query_len == 0 {
        return Err(SearchError::Validation(ValidationError::Required {
            field: "query",
        }));
    }
    if query_len > QUERY_MAX_CHARS {
        return Err(SearchError::Validation(ValidationError::LengthOutOfRange {
            field: "query",
            min: 1,
            max: QUERY_MAX_CHARS,
        }));
    }

    Ok(())
}

fn build_search_result(
    searcher: &tantivy::Searcher,
    address: DocAddress,
    doc_id_field: tantivy::schema::Field,
    text_field: tantivy::schema::Field,
    metadata_field: Option<tantivy::schema::Field>,
    score: f32,
) -> Result<SearchResult, SearchError> {
    let document: TantivyDocument =
        searcher
            .doc(address)
            .map_err(|source| SearchError::Execution {
                message: format!("failed to load matched document: {source}"),
            })?;
    let doc_id = document
        .get_first(doc_id_field)
        .and_then(|value| value.as_str())
        .ok_or_else(|| SearchError::Execution {
            message: format!("matched document is missing {DOC_ID_FIELD}"),
        })?;
    let text = document
        .get_first(text_field)
        .and_then(|value| value.as_str())
        .ok_or_else(|| SearchError::Execution {
            message: format!("matched document is missing {TEXT_FIELD}"),
        })?;
    let metadata = load_metadata(&document, metadata_field)?;
    let citation = metadata.as_ref().and_then(Citation::from_metadata);

    Ok(SearchResult {
        doc_id: doc_id.to_string(),
        score,
        text: text.to_string(),
        metadata,
        source: SearchSource::Keyword,
        chunk_source: ChunkSource::Dynamic,
        corpus_type: None,
        citation,
    })
}

fn load_metadata(
    document: &TantivyDocument,
    metadata_field: Option<tantivy::schema::Field>,
) -> Result<Option<HashMap<String, Value>>, SearchError> {
    let Some(metadata_field) = metadata_field else {
        return Ok(None);
    };
    let Some(metadata_json) = document
        .get_first(metadata_field)
        .and_then(|value| value.as_str())
    else {
        return Ok(None);
    };

    serde_json::from_str(metadata_json)
        .map(Some)
        .map_err(|source| SearchError::Execution {
            message: format!("matched document has invalid {METADATA_FIELD}: {source}"),
        })
}
