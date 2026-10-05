use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use rayon::prelude::*;
use serde_json::Value;

use crate::error::SearchError;
use crate::index::{IndexCodec, MmapIndex, PreparedTurboQuery, TurboRecordSlice};
use crate::models::{ChunkSource, Citation, CorpusType, SearchResult, SearchSource};
use crate::storage::ActiveManifest;

use super::context_builder::corpus_type_label;
use super::retrieval_common::{validate_embedding_dim, validate_query_embedding, validate_top_k};
use super::StaticRetriever;

/// Owns its `MmapIndex` via `Arc`, so the searcher — not a leaked `'static`
/// allocation — is the sole owner of the mmap. When a static-release flip
/// replaces the cached handler and the last in-flight request finishes, the
/// `Arc` count reaches zero and the mmap is released. `Clone` is a cheap
/// refcount bump; the parallel `rayon` scan borrows `&self` and is unaffected.
#[derive(Debug, Clone)]
pub struct TurboQuantSearcher {
    pub index: Arc<MmapIndex>,
}

impl TurboQuantSearcher {
    pub fn new(index: Arc<MmapIndex>) -> Self {
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

        // Dispatch on the record layout once, outside the scan, so each
        // layout's inner loop is monomorphic. Each layout is scored by its
        // own codec's prepared query.
        let heap = match (self.index.records(), self.index.codec()) {
            (
                TurboRecordSlice::V2Dim512(records),
                IndexCodec::Legacy {
                    centroids,
                    projection,
                },
            ) => {
                let prepared_query =
                    PreparedTurboQuery::prepare(query_embedding, centroids, projection).map_err(
                        |source| SearchError::Execution {
                            message: format!("failed to prepare turbo query: {source}"),
                        },
                    )?;
                scan_top_k(records, top_k, |record| {
                    (record.doc_id, prepared_query.score(record))
                })
            }
            (TurboRecordSlice::V4Dim512(records), IndexCodec::Prod(codec)) => {
                let prepared_query = codec.prepare_query(query_embedding).map_err(|source| {
                    SearchError::Execution {
                        message: format!("failed to prepare turbo query: {source}"),
                    }
                })?;
                scan_top_k(records, top_k, |record| {
                    (record.doc_id, prepared_query.score(record.code()))
                })
            }
            // `MmapIndex::load` loads each layout's own codec, so a loaded
            // index never gets here.
            (TurboRecordSlice::V2Dim512(_), IndexCodec::Prod(_))
            | (TurboRecordSlice::V4Dim512(_), IndexCodec::Legacy { .. }) => {
                return Err(SearchError::Execution {
                    message: "static index records and codec are of different versions".into(),
                })
            }
        };

        let mut ranked = heap.into_vec();
        ranked.sort_by(compare_ranked_results);

        // Materialize text/title only for the selected top-K, keeping the
        // parallel scan above zero-copy over the mmap.
        let has_doc_sidecars = self.index.has_doc_sidecars();
        ranked
            .into_iter()
            .map(|candidate| {
                let corpus_type =
                    CorpusType::from_id(self.index.meta(candidate.record_index).corpus_type);
                let text = self.index.text(candidate.record_index).to_string();

                // v3 and v4 images carry a doc_id/metadata sidecar; v2 images
                // do not, so v2 keeps its legacy behavior verbatim (hashed u64
                // doc_id, `metadata: None`, title-only citation).
                let (doc_id, metadata) = if has_doc_sidecars {
                    // Prefer the original string doc_id; fall back to the hashed
                    // u64 only if the sidecar is missing for this record. A
                    // doc_id whose bytes aren't UTF-8 fails the request: the
                    // hash would stand in for an ID no caller knows.
                    let doc_id = self
                        .index
                        .original_doc_id(candidate.record_index as usize)
                        .map_err(|source| SearchError::Execution {
                            message: format!("failed to read static index doc_id: {source}"),
                        })?
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| candidate.doc_id.to_string());
                    // Metadata is optional, so bytes that aren't UTF-8 are
                    // dropped like JSON that doesn't parse.
                    let metadata = match self.index.metadata_json(candidate.record_index as usize) {
                        Ok(json) => json.and_then(|json| parse_metadata(json, &doc_id)),
                        Err(error) => {
                            eprintln!(
                                "warning: turbo static doc {doc_id} has unreadable metadata \
                                 json, falling back to metadata: None ({error})"
                            );
                            None
                        }
                    };
                    (doc_id, metadata)
                } else {
                    (candidate.doc_id.to_string(), None)
                };

                // A metadata-derived citation wins for v3/v4 records; otherwise fall
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

                Ok(SearchResult {
                    doc_id,
                    score: candidate.score,
                    text,
                    metadata,
                    source: SearchSource::Static,
                    chunk_source: ChunkSource::Static,
                    corpus_type: Some(corpus_type),
                    citation,
                })
            })
            .collect()
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
/// One candidate of [`scan_top_k`]. Ordered best first: a higher score is
/// `Less`, and equal scores rank the smaller doc_id first, so
/// `BinaryHeap::into_sorted_vec` returns the ranking.
#[derive(Debug, Clone)]
pub struct RankedResult {
    pub score: f32,
    pub doc_id: u64,
    /// Position of the record in the scanned slice.
    pub record_index: u64,
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

/// Parallel bounded top-K over one record layout. `score_record` returns a
/// record's `(doc_id, score)` and is infallible: everything that can fail was
/// checked when the query was prepared, so the scan carries no per-record
/// `Result`. The doc_id comes from the record itself, which every builder
/// writes with the same hashed id as its meta entry, so the scan never touches
/// the meta mmap.
///
/// Public so the benchmark harness (`examples/turbo_bench`) can time its
/// exact-f32 baseline through the same skeleton as the codecs.
pub fn scan_top_k<R, F>(records: &[R], top_k: usize, score_record: F) -> BinaryHeap<RankedResult>
where
    R: Sync,
    F: Fn(&R) -> (u64, f32) + Sync,
{
    records
        .par_iter()
        .enumerate()
        .fold(
            || BinaryHeap::with_capacity(top_k),
            |mut heap, (record_index, record)| {
                let (doc_id, score) = score_record(record);
                let candidate = RankedResult {
                    score,
                    doc_id,
                    record_index: record_index as u64,
                };
                push_bounded(&mut heap, candidate, top_k);
                heap
            },
        )
        .reduce(BinaryHeap::new, |mut left, right| {
            for candidate in right.into_sorted_vec() {
                push_bounded(&mut left, candidate, top_k);
            }
            left
        })
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
