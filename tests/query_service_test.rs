use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ltsearch::models::{SearchRequest, SearchResponse};
use ltsearch::query_lambda::{QueryLambdaError, QueryRequestHandler};
use ltsearch::query_service::{
    resolve_versioned_handler, resolve_versioned_handler_with_retry, QueryService,
};

#[test]
fn fresh_service_reports_no_cached_version() {
    let service = QueryService::new();
    assert_eq!(service.cached_version(), None);
}

#[test]
fn versioned_cache_reuses_current_version_and_rebootstraps_after_version_change() {
    let state = Mutex::new(None);
    let active_versions = [7_u64, 7, 8];
    let active_version_index = AtomicUsize::new(0);
    let bootstrap_calls = AtomicUsize::new(0);

    let first = resolve_versioned_handler(
        &state,
        || Ok(active_versions[active_version_index.fetch_add(1, Ordering::SeqCst)]),
        |version| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Box::new(move |_request| {
                Ok(SearchResponse {
                    static_chunks: vec![],
                    static_count: 0,
                    dynamic_chunks: vec![],
                    dynamic_count: 0,
                    latency_ms: 1,
                    index_version: version,
                    static_release_id: None,
                })
            }) as QueryRequestHandler))
        },
    )
    .expect("expected first bootstrap to succeed");

    let second = resolve_versioned_handler(
        &state,
        || Ok(active_versions[active_version_index.fetch_add(1, Ordering::SeqCst)]),
        |_version| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            panic!("cache should reuse the existing handler for the same version");
        },
    )
    .expect("expected cached version to be reused");

    let third = resolve_versioned_handler(
        &state,
        || Ok(active_versions[active_version_index.fetch_add(1, Ordering::SeqCst)]),
        |version| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Box::new(move |_request| {
                Ok(SearchResponse {
                    static_chunks: vec![],
                    static_count: 0,
                    dynamic_chunks: vec![],
                    dynamic_count: 0,
                    latency_ms: 1,
                    index_version: version,
                    static_release_id: None,
                })
            }) as QueryRequestHandler))
        },
    )
    .expect("expected version change to trigger a fresh bootstrap");

    assert_eq!(bootstrap_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        first(valid_search_request_for_cache_test())
            .unwrap()
            .index_version,
        7
    );
    assert_eq!(
        second(valid_search_request_for_cache_test())
            .unwrap()
            .index_version,
        7
    );
    assert_eq!(
        third(valid_search_request_for_cache_test())
            .unwrap()
            .index_version,
        8
    );
}

#[test]
fn versioned_cache_retries_once_when_bootstrap_loses_version_race() {
    let state = Mutex::new(None);
    let active_versions = [7_u64, 8, 8];
    let active_version_index = AtomicUsize::new(0);
    let bootstrap_calls = AtomicUsize::new(0);

    let handler = resolve_versioned_handler_with_retry(
        &state,
        || Ok(active_versions[active_version_index.fetch_add(1, Ordering::SeqCst)]),
        |version| {
            let attempt = bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                return Err(QueryLambdaError {
                    error_type: "execution_error".into(),
                    message: format!(
                        "query lambda bootstrap failed: active manifest version changed during bootstrap: expected {version}, got 8"
                    ),
                });
            }

            Ok(Arc::new(Box::new(move |_request| {
                Ok(SearchResponse {
                    static_chunks: vec![],
                    static_count: 0,
                    dynamic_chunks: vec![],
                    dynamic_count: 0,
                    latency_ms: 1,
                    index_version: version,
                    static_release_id: None,
                })
            }) as QueryRequestHandler))
        },
    )
    .expect("expected one retry to succeed after the version race");

    assert_eq!(bootstrap_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        handler(valid_search_request_for_cache_test())
            .unwrap()
            .index_version,
        8
    );
}

fn valid_search_request_for_cache_test() -> SearchRequest {
    SearchRequest {
        query: "rust".into(),
        top_k: 1,
        filters: None,
        include_metadata: false,
        corpus_weights: None,
    }
}
