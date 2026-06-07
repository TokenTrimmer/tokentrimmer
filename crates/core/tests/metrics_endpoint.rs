//! Integration tests for the `/metrics` Prometheus endpoint.
//!
//! The global recorder is shared across this test binary, so assertions check
//! metric/label PRESENCE, not exact counts.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;
use tt_core::{build_router, AppState, ProviderRegistry};

fn router() -> axum::Router {
    build_router(AppState::new(ProviderRegistry::new()))
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let (status, headers, body) = get(router(), "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/plain; version=0.0.4"
    );
    assert!(body.contains("tt_build_info"), "build_info missing: {body}");
    assert!(
        body.contains("process_uptime_seconds"),
        "uptime missing: {body}"
    );
}

#[tokio::test]
async fn render_is_some_after_build_router() {
    let _ = router();
    assert!(tt_core::metrics::render().is_some());
}
