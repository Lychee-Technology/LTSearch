use ltsearch::models::{SearchResult, SearchSource};
use ltsearch::query::HybridRanker;
use serde_json::json;

fn sample_result(doc_id: &str, score: f32, source: SearchSource) -> SearchResult {
    SearchResult {
        doc_id: doc_id.into(),
        score,
        text: format!("text for {doc_id}"),
        metadata: None,
        source,
        corpus_type: None,
    }
}

fn assert_close(left: f32, right: f32) {
    let delta = (left - right).abs();
    assert!(
        delta < 1e-6,
        "expected {left} to equal {right}, delta was {delta}"
    );
}

#[test]
fn compute_rrf_score_uses_one_based_rank() {
    let ranker = HybridRanker::new(60.0);

    assert_close(ranker.compute_rrf_score(1), 1.0 / 61.0);
    assert_close(ranker.compute_rrf_score(3), 1.0 / 63.0);
}

#[test]
#[should_panic(expected = "rank must be at least 1")]
fn compute_rrf_score_rejects_zero_rank() {
    let ranker = HybridRanker::new(60.0);

    let _ = ranker.compute_rrf_score(0);
}

#[test]
fn fuse_deduplicates_overlap_and_marks_results_hybrid() {
    let ranker = HybridRanker::new(60.0);
    let vector_results = vec![
        sample_result("doc-1", 0.91, SearchSource::Vector),
        sample_result("doc-2", 0.80, SearchSource::Vector),
    ];
    let keyword_results = vec![
        sample_result("doc-2", 12.0, SearchSource::Keyword),
        sample_result("doc-3", 10.0, SearchSource::Keyword),
    ];

    let fused = ranker.fuse(vector_results, keyword_results);

    assert_eq!(
        fused.iter().map(|r| r.doc_id.as_str()).collect::<Vec<_>>(),
        vec!["doc-2", "doc-1", "doc-3"]
    );
    assert!(fused
        .iter()
        .all(|result| result.source == SearchSource::Hybrid));
    assert_close(fused[0].score, (1.0 / 62.0) + (1.0 / 61.0));
    assert_close(fused[1].score, 1.0 / 61.0);
    assert_close(fused[2].score, 1.0 / 62.0);
}

#[test]
fn fuse_merges_disjoint_lists_and_sorts_by_descending_rrf_score() {
    let ranker = HybridRanker::new(60.0);
    let vector_results = vec![
        sample_result("doc-a", 0.95, SearchSource::Vector),
        sample_result("doc-b", 0.90, SearchSource::Vector),
    ];
    let keyword_results = vec![sample_result("doc-c", 15.0, SearchSource::Keyword)];

    let fused = ranker.fuse(vector_results, keyword_results);

    assert_eq!(
        fused.iter().map(|r| r.doc_id.as_str()).collect::<Vec<_>>(),
        vec!["doc-a", "doc-c", "doc-b"]
    );
    assert_close(fused[0].score, 1.0 / 61.0);
    assert_close(fused[1].score, 1.0 / 61.0);
    assert_close(fused[2].score, 1.0 / 62.0);
    assert!(fused[0].score >= fused[1].score);
    assert!(fused[1].score >= fused[2].score);
}

#[test]
fn fuse_accumulates_rrf_scores_from_mixed_ranks() {
    let ranker = HybridRanker::new(10.0);
    let vector_results = vec![
        sample_result("doc-x", 0.98, SearchSource::Vector),
        sample_result("doc-y", 0.97, SearchSource::Vector),
        sample_result("doc-z", 0.96, SearchSource::Vector),
    ];
    let keyword_results = vec![
        sample_result("doc-z", 22.0, SearchSource::Keyword),
        sample_result("doc-y", 21.0, SearchSource::Keyword),
    ];

    let fused = ranker.fuse(vector_results, keyword_results);

    assert_eq!(
        fused.iter().map(|r| r.doc_id.as_str()).collect::<Vec<_>>(),
        vec!["doc-z", "doc-y", "doc-x"]
    );
    assert_close(fused[0].score, (1.0 / 13.0) + (1.0 / 11.0));
    assert_close(fused[1].score, (1.0 / 12.0) + (1.0 / 12.0));
    assert_close(fused[2].score, 1.0 / 11.0);
}

#[test]
fn fuse_preserves_metadata_from_duplicate_result_when_available() {
    let ranker = HybridRanker::new(60.0);
    let vector_results = vec![sample_result("doc-1", 0.91, SearchSource::Vector)];
    let keyword_results = vec![SearchResult {
        doc_id: "doc-1".into(),
        score: 12.0,
        text: "text for doc-1".into(),
        metadata: Some(std::collections::HashMap::from([(
            "lang".into(),
            json!("rust"),
        )])),
        source: SearchSource::Keyword,
        corpus_type: None,
    }];

    let fused = ranker.fuse(vector_results, keyword_results);

    assert_eq!(fused.len(), 1);
    assert_eq!(fused[0].doc_id, "doc-1");
    assert_eq!(fused[0].source, SearchSource::Hybrid);
    assert_eq!(fused[0].metadata.as_ref().unwrap()["lang"], json!("rust"));
}

#[test]
fn fuse_merges_metadata_maps_from_duplicate_results() {
    let ranker = HybridRanker::new(60.0);
    let vector_results = vec![SearchResult {
        doc_id: "doc-1".into(),
        score: 0.91,
        text: "text for doc-1".into(),
        metadata: Some(std::collections::HashMap::from([
            ("lang".into(), json!("rust")),
            ("vector_only".into(), json!(true)),
            ("shared".into(), json!("vector")),
        ])),
        source: SearchSource::Vector,
        corpus_type: None,
    }];
    let keyword_results = vec![SearchResult {
        doc_id: "doc-1".into(),
        score: 12.0,
        text: "text for doc-1".into(),
        metadata: Some(std::collections::HashMap::from([
            ("keyword_only".into(), json!(true)),
            ("shared".into(), json!("keyword")),
        ])),
        source: SearchSource::Keyword,
        corpus_type: None,
    }];

    let fused = ranker.fuse(vector_results, keyword_results);

    assert_eq!(fused.len(), 1);
    assert_eq!(fused[0].doc_id, "doc-1");
    assert_eq!(fused[0].source, SearchSource::Hybrid);
    let metadata = fused[0].metadata.as_ref().unwrap();
    assert_eq!(metadata["lang"], json!("rust"));
    assert_eq!(metadata["vector_only"], json!(true));
    assert_eq!(metadata["keyword_only"], json!(true));
    assert_eq!(metadata["shared"], json!("vector"));
}
