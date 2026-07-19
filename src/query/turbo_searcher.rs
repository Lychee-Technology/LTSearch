use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use rayon::prelude::*;
use serde_json::Value;

use crate::error::SearchError;
use crate::index::{encode_vector, score_query_against_record_512, MmapIndex, TurboRecordSlice};
use crate::models::{ChunkSource, Citation, CorpusType, SearchResult, SearchSource};
use crate::storage::ActiveManifest;

use super::context_builder::corpus_type_label;
use super::retrieval_common::{validate_embedding_dim, validate_query_embedding, validate_top_k};
use super::StaticRetriever;

#[derive(Debug, Clone, Copy)]
pub struct TurboQuantSearcher {
    pub index: &'static MmapIndex,
}

impl TurboQuantSearcher {
    pub fn new(index: &'static MmapIndex) -> Self {
        Self { index }
    }
}

impl StaticRetriever for TurboQuantSearcher {
    fn search(
        &self,
        _active_manifest: &ActiveManifest,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        validate_query_embedding(query_embedding)?;
        validate_embedding_dim(query_embedding, self.index.dim() as usize)?;
        validate_top_k(top_k)?;

        let encoded_query = encode_vector(
            query_embedding,
            self.index.centroids(),
            self.index.projection(),
        )
        .map_err(|source| SearchError::Execution {
            message: format!("failed to encode turbo query embedding: {source}"),
        })?;

        let heap = match self.index.records() {
            TurboRecordSlice::V2Dim512(records) => records
                .par_iter()
                .enumerate()
                .try_fold(BinaryHeap::new, |mut heap, (record_index, record)| {
                    let score = score_query_against_record_512(
                        query_embedding,
                        &encoded_query,
                        record,
                        self.index.centroids(),
                        self.index.projection(),
                    )
                    .map_err(|source| SearchError::Execution {
                        message: format!("failed to score turbo record {record_index}: {source}"),
                    })?;

                    let meta = self.index.meta(record_index as u64);
                    let candidate = RankedResult {
                        score,
                        doc_id: meta.doc_id,
                        record_index: record_index as u64,
                    };

                    push_bounded(&mut heap, candidate, top_k);
                    Ok::<_, SearchError>(heap)
                })
                .try_reduce(BinaryHeap::new, |mut left, right| {
                    for candidate in right.into_sorted_vec() {
                        push_bounded(&mut left, candidate, top_k);
                    }
                    Ok::<_, SearchError>(left)
                })?,
        };

        let mut ranked = heap.into_vec();
        ranked.sort_by(compare_ranked_results);

        // Materialize text/title only for the selected top-K, keeping the
        // parallel scan above zero-copy over the mmap.
        let is_v3 = self.index.version() == 3;
        Ok(ranked
            .into_iter()
            .map(|candidate| {
                let corpus_type =
                    CorpusType::from_id(self.index.meta(candidate.record_index).corpus_type);
                let text = self.index.text(candidate.record_index).to_string();

                // v3 images carry a doc_id/metadata sidecar; v2 images do not,
                // so v2 keeps its legacy behavior verbatim (hashed u64 doc_id,
                // `metadata: None`, title-only citation). Only the top-K
                // materialization differs — the scan/scoring path is shared.
                let (doc_id, metadata) = if is_v3 {
                    // Prefer the original string doc_id; fall back to the hashed
                    // u64 only if the sidecar is missing for this record.
                    let doc_id = self
                        .index
                        .original_doc_id(candidate.record_index as usize)
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| candidate.doc_id.to_string());
                    let metadata = self
                        .index
                        .metadata_json(candidate.record_index as usize)
                        .and_then(|json| parse_metadata(json, &doc_id));
                    (doc_id, metadata)
                } else {
                    (candidate.doc_id.to_string(), None)
                };

                // A metadata-derived citation wins for v3 records; otherwise fall
                // back to the title-only citation shared with v2. A title makes
                // the chunk citable: ContextBuilder reads `citation.title` to
                // render `[法规 #1] <title>`. Without either, `citation` stays
                // None and the bare label is rendered.
                let citation = metadata
                    .as_ref()
                    .and_then(Citation::from_metadata)
                    .or_else(|| {
                        self.index
                            .title(candidate.record_index)
                            .map(|title| Citation {
                                resource_id: doc_id.clone(),
                                source_type: corpus_type_label(Some(&corpus_type)).to_string(),
                                source_ref: doc_id.clone(),
                                title: Some(title.to_string()),
                                url: None,
                            })
                    });

                SearchResult {
                    doc_id,
                    score: candidate.score,
                    text,
                    metadata,
                    source: SearchSource::Static,
                    chunk_source: ChunkSource::Static,
                    corpus_type: Some(corpus_type),
                    citation,
                }
            })
            .collect())
    }
}

/// Parses a v3 metadata JSON blob into a map. On failure (malformed JSON or a
/// non-object top level) the record's metadata is conservatively dropped to
/// `None` with a warning — a single bad record must not fail the whole request.
fn parse_metadata(json: &str, doc_id: &str) -> Option<HashMap<String, Value>> {
    match serde_json::from_str::<HashMap<String, Value>>(json) {
        Ok(metadata) => Some(metadata),
        Err(error) => {
            eprintln!(
                "warning: turbo static doc {doc_id} has unparseable metadata json, \
                 falling back to metadata: None ({error})"
            );
            None
        }
    }
}

// Only score + doc_id drive ranking/tie-breaks, so the parallel scan keeps
// candidates cheap (no per-record String allocation); the winning records'
// text/title are read from the mmap after top-K selection via `record_index`.
#[derive(Debug, Clone)]
struct RankedResult {
    score: f32,
    doc_id: u64,
    record_index: u64,
}

impl PartialEq for RankedResult {
    fn eq(&self, other: &Self) -> bool {
        self.score.to_bits() == other.score.to_bits() && self.doc_id == other.doc_id
    }
}

impl Eq for RankedResult {}

impl PartialOrd for RankedResult {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedResult {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_ranked_results(self, other)
    }
}

fn push_bounded(heap: &mut BinaryHeap<RankedResult>, candidate: RankedResult, top_k: usize) {
    if heap.len() < top_k {
        heap.push(candidate);
        return;
    }

    let should_replace = heap
        .peek()
        .map(|worst| compare_ranked_results(&candidate, worst) == Ordering::Less)
        .unwrap_or(true);

    if should_replace {
        heap.pop();
        heap.push(candidate);
    }
}

fn compare_ranked_results(left: &RankedResult, right: &RankedResult) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.doc_id.cmp(&right.doc_id))
}
