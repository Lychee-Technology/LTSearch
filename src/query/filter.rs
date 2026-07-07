use serde_json::Value;

use crate::models::{FilterValue, SearchResult};

pub fn apply_filters(
    results: Vec<SearchResult>,
    filters: Option<&std::collections::HashMap<String, FilterValue>>,
) -> Vec<SearchResult> {
    let Some(filters) = filters else {
        return results;
    };
    if filters.is_empty() {
        return results;
    }

    results
        .into_iter()
        .filter(|result| matches_filters(result, filters))
        .collect()
}

/// Drops the freeform `metadata` map when a caller requests `include_metadata=false`.
///
/// `citation` is a first-class provenance field (title, source ref, url), not
/// part of the freeform metadata blob, so it is preserved — an upstream caller
/// building the LLM context still needs `citation.title` for source labels even
/// when it does not want the full metadata map.
pub fn strip_metadata(mut results: Vec<SearchResult>) -> Vec<SearchResult> {
    for result in &mut results {
        result.metadata = None;
    }

    results
}

fn matches_filters(
    result: &SearchResult,
    filters: &std::collections::HashMap<String, FilterValue>,
) -> bool {
    let Some(metadata) = &result.metadata else {
        return false;
    };

    filters.iter().all(|(field, expected)| {
        metadata
            .get(field)
            .is_some_and(|actual| matches_filter_value(actual, expected))
    })
}

fn matches_filter_value(actual: &Value, expected: &FilterValue) -> bool {
    match expected {
        FilterValue::StringEquals(value) => actual.as_str() == Some(value.as_str()),
        FilterValue::BoolEquals(value) => actual.as_bool() == Some(*value),
        FilterValue::NumberEquals(value) => actual.as_f64() == Some(*value),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::models::{ChunkSource, Citation, CorpusType, SearchSource};

    fn result_with_metadata_and_citation() -> SearchResult {
        SearchResult {
            doc_id: "doc-1".into(),
            score: 0.9,
            text: "law text".into(),
            metadata: Some(HashMap::from([("lang".into(), json!("zh"))])),
            source: SearchSource::Static,
            chunk_source: ChunkSource::Static,
            corpus_type: Some(CorpusType::Legal),
            citation: Some(Citation {
                resource_id: "res-1".into(),
                source_type: "s3".into(),
                source_ref: "ref-1".into(),
                title: Some("民法典".into()),
                url: None,
            }),
        }
    }

    #[test]
    fn strip_metadata_clears_metadata_but_preserves_citation() {
        let stripped = strip_metadata(vec![result_with_metadata_and_citation()]);

        assert!(stripped[0].metadata.is_none());
        // citation.title must survive so upstream can build LLM source labels
        let citation = stripped[0]
            .citation
            .as_ref()
            .expect("citation must be preserved");
        assert_eq!(citation.title.as_deref(), Some("民法典"));
        // corpus_type is likewise a first-class field, not stripped
        assert_eq!(stripped[0].corpus_type, Some(CorpusType::Legal));
    }
}
