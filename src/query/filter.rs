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

pub fn strip_metadata(mut results: Vec<SearchResult>) -> Vec<SearchResult> {
    for result in &mut results {
        result.metadata = None;
        result.citation = None;
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
