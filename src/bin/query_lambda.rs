use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::models::{SearchRequest, SearchResponse};
use ltsearch::query_lambda::{
    bootstrap_query_handler_for_version_from_env, handle_search_request,
    is_retriable_bootstrap_version_change, load_active_query_version_from_env, QueryLambdaError,
    SharedQueryRequestHandler,
};
use serde::Serialize;
use serde_json::Value;
use std::sync::{Mutex, OnceLock};

#[derive(Clone)]
struct CachedQueryHandler {
    version_id: u64,
    handler: SharedQueryRequestHandler,
}

static QUERY_HANDLER: OnceLock<Mutex<Option<CachedQueryHandler>>> = OnceLock::new();

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum QueryLambdaPayload {
    Success(SearchResponse),
    Error(QueryLambdaError),
}

fn decode_request_payload(payload: Value) -> Result<SearchRequest, QueryLambdaPayload> {
    serde_json::from_value(payload).map_err(|source| {
        QueryLambdaPayload::Error(QueryLambdaError {
            error_type: "validation_error".into(),
            message: format!("failed to deserialize search request: {source}"),
        })
    })
}

fn resolve_versioned_handler<V, B>(
    cache: &Mutex<Option<CachedQueryHandler>>,
    load_active_version: V,
    bootstrap: B,
) -> Result<SharedQueryRequestHandler, QueryLambdaError>
where
    V: FnOnce() -> Result<u64, QueryLambdaError>,
    B: FnOnce(u64) -> Result<SharedQueryRequestHandler, QueryLambdaError>,
{
    let version_id = load_active_version()?;
    let mut state = cache.lock().expect("query handler cache lock poisoned");

    if let Some(cached) = state.as_ref() {
        if cached.version_id == version_id {
            return Ok(cached.handler.clone());
        }
    }

    let handler = bootstrap(version_id)?;
    *state = Some(CachedQueryHandler {
        version_id,
        handler: handler.clone(),
    });
    Ok(handler)
}

fn resolve_versioned_handler_with_retry<V, B>(
    cache: &Mutex<Option<CachedQueryHandler>>,
    mut load_active_version: V,
    bootstrap: B,
) -> Result<SharedQueryRequestHandler, QueryLambdaError>
where
    V: FnMut() -> Result<u64, QueryLambdaError>,
    B: Fn(u64) -> Result<SharedQueryRequestHandler, QueryLambdaError>,
{
    match resolve_versioned_handler(cache, &mut load_active_version, &bootstrap) {
        Ok(handler) => Ok(handler),
        Err(error) if is_retriable_bootstrap_version_change(&error) => {
            resolve_versioned_handler(cache, load_active_version, &bootstrap)
        }
        Err(error) => Err(error),
    }
}

fn query_handler() -> Result<SharedQueryRequestHandler, QueryLambdaError> {
    let cache = QUERY_HANDLER.get_or_init(|| Mutex::new(None));
    resolve_versioned_handler_with_retry(
        cache,
        load_active_query_version_from_env,
        |expected_version| {
            bootstrap_query_handler_for_version_from_env(expected_version)
                .map(SharedQueryRequestHandler::new)
        },
    )
}

async fn function_handler(event: LambdaEvent<Value>) -> Result<QueryLambdaPayload, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_request_payload(payload) {
        Ok(request) => request,
        Err(payload) => return Ok(payload),
    };

    let payload = match query_handler() {
        Ok(handler) => match handle_search_request(handler.as_ref(), request) {
            Ok(response) => QueryLambdaPayload::Success(response),
            Err(error) => QueryLambdaPayload::Error(error),
        },
        Err(error) => QueryLambdaPayload::Error(error),
    };

    Ok(payload)
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?
        .block_on(async { lambda_runtime::run(service_fn(function_handler)).await })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use ltsearch::query_lambda::QueryRequestHandler;

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
                        results: vec![],
                        total_count: 0,
                        latency_ms: 1,
                        index_version: version,
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
                        results: vec![],
                        total_count: 0,
                        latency_ms: 1,
                        index_version: version,
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
                        results: vec![],
                        total_count: 0,
                        latency_ms: 1,
                        index_version: version,
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

    #[test]
    fn malformed_event_payload_returns_typed_error_envelope() {
        let payload = decode_request_payload(serde_json::json!({"top_k": "wrong"}));

        match payload {
            Ok(_) => panic!("expected malformed payload to return an error envelope"),
            Err(payload) => match payload {
                QueryLambdaPayload::Success(_) => {
                    panic!("expected malformed payload to produce an error envelope")
                }
                QueryLambdaPayload::Error(error) => {
                    assert_eq!(error.error_type, "validation_error");
                    assert!(error
                        .message
                        .contains("failed to deserialize search request"));
                }
            },
        }
    }

    fn valid_search_request_for_cache_test() -> SearchRequest {
        SearchRequest {
            query: "rust".into(),
            top_k: 1,
            filters: None,
            include_metadata: false,
        }
    }
}
