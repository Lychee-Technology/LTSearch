use axum::http::StatusCode;
use ltsearch::http::{error_status, health_response, HealthBody};

#[test]
fn validation_error_maps_to_400_and_others_to_500() {
    assert_eq!(error_status("validation_error"), StatusCode::BAD_REQUEST);
    assert_eq!(
        error_status("execution_error"),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        error_status("operation_error"),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[test]
fn health_response_uses_503_when_not_ok() {
    let ok = health_response(HealthBody {
        status: "ok".into(),
        component: "query".into(),
        index_version: Some(3),
        static_release_id: None,
        detail: None,
    });
    assert_eq!(ok.status(), StatusCode::OK);

    let bad = health_response(HealthBody {
        status: "unavailable".into(),
        component: "query".into(),
        index_version: None,
        static_release_id: None,
        detail: Some("LTEmbed bundle missing".into()),
    });
    assert_eq!(bad.status(), StatusCode::SERVICE_UNAVAILABLE);
}
