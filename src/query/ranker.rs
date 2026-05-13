use std::collections::hash_map::Entry;
use std::collections::HashMap;

use crate::models::{SearchResult, SearchSource};

#[derive(Debug, Clone, PartialEq)]
pub struct HybridRanker {
    rrf_k: f32,
}

impl HybridRanker {
    pub fn new(rrf_k: f32) -> Self {
        assert!(rrf_k > 0.0, "rrf_k must be positive");
        Self { rrf_k }
    }

    pub fn compute_rrf_score(&self, rank: usize) -> f32 {
        assert!(rank >= 1, "rank must be at least 1");
        1.0 / (self.rrf_k + rank as f32)
    }

    pub fn fuse(
        &self,
        vector_results: Vec<SearchResult>,
        keyword_results: Vec<SearchResult>,
    ) -> Vec<SearchResult> {
        let mut rrf_scores: HashMap<String, f32> = HashMap::new();
        let mut doc_map: HashMap<String, SearchResult> = HashMap::new();

        for (index, result) in vector_results.into_iter().enumerate() {
            let rank = index + 1;
            *rrf_scores.entry(result.doc_id.clone()).or_insert(0.0) += self.compute_rrf_score(rank);
            doc_map.entry(result.doc_id.clone()).or_insert(result);
        }

        for (index, result) in keyword_results.into_iter().enumerate() {
            let rank = index + 1;
            *rrf_scores.entry(result.doc_id.clone()).or_insert(0.0) += self.compute_rrf_score(rank);
            match doc_map.entry(result.doc_id.clone()) {
                Entry::Occupied(mut entry) => {
                    merge_result_payload(entry.get_mut(), result);
                }
                Entry::Vacant(entry) => {
                    entry.insert(result);
                }
            }
        }

        let mut fused: Vec<SearchResult> = rrf_scores
            .into_iter()
            .map(|(doc_id, score)| {
                let mut result = doc_map.remove(&doc_id).expect("missing search result");
                result.score = score;
                result.source = SearchSource::Hybrid;
                result
            })
            .collect();

        fused.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap()
                .then_with(|| left.doc_id.cmp(&right.doc_id))
        });

        fused
    }
}

fn merge_result_payload(existing: &mut SearchResult, incoming: SearchResult) {
    merge_metadata(&mut existing.metadata, incoming.metadata);
    if existing.citation.is_none() {
        existing.citation = incoming.citation;
    }
}

fn merge_metadata(
    existing: &mut Option<HashMap<String, serde_json::Value>>,
    incoming: Option<HashMap<String, serde_json::Value>>,
) {
    match (existing, incoming) {
        (Some(existing_map), Some(incoming_map)) => {
            for (key, value) in incoming_map {
                existing_map.entry(key).or_insert(value);
            }
        }
        (slot @ None, Some(incoming_map)) => {
            *slot = Some(incoming_map);
        }
        _ => {}
    }
}
