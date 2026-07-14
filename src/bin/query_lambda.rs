use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::models::{SearchRequest, SearchResponse};
use ltsearch::query_lambda::{handle_search_request, QueryLambdaError};
use ltsearch::query_service::QueryService;
use serde::Serialize;
use serde_json::Value;
use std::sync::OnceLock;

static QUERY_SERVICE: OnceLock<QueryService> = OnceLock::new();

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

async fn function_handler(event: LambdaEvent<Value>) -> Result<QueryLambdaPayload, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_request_payload(payload) {
        Ok(request) => request,
        Err(payload) => return Ok(payload),
    };

    let service = QUERY_SERVICE.get_or_init(QueryService::new);

    if let Err(error) = service.sync_artifacts_if_configured().await {
        return Ok(QueryLambdaPayload::Error(QueryLambdaError {
            error_type: "execution_error".into(),
            message: format!("query lambda bootstrap failed: {error}"),
        }));
    }

    let payload = match service.resolve_handler() {
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
    use super::*;

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
}
