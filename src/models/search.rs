use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ValidationError;

const QUERY_MAX_CHARS: usize = 1_000;
const TOP_K_MAX: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FilterValue {
    StringEquals(String),
    BoolEquals(bool),
    NumberEquals(f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub top_k: usize,
    #[serde(default)]
    pub filters: Option<HashMap<String, FilterValue>>,
    pub include_metadata: bool,
    #[serde(default)]
    pub corpus_weights: Option<CorpusWeights>,
}

impl SearchRequest {
    pub fn validate(&self) -> Result<(), ValidationError> {
        let query_len = self.query.chars().count();
        if query_len == 0 {
            return Err(ValidationError::Required { field: "query" });
        }
        if query_len > QUERY_MAX_CHARS {
            return Err(ValidationError::LengthOutOfRange {
                field: "query",
                min: 1,
                max: QUERY_MAX_CHARS,
            });
        }
        if self.top_k == 0 || self.top_k > TOP_K_MAX {
            return Err(ValidationError::RangeOutOfRange {
                field: "top_k",
                min: 1,
                max: TOP_K_MAX as u64,
            });
        }
        if let Some(filters) = &self.filters {
            for (field, value) in filters {
                validate_filter_field_name(field)?;
                value.validate()?;
            }
        }
        if let Some(weights) = &self.corpus_weights {
            weights.validate()?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSource {
    Vector,
    Keyword,
    Hybrid,
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorpusType {
    Legal,
    Contract,
    Rfc,
    Other(u8),
}

impl CorpusType {
    pub fn from_id(id: u8) -> Self {
        match id {
            0 => Self::Legal,
            1 => Self::Contract,
            2 => Self::Rfc,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorpusWeights {
    pub static_bias: f32,
    pub dynamic_bias: f32,
}

impl CorpusWeights {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.static_bias < 0.0 || self.static_bias > 1.0 {
            return Err(ValidationError::InvalidValue {
                field: "corpus_weights.static_bias",
            });
        }
        if self.dynamic_bias < 0.0 || self.dynamic_bias > 1.0 {
            return Err(ValidationError::InvalidValue {
                field: "corpus_weights.dynamic_bias",
            });
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub doc_id: String,
    pub score: f32,
    pub text: String,
    pub metadata: Option<HashMap<String, Value>>,
    pub source: SearchSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_type: Option<CorpusType>,
}

impl SearchResult {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.doc_id.is_empty() {
            return Err(ValidationError::Required { field: "doc_id" });
        }
        if !self.score.is_finite() || self.score < 0.0 {
            return Err(ValidationError::InvalidValue { field: "score" });
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub total_count: usize,
    pub latency_ms: u64,
    pub index_version: u64,
}

impl SearchResponse {
    pub fn validate(&self, requested_top_k: usize) -> Result<(), ValidationError> {
        if self.results.len() > self.total_count {
            return Err(ValidationError::Mismatch {
                field: "results",
                expected: "results.len() <= total_count",
            });
        }
        if self.results.len() > requested_top_k {
            return Err(ValidationError::Mismatch {
                field: "results",
                expected: "results.len() <= requested_top_k",
            });
        }

        Ok(())
    }
}

impl FilterValue {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::StringEquals(value) if value.is_empty() => Err(ValidationError::Required {
                field: "filters.value",
            }),
            Self::NumberEquals(value) if !value.is_finite() => Err(ValidationError::InvalidValue {
                field: "filters.value",
            }),
            _ => Ok(()),
        }
    }
}

fn validate_filter_field_name(field: &str) -> Result<(), ValidationError> {
    let mut chars = field.chars();
    let Some(first) = chars.next() else {
        return Err(ValidationError::Required {
            field: "filters.field",
        });
    };

    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(ValidationError::InvalidValue {
            field: "filters.field",
        });
    }

    if chars.any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_')) {
        return Err(ValidationError::InvalidValue {
            field: "filters.field",
        });
    }

    Ok(())
}
