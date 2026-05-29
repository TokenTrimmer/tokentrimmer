//! Integration test: POST /v1/preview returns a valid PreviewResponse.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::util::ServiceExt;
use tt_core::{build_router, state::AppState};

#[tokio::test]
async fn preview_returns_shape_for_known_model() {
    let app = build_router(AppState::with_default_providers());
    let body = json!({
        "model": "claude-haiku-4-5",
        "messages": [{"role": "user", "content": "Hello"}],
        "max_tokens": 100
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["current"]["model"], "claude-haiku-4-5");
    assert!(value["current"]["cost_usd"].as_f64().unwrap() > 0.0);
    assert!(value["cache_projections"]["weighted_savings_usd"].as_f64().unwrap() >= 0.0);
}

#[tokio::test]
async fn preview_returns_400_on_unknown_model() {
    let app = build_router(AppState::with_default_providers());
    let body = json!({
        "model": "model-that-does-not-exist",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
