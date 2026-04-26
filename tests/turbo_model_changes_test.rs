use ltsearch::models::{CorpusType, CorpusWeights, SearchRequest, SearchResult, SearchSource};
use serde_json::json;

#[test]
fn search_source_has_static_variant() {
    let source = SearchSource::Static;
    let json = serde_json::to_value(&source).unwrap();
    assert_eq!(json, json!("static"));

    let decoded: SearchSource = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, SearchSource::Static);
}

#[test]
fn search_result_with_corpus_type_serializes() {
    let result = SearchResult {
        doc_id: "doc-1".into(),
        score: 0.9,
        text: "legal text".into(),
        metadata: None,
        source: SearchSource::Static,
        chunk_source: ltsearch::models::ChunkSource::Static,
        corpus_type: Some(CorpusType::Legal),
    };

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["source"], json!("static"));
    assert_eq!(json["corpus_type"], json!("legal"));

    let decoded: SearchResult = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.source, SearchSource::Static);
    assert_eq!(decoded.corpus_type, Some(CorpusType::Legal));
}

#[test]
fn search_result_without_corpus_type_is_backward_compatible() {
    let json = json!({
        "doc_id": "doc-1",
        "score": 0.8,
        "text": "hello",
        "metadata": null,
        "source": "hybrid"
    });

    let result: SearchResult = serde_json::from_value(json).unwrap();
    assert_eq!(result.source, SearchSource::Hybrid);
    assert_eq!(result.corpus_type, None);
}

#[test]
fn search_request_with_corpus_weights_serializes() {
    let request = SearchRequest {
        query: "test".into(),
        top_k: 5,
        filters: None,
        include_metadata: false,
        corpus_weights: Some(CorpusWeights {
            static_bias: 0.8,
            dynamic_bias: 0.2,
        }),
    };

    assert!(request.validate().is_ok());
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(
        json["corpus_weights"]["static_bias"].as_f64(),
        Some(0.8_f32 as f64)
    );
}

#[test]
fn search_request_without_corpus_weights_is_backward_compatible() {
    let json = json!({
        "query": "test",
        "top_k": 5,
        "include_metadata": false
    });

    let request: SearchRequest = serde_json::from_value(json).unwrap();
    assert!(request.corpus_weights.is_none());
    assert!(request.validate().is_ok());
}

#[test]
fn corpus_type_all_variants_serialize() {
    let variants = vec![
        (CorpusType::Legal, "legal"),
        (CorpusType::Contract, "contract"),
        (CorpusType::Rfc, "rfc"),
    ];

    for (variant, expected_str) in &variants {
        let json = serde_json::to_value(variant).unwrap();
        assert_eq!(json, json!(expected_str));
    }

    let other = CorpusType::Other(42);
    let json = serde_json::to_value(&other).unwrap();
    assert!(json.is_object() || json.is_string());
}

#[test]
fn corpus_type_maps_from_u8_id() {
    assert_eq!(CorpusType::from_id(0), CorpusType::Legal);
    assert_eq!(CorpusType::from_id(1), CorpusType::Contract);
    assert_eq!(CorpusType::from_id(2), CorpusType::Rfc);
    assert_eq!(CorpusType::from_id(42), CorpusType::Other(42));
}

#[test]
fn corpus_weights_validates_bias_range() {
    let valid = CorpusWeights {
        static_bias: 0.5,
        dynamic_bias: 0.5,
    };
    assert!(valid.validate().is_ok());

    let negative = CorpusWeights {
        static_bias: -0.1,
        dynamic_bias: 0.5,
    };
    assert!(negative.validate().is_err());

    let over_one = CorpusWeights {
        static_bias: 0.5,
        dynamic_bias: 1.1,
    };
    assert!(over_one.validate().is_err());
}

#[test]
fn static_search_result_validates_like_any_other() {
    let result = SearchResult {
        doc_id: "doc-1".into(),
        score: 0.5,
        text: "static text".into(),
        metadata: None,
        source: SearchSource::Static,
        chunk_source: ltsearch::models::ChunkSource::Static,
        corpus_type: Some(CorpusType::Legal),
    };
    assert!(result.validate().is_ok());

    let invalid = SearchResult {
        doc_id: "".into(),
        ..result
    };
    assert!(invalid.validate().is_err());
}
