//! HTTP contract tests for authenticated, request-specific local preflight.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use serde_json::Value;
use tower::util::ServiceExt;
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_core::{
    build_router,
    registry::{register_providers, ProvidersConfig},
    AppState, ProviderRegistry,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

async fn issue_live_key(store: &InMemoryKeyStore) -> String {
    issue(
        store,
        &InMemoryAuditWriter::new(),
        Uuid::now_v7(),
        "request-preflight-test",
        Environment::Live,
        Actor::System,
    )
    .await
    .expect("issue live key")
    .plaintext
}

fn preflight_request(bearer: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/capabilities/preflight")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("preflight request")
}

fn preflight_batch_request(bearer: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/capabilities/preflight/batch")
        .header(header::CONTENT_TYPE, "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .expect("preflight batch request")
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("preflight body");
    serde_json::from_slice(&bytes).expect("preflight JSON")
}

#[tokio::test]
async fn request_preflight_is_authenticated_bounded_non_dispatch_evidence() {
    let declaration = serde_json::json!({
        "schema_version": 1,
        "model": "gpt-4o-mini",
        "provider": null,
        "required_capabilities": ["text"],
        "declared_input_tokens": 100,
        "requested_max_output_tokens": 100
    });
    let anonymous = build_router(AppState::new(ProviderRegistry::new()))
        .oneshot(preflight_request(None, declaration))
        .await
        .expect("anonymous preflight");
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let key_store = Arc::new(InMemoryKeyStore::new());
    let key = issue_live_key(key_store.as_ref()).await;
    let mut registry = ProviderRegistry::new();
    let mut providers = ProvidersConfig::none();
    providers.openai = true;
    register_providers(&mut registry, &providers);
    let app = build_router(AppState::new(registry).with_key_store(key_store));
    let response = app
        .clone()
        .oneshot(preflight_request(
            Some(&key),
            serde_json::json!({
                "schema_version": 1,
                "model": "gpt-4o-mini",
                "provider": null,
                "required_capabilities": ["text", "tools"],
                "declared_input_tokens": 128001,
                "requested_max_output_tokens": 4096
            }),
        ))
        .await
        .expect("authenticated preflight");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("private, no-store")
    );
    assert_eq!(
        response
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    let body = body_json(response).await;
    assert_eq!(body["scope"], "request_preflight");
    assert_eq!(body["provider_resolution"]["state"], "exact_catalog_match");
    assert_eq!(body["credential"]["state"], "unknown");
    assert_eq!(body["model_support"]["state"], "supported_by_catalog");
    assert_eq!(body["catalog_limits"]["state"], "exceeds_catalog_metadata");
    assert_eq!(body["catalog_cost"]["state"], "catalog_projection");
    assert_eq!(
        body["catalog_cost"]["source"],
        "registered_provider_pricing_catalog"
    );
    assert_eq!(body["catalog_cost"]["input_tokens_low"], 128001);
    assert_eq!(body["catalog_cost"]["input_tokens_high"], 128001);
    assert_eq!(body["catalog_cost"]["output_tokens_low"], 0);
    assert_eq!(body["catalog_cost"]["output_tokens_high"], 4096);
    assert!(body["catalog_cost"]["standard_cost_usd_low"]
        .as_f64()
        .is_some_and(|value| value >= 0.0));
    assert!(body["catalog_cost"]["standard_cost_usd_high"]
        .as_f64()
        .is_some_and(|value| value > 0.0));
    assert_eq!(body["provider_health"]["state"], "unknown");
    assert_eq!(body["request_acceptance"]["state"], "unknown");
    assert!(body["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .any(|action| action["code"] == "reduce_declared_tokens_or_choose_model"));
    let serialized = body.to_string();
    assert!(!serialized.contains(&key));
    assert!(!serialized.contains("org_id"));
    assert!(!serialized.contains("key_id"));

    let batch_response = app
        .oneshot(preflight_batch_request(
            &key,
            serde_json::json!({
                "schema_version": 1,
                "requests": [
                    {
                        "schema_version": 1,
                        "model": "gpt-4o-mini",
                        "provider": null,
                        "required_capabilities": ["text"],
                        "declared_input_tokens": 100,
                        "requested_max_output_tokens": 100
                    },
                    {
                        "schema_version": 1,
                        "model": "gpt-4o",
                        "provider": null,
                        "required_capabilities": ["text", "streaming"],
                        "declared_input_tokens": null,
                        "requested_max_output_tokens": 100
                    }
                ]
            }),
        ))
        .await
        .expect("authenticated batch preflight");
    assert_eq!(batch_response.status(), StatusCode::OK);
    assert_eq!(
        batch_response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("private, no-store")
    );
    let batch = body_json(batch_response).await;
    assert_eq!(batch["scope"], "request_preflight_batch");
    assert_eq!(batch["documents"].as_array().map(Vec::len), Some(2));
    assert_eq!(batch["documents"][0]["request"]["model"], "gpt-4o-mini");
    assert_eq!(batch["documents"][1]["request"]["model"], "gpt-4o");
    assert_eq!(batch["documents"][0]["generated_at"], batch["generated_at"]);
    assert_eq!(batch["documents"][1]["generated_at"], batch["generated_at"]);
    assert_eq!(
        batch["limitations"][0]["code"],
        "preflight_batch_single_responder_not_atomic"
    );
}
