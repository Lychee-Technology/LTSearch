use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ValidationError;

const QUERY_MAX_CHARS: usize = 1_000;
pub const TOP_K_MAX: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FilterValue {
    StringEquals(String),
    BoolEquals(bool),
    NumberEquals(f64),
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkSource {
    Static,
    #[default]
    Dynamic,
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
            0 => CorpusType::Legal,
            1 => CorpusType::Contract,
            2 => CorpusType::Rfc,
            other => CorpusType::Other(other),
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
        if !(0.0..=1.0).contains(&self.static_bias) {
            return Err(ValidationError::InvalidValue {
                field: "static_bias",
            });
        }
        if !(0.0..=1.0).contains(&self.dynamic_bias) {
            return Err(ValidationError::InvalidValue {
                field: "dynamic_bias",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub top_k: usize,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub resource_id: String,
    pub source_type: String,
    pub source_ref: String,
    pub title: Option<String>,
    pub url: Option<String>,
}

impl Citation {
    pub fn from_metadata(metadata: &HashMap<String, Value>) -> Option<Self> {
        let resource_id = metadata.get("resource_id")?.as_str()?.to_string();
        let source_type = metadata.get("source_type")?.as_str()?.to_string();
        let source_ref = metadata.get("source_ref")?.as_str()?.to_string();
        let title = metadata
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let url = metadata
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Some(Self {
            resource_id,
            source_type,
            source_ref,
            title,
            url,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub doc_id: String,
    pub score: f32,
    pub text: String,
    pub metadata: Option<HashMap<String, Value>>,
    pub source: SearchSource,
    #[serde(default)]
    pub chunk_source: ChunkSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_type: Option<CorpusType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citation: Option<Citation>,
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
    pub static_chunks: Vec<SearchResult>,
    pub static_count: usize,
    pub dynamic_chunks: Vec<SearchResult>,
    pub dynamic_count: usize,
    pub latency_ms: u64,
    pub index_version: u64,
}

impl SearchResponse {
    pub fn validate(&self, requested_top_k: usize) -> Result<(), ValidationError> {
        if self.static_chunks.len() > self.static_count {
            return Err(ValidationError::Mismatch {
                field: "static_chunks",
                expected: "static_chunks.len() <= static_count",
            });
        }
        if self.dynamic_chunks.len() > self.dynamic_count {
            return Err(ValidationError::Mismatch {
                field: "dynamic_chunks",
                expected: "dynamic_chunks.len() <= dynamic_count",
            });
        }
        if self.static_chunks.len() > requested_top_k {
            return Err(ValidationError::Mismatch {
                field: "static_chunks",
                expected: "static_chunks.len() <= requested_top_k",
            });
        }
        if self.dynamic_chunks.len() > requested_top_k {
            return Err(ValidationError::Mismatch {
                field: "dynamic_chunks",
                expected: "dynamic_chunks.len() <= requested_top_k",
            });
        }
        for result in &self.static_chunks {
            result.validate()?;
        }
        for result in &self.dynamic_chunks {
            result.validate()?;
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

#[cfg(test)]
mod turbo_model_tests {
    use super::*;

    #[test]
    fn chunk_source_serializes() {
        let s = serde_json::to_string(&ChunkSource::Static).unwrap();
        assert_eq!(s, "\"static\"");
        let d = serde_json::to_string(&ChunkSource::Dynamic).unwrap();
        assert_eq!(d, "\"dynamic\"");
    }

    #[test]
    fn corpus_type_roundtrip() {
        let t = CorpusType::Legal;
        let json = serde_json::to_string(&t).unwrap();
        let back: CorpusType = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn search_request_corpus_weights_optional() {
        let req = SearchRequest {
            query: "test".into(),
            top_k: 5,
            filters: None,
            include_metadata: false,
            corpus_weights: None,
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn search_request_rejects_out_of_range_corpus_weights() {
        let req = SearchRequest {
            query: "test".into(),
            top_k: 5,
            filters: None,
            include_metadata: false,
            corpus_weights: Some(CorpusWeights {
                static_bias: 1.5,
                dynamic_bias: 0.5,
            }),
        };
        assert!(req.validate().is_err());
    }

    #[test]
    fn search_result_with_static_source() {
        let r = SearchResult {
            doc_id: "abc".into(),
            score: 0.9,
            text: "hello".into(),
            metadata: None,
            source: SearchSource::Vector,
            chunk_source: ChunkSource::Static,
            corpus_type: Some(CorpusType::Legal),
            citation: None,
        };
        assert!(r.validate().is_ok());
    }
}
