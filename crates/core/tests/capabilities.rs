//! HTTP contract tests for the authenticated gateway-runtime capabilities
//! snapshot. These deliberately use issued `tt_live_*` keys: sandbox and
//! dogfood identities are not organization capability evidence.

use std::sync::Arc;

use async_trait::async_trait;
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
    routes::panel::panel_max_members,
    tier_resolver::{ResolvedTier, TierResolver, TierResolverError},
    AppState, ProviderRegistry,
};
use tt_shared::CallerTier;
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Minimal resolver that lets an issued live key carry a selected tier.
struct FixedTierResolver {
    tier: CallerTier,
}

#[async_trait]
impl TierResolver for FixedTierResolver {
    async fn resolve(&self, _org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
        let mut resolved = ResolvedTier::free_default();
        resolved.caller_tier = self.tier;
        Ok(resolved)
    }
}

async fn issue_live_key(store: &InMemoryKeyStore) -> String {
    issue(
        store,
        &InMemoryAuditWriter::new(),
        Uuid::now_v7(),
        "capabilities-test",
        Environment::Live,
        Actor::System,
    )
    .await
    .expect("issue live key")
    .plaintext
}

async fn app_with_live_key(
    panel_enabled: bool,
    panel_min_tier: CallerTier,
    caller_tier: CallerTier,
) -> (axum::Router, String) {
    let key_store = Arc::new(InMemoryKeyStore::new());
    let key = issue_live_key(key_store.as_ref()).await;
    let state = AppState::new(ProviderRegistry::new())
        .with_panel_enabled(panel_enabled)
        .with_panel_min_tier(panel_min_tier)
        .with_key_store(key_store)
        .with_tier_resolver(Arc::new(FixedTierResolver { tier: caller_tier }));
    (build_router(state), key)
}

fn capabilities_request(bearer: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri("/v1/capabilities");
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    builder.body(Body::empty()).expect("capabilities request")
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

fn preflight_batch_request(bearer: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/capabilities/preflight/batch")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("preflight batch request")
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("capabilities body");
    serde_json::from_slice(&bytes).expect("capabilities JSON")
}

fn assert_unknown_readiness(body: &Value) {
    for (field, reason_code) in [
        ("provider_credentials", "provider_credentials_not_inspected"),
        ("provider_health", "provider_health_not_probed"),
        ("model_support", "model_support_not_negotiated"),
        ("modality_support", "modality_support_not_negotiated"),
    ] {
        let fact = body.get(field).expect("capability fact");
        assert_eq!(fact["state"].as_str(), Some("unknown"), "{field}: {fact}");
        assert_eq!(
            fact["source"].as_str(),
            Some("not_negotiated"),
            "{field}: {fact}"
        );
        assert_eq!(
            fact["reason"]["code"].as_str(),
            Some(reason_code),
            "{field}: {fact}"
        );
    }
}

#[tokio::test]
async fn capabilities_rejects_anonymous_sandbox_and_dogfood_traffic() {
    let anonymous_app = build_router(AppState::new(ProviderRegistry::new()));
    let anonymous = anonymous_app
        .oneshot(capabilities_request(None))
        .await
        .expect("anonymous response");
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let sandbox_app = build_router(AppState::new(ProviderRegistry::new()));
    let sandbox = sandbox_app
        .oneshot(capabilities_request(Some("tt_test_capabilities")))
        .await
        .expect("sandbox response");
    assert_eq!(sandbox.status(), StatusCode::UNAUTHORIZED);

    let dogfood_app = build_router(AppState::new(ProviderRegistry::new()).with_dogfood_enabled());
    let dogfood = dogfood_app
        .oneshot(capabilities_request(None))
        .await
        .expect("dogfood response");
    assert_eq!(dogfood.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn capabilities_reports_the_real_gate_and_keeps_readiness_unknown() {
    let (app, key) = app_with_live_key(true, CallerTier::Pro, CallerTier::Free).await;
    let response = app
        .oneshot(capabilities_request(Some(&key)))
        .await
        .expect("capabilities response");

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
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["scope"], "gateway_runtime");
    assert_eq!(body["snapshot_scope"], "responding_process");
    assert_eq!(body["features"]["fusion"]["enabled"]["state"], "enabled");
    assert_eq!(body["features"]["fusion"]["access"]["state"], "unavailable");
    assert_eq!(
        body["features"]["fusion"]["access"]["reason"]["code"],
        "fusion_tier_below_minimum"
    );
    assert_eq!(body["features"]["fusion"]["current_tier"]["value"], "free");
    assert_eq!(body["features"]["fusion"]["minimum_tier"]["value"], "pro");
    assert_eq!(
        body["features"]["fusion"]["limits"]["member_models_max"]["value"],
        serde_json::json!(panel_max_members())
    );
    assert_unknown_readiness(&body);

    let serialized = body.to_string();
    assert!(
        body.get("org_id").is_none(),
        "response must not expose org IDs"
    );
    assert!(
        body.get("key_id").is_none(),
        "response must not expose key IDs"
    );
    assert!(
        !serialized.contains(&key),
        "response must not expose the bearer credential"
    );
}

#[tokio::test]
async fn capabilities_reports_an_allowed_fusion_gate_without_provider_claims() {
    let (app, key) = app_with_live_key(true, CallerTier::Pro, CallerTier::Pro).await;
    let response = app
        .oneshot(capabilities_request(Some(&key)))
        .await
        .expect("capabilities response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["features"]["fusion"]["access"]["state"], "available");
    assert_eq!(
        body["features"]["fusion"]["access"]["reason"]["code"],
        "fusion_gateway_gate_passed"
    );
    assert_eq!(body["features"]["fusion"]["current_tier"]["value"], "pro");
    assert_unknown_readiness(&body);
}

#[tokio::test]
async fn capabilities_prioritizes_the_disabled_kill_switch() {
    let (app, key) = app_with_live_key(false, CallerTier::Pro, CallerTier::Pro).await;
    let response = app
        .oneshot(capabilities_request(Some(&key)))
        .await
        .expect("capabilities response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["features"]["fusion"]["enabled"]["state"], "disabled");
    assert_eq!(
        body["features"]["fusion"]["access"]["reason"]["code"],
        "fusion_disabled"
    );
}

#[tokio::test]
async fn request_preflight_is_authenticated_bounded_non_dispatch_evidence() {
    let anonymous = build_router(AppState::new(ProviderRegistry::new()))
        .oneshot(preflight_request(
            None,
            serde_json::json!({
                "schema_version": 1,
                "model": "gpt-4o-mini",
                "provider": null,
                "required_capabilities": ["text"],
                "declared_input_tokens": 100,
                "requested_max_output_tokens": 100
            }),
        ))
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
            Some(&key),
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
