use lambda_runtime::{service_fn, Error, LambdaEvent};
use ltsearch::lambda_events::{ApiGatewayV2Request, ApiGatewayV2Response};
use ltsearch::models::SearchRequest;
use ltsearch::query_lambda::handle_search_request;
use ltsearch::query_service::QueryService;
use serde_json::Value;
use std::sync::OnceLock;

static QUERY_SERVICE: OnceLock<QueryService> = OnceLock::new();

/// 事件 → SearchRequest：信封解析失败与 body 反序列化失败都折叠成 400 响应。
fn decode_search_request(payload: Value) -> Result<SearchRequest, ApiGatewayV2Response> {
    let event: ApiGatewayV2Request = serde_json::from_value(payload).map_err(|source| {
        ApiGatewayV2Response::error(
            "validation_error",
            format!("failed to deserialize API Gateway event: {source}"),
        )
    })?;
    let bytes = event
        .body_bytes()
        .map_err(|error| ApiGatewayV2Response::error("validation_error", error))?;
    serde_json::from_slice(&bytes).map_err(|source| {
        ApiGatewayV2Response::error(
            "validation_error",
            format!("failed to deserialize search request: {source}"),
        )
    })
}

async fn function_handler(event: LambdaEvent<Value>) -> Result<ApiGatewayV2Response, Error> {
    let (payload, _) = event.into_parts();
    let request = match decode_search_request(payload) {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };

    let service = QUERY_SERVICE.get_or_init(QueryService::new);

    if let Err(error) = service.sync_artifacts_if_configured().await {
        return Ok(ApiGatewayV2Response::error(
            "execution_error",
            format!("query lambda bootstrap failed: {error}"),
        ));
    }

    let response = match service.resolve_handler() {
        Ok(handler) => match handle_search_request(handler.as_ref(), request) {
            Ok(response) => ApiGatewayV2Response::json(200, &response),
            Err(error) => ApiGatewayV2Response::error(&error.error_type, error.message),
        },
        Err(error) => ApiGatewayV2Response::error(&error.error_type, error.message),
    };

    Ok(response)
}

fn main() -> Result<(), Error> {
    tokio::runtime::Runtime::new()?
        .block_on(async { lambda_runtime::run(service_fn(function_handler)).await })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_apigw_event_decodes_search_request() {
        let payload = json!({
            "version": "2.0",
            "rawPath": "/query",
            "body": "{\"query\":\"rust retrieval\",\"top_k\":3,\"filters\":null,\"include_metadata\":true}",
            "isBase64Encoded": false
        });

        let request = decode_search_request(payload).expect("valid event should decode");
        assert_eq!(request.query, "rust retrieval");
        assert_eq!(request.top_k, 3);
    }

    #[test]
    fn malformed_body_returns_400_envelope() {
        let payload = json!({"rawPath": "/query", "body": "{\"top_k\": \"wrong\"}"});

        let response = decode_search_request(payload).expect_err("must produce error envelope");
        assert_eq!(response.status_code, 400);
        assert!(response.body.contains("validation_error"));
        assert!(response
            .body
            .contains("failed to deserialize search request"));
    }

    #[test]
    fn non_apigw_event_returns_400_envelope() {
        // 直调裸 JSON（旧契约）不再被接受：信封字段类型不匹配 → 400。
        let response =
            decode_search_request(json!({"body": 42})).expect_err("bare payload must be rejected");
        assert_eq!(response.status_code, 400);
        assert!(response
            .body
            .contains("failed to deserialize API Gateway event"));
    }
}
