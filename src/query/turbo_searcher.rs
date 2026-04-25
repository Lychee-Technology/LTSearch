use std::cmp::Ordering;
use std::collections::BinaryHeap;

use rayon::prelude::*;

use crate::error::{SearchError, ValidationError};
use crate::index::{
    encode_vector, score_query_against_record_512, MmapIndex, TurboRecordSlice,
};
use crate::models::{CorpusType, SearchResult, SearchSource};

const TOP_K_MAX: usize = 100;

#[derive(Debug, Clone, Copy)]
pub struct TurboQuantSearcher {
    index: &'static MmapIndex,
}

impl TurboQuantSearcher {
    pub fn new(index: &'static MmapIndex) -> Self {
        Self { index }
    }

    pub fn search(
        &self,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        validate_query_embedding(query_embedding, self.index.dim() as usize)?;
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
            TurboRecordSlice::V1Dim512(records) => records
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
                        text: self.index.text(record_index as u64).to_string(),
                        corpus_type: CorpusType::from_id(meta.corpus_type),
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

        Ok(ranked
            .into_iter()
            .map(|candidate| SearchResult {
                doc_id: candidate.doc_id.to_string(),
                score: candidate.score,
                text: candidate.text,
                metadata: None,
                source: SearchSource::Static,
                corpus_type: Some(candidate.corpus_type),
            })
            .collect())
    }
}

#[derive(Debug, Clone)]
struct RankedResult {
    score: f32,
    doc_id: u64,
    text: String,
    corpus_type: CorpusType,
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

fn validate_query_embedding(
    query_embedding: &[f32],
    expected_dim: usize,
) -> Result<(), SearchError> {
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
    if query_embedding.len() != expected_dim {
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
