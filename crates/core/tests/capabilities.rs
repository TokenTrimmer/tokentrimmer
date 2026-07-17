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

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("capabilities body");
    serde_json::from_slice(&bytes).expect("capabilities JSON")
}

fn assert_unknown_readiness(body: &Value) {
    for field in [
        "provider_credentials",
        "provider_health",
        "model_support",
        "modality_support",
    ] {
        let fact = body.get(field).expect("capability fact");
        assert_eq!(fact["state"].as_str(), Some("unknown"), "{field}: {fact}");
        assert_eq!(
            fact["source"].as_str(),
            Some("not_negotiated"),
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
