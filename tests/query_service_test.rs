mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ltsearch::models::{SearchRequest, SearchResponse};
use ltsearch::query_lambda::{QueryLambdaError, QueryRequestHandler};
use ltsearch::query_service::{
    resolve_versioned_handler, resolve_versioned_handler_with_retry, QueryService,
};
use ltsearch::storage::{version_manifest_key, StaticReleaseHead, INDEX_HEAD_KEY, STATIC_HEAD_KEY};

#[test]
fn fresh_service_reports_no_cached_version() {
    let service = QueryService::new();
    assert_eq!(service.cached_version(), None);
}

// `cached_pair` reads both halves of the cache key under a *single* mutex lock,
// so `/health` can report `(index_version, static_release_id)` from one atomic
// read of the same cache generation — never a cross-generation mix (issue #112
// AC4). A fresh service holds no pair; after a successful resolve the pair
// equals the `(version, release_id)` the handler was bootstrapped for.
#[allow(clippy::await_holding_lock)]
#[test]
fn cached_pair_returns_both_components_from_single_key() {
    let _guard = common::ENV_LOCK.lock().unwrap();

    let service = QueryService::new();
    assert_eq!(service.cached_pair(), None, "fresh service holds no pair");

    let root = common::temp_fixture_dir("query-service-cached-pair");
    common::write_fixture(&root, INDEX_HEAD_KEY, &common::sample_head_json(7));
    common::write_fixture(
        &root,
        &version_manifest_key(7),
        &common::sample_manifest_json(7),
    );
    common::write_index(
        &root,
        "index/v7/shard_0",
        &[("doc-1", "rust hybrid search"), ("doc-2", "rust keyword")],
    );
    common::write_lance_fixture(
        &root,
        "lance/v7/shard_0",
        &[
            serde_json::json!({"doc_id":"doc-1","text":"rust hybrid search","embedding":[0.9,0.0,0.0]}),
            serde_json::json!({"doc_id":"doc-2","text":"rust keyword","embedding":[0.8,0.0,0.0]}),
        ],
    );
    let release_id = "c".repeat(64);
    common::write_static_release_fixture(
        &root,
        &release_id,
        &[common::StaticFixtureDoc {
            doc_id: 10,
            corpus_type: 0,
            text: "static legal ten",
            embedding: common::padded_embedding(&[1.2, -1.4, 0.3, 0.9]),
        }],
    );
    let head = StaticReleaseHead::new(release_id.clone(), 1_700_000_000_000);
    common::write_fixture(&root, STATIC_HEAD_KEY, &head.to_json_pretty());

    std::env::set_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER", "fixed");
    std::env::set_var("LTSEARCH_QUERY_ARTIFACT_ROOT", &root);
    std::env::set_var("LTSEARCH_QUERY_FIXED_EMBEDDING", "0.1,0.2,0.3");
    std::env::remove_var("LTSEARCH_QUERY_S3_BUCKET");

    service
        .resolve_handler()
        .expect("expected a successful resolve against the disk fixture");

    assert_eq!(
        service.cached_pair(),
        Some((7, Some(release_id))),
        "cached_pair must return both halves of the resolved cache key"
    );

    std::env::remove_var("LTSEARCH_QUERY_EMBEDDING_PROVIDER");
    std::env::remove_var("LTSEARCH_QUERY_ARTIFACT_ROOT");
    std::env::remove_var("LTSEARCH_QUERY_FIXED_EMBEDDING");
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

// --- Task 11: cache key is now the (dynamic_version, static_release_id) pair. ---
// These exercise the generic `resolve_versioned_handler{,_with_retry}` at
// K = (u64, Option<String>); the two u64 guardrail tests above stay green
// because K is inferred as `u64` there (design acceptance point).

fn cache_test_handler(version: u64, release: Option<String>) -> Arc<QueryRequestHandler> {
    Arc::new(Box::new(move |_request| {
        Ok(SearchResponse {
            static_chunks: vec![],
            static_count: 0,
            dynamic_chunks: vec![],
            dynamic_count: 0,
            latency_ms: 1,
            index_version: version,
            static_release_id: release.clone(),
        })
    }) as QueryRequestHandler)
}

// Dynamic version pinned at 7; the static release pointer flips r1 → r1 → r2.
// The unchanged pair hits the cache; the third pair (new release) rebuilds.
#[test]
fn cache_rebuilds_when_static_release_changes_even_if_dynamic_version_stable() {
    let state = Mutex::new(None);
    let r1 = "a".repeat(64);
    let r2 = "b".repeat(64);
    let keys = [
        (7_u64, Some(r1.clone())),
        (7, Some(r1.clone())),
        (7, Some(r2.clone())),
    ];
    let key_index = AtomicUsize::new(0);
    let bootstrap_calls = AtomicUsize::new(0);

    resolve_versioned_handler(
        &state,
        || Ok(keys[key_index.fetch_add(1, Ordering::SeqCst)].clone()),
        |(version, release)| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            Ok(cache_test_handler(version, release))
        },
    )
    .expect("expected first bootstrap to succeed");

    resolve_versioned_handler(
        &state,
        || Ok(keys[key_index.fetch_add(1, Ordering::SeqCst)].clone()),
        |_key| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            panic!("cache should reuse the handler for an identical (version, release) pair");
        },
    )
    .expect("expected the unchanged pair to be reused");

    let third = resolve_versioned_handler(
        &state,
        || Ok(keys[key_index.fetch_add(1, Ordering::SeqCst)].clone()),
        |(version, release)| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            Ok(cache_test_handler(version, release))
        },
    )
    .expect("expected a static release change to trigger a fresh bootstrap");

    assert_eq!(bootstrap_calls.load(Ordering::SeqCst), 2);
    let response = third(valid_search_request_for_cache_test()).unwrap();
    assert_eq!(response.index_version, 7);
    assert_eq!(response.static_release_id, Some(r2));
}

// Static release pinned at r1; the dynamic version flips 7 → 7 → 8.
// Symmetric to the case above: the changed dynamic half also rebuilds.
#[test]
fn cache_rebuilds_when_dynamic_version_changes_static_stable() {
    let state = Mutex::new(None);
    let r1 = "a".repeat(64);
    let keys = [
        (7_u64, Some(r1.clone())),
        (7, Some(r1.clone())),
        (8, Some(r1.clone())),
    ];
    let key_index = AtomicUsize::new(0);
    let bootstrap_calls = AtomicUsize::new(0);

    resolve_versioned_handler(
        &state,
        || Ok(keys[key_index.fetch_add(1, Ordering::SeqCst)].clone()),
        |(version, release)| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            Ok(cache_test_handler(version, release))
        },
    )
    .expect("expected first bootstrap to succeed");

    resolve_versioned_handler(
        &state,
        || Ok(keys[key_index.fetch_add(1, Ordering::SeqCst)].clone()),
        |_key| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            panic!("cache should reuse the handler for an identical (version, release) pair");
        },
    )
    .expect("expected the unchanged pair to be reused");

    let third = resolve_versioned_handler(
        &state,
        || Ok(keys[key_index.fetch_add(1, Ordering::SeqCst)].clone()),
        |(version, release)| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            Ok(cache_test_handler(version, release))
        },
    )
    .expect("expected a dynamic version change to trigger a fresh bootstrap");

    assert_eq!(bootstrap_calls.load(Ordering::SeqCst), 2);
    let response = third(valid_search_request_for_cache_test()).unwrap();
    assert_eq!(response.index_version, 8);
    assert_eq!(response.static_release_id, Some(r1));
}

// The pair (7, None) repeated three times bootstraps exactly once.
#[test]
fn cache_hits_when_pair_unchanged() {
    let state = Mutex::new(None);
    let bootstrap_calls = AtomicUsize::new(0);

    let first = resolve_versioned_handler(
        &state,
        || Ok((7_u64, None)),
        |(version, release)| {
            bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            Ok(cache_test_handler(version, release))
        },
    )
    .expect("expected first bootstrap to succeed");

    for _ in 0..2 {
        resolve_versioned_handler(
            &state,
            || Ok((7_u64, None)),
            |_key| {
                bootstrap_calls.fetch_add(1, Ordering::SeqCst);
                panic!("cache should reuse the handler while the pair is unchanged");
            },
        )
        .expect("expected the unchanged pair to be reused");
    }

    assert_eq!(bootstrap_calls.load(Ordering::SeqCst), 1);
    let response = first(valid_search_request_for_cache_test()).unwrap();
    assert_eq!(response.index_version, 7);
    assert!(response.static_release_id.is_none());
}

// A dynamic version race mid-bootstrap is covered by the existing single
// retry even with a stable static release — no separate static retriable
// signal is needed (design decision 2).
#[test]
fn retry_covers_dynamic_version_change_with_static_stable() {
    let state = Mutex::new(None);
    let r1 = "a".repeat(64);
    let keys = [
        (7_u64, Some(r1.clone())),
        (8, Some(r1.clone())),
        (8, Some(r1.clone())),
    ];
    let key_index = AtomicUsize::new(0);
    let bootstrap_calls = AtomicUsize::new(0);

    let handler = resolve_versioned_handler_with_retry(
        &state,
        || Ok(keys[key_index.fetch_add(1, Ordering::SeqCst)].clone()),
        |(version, release)| {
            let attempt = bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                return Err(QueryLambdaError {
                    error_type: "execution_error".into(),
                    message: format!(
                        "query lambda bootstrap failed: active manifest version changed during bootstrap: expected {version}, got 8"
                    ),
                });
            }

            Ok(cache_test_handler(version, release))
        },
    )
    .expect("expected one retry to cover the dynamic version race with static stable");

    assert_eq!(bootstrap_calls.load(Ordering::SeqCst), 2);
    let response = handler(valid_search_request_for_cache_test()).unwrap();
    assert_eq!(response.index_version, 8);
    assert_eq!(response.static_release_id, Some(r1));
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
