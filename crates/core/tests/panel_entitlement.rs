//! Phase 7 / Task 2 — CallerTier entitlement gate for the deep-research panel.
//!
//! Tests the `TT_PANEL_MIN_TIER` gate wired into `prepare` (chat.rs), exercising:
//!   1. Default allow-all: no `with_panel_min_tier` → Free/None caller gets 200.
//!   2. Gate bites: `with_panel_min_tier(Pro)` → Free caller 403, Pro caller 200;
//!      zero upstream dispatches on the blocked path.
//!   3. Order: kill-switch first — `panel_enabled(false)` + min-tier(Pro) + Free
//!      caller → 403 `panel_disabled` (NOT the entitlement `operation_not_permitted`).

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_core::{
    build_router,
    tier_resolver::{ResolvedTier, TierResolver, TierResolverError},
    AppState, ProviderRegistry,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    CallerTier, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Minimal counted mock provider
// ---------------------------------------------------------------------------

struct CountedMock {
    id: &'static str,
    model: &'static str,
    calls: Arc<AtomicUsize>,
}

impl CountedMock {
    fn new(id: &'static str, model: &'static str, calls: Arc<AtomicUsize>) -> Self {
        Self { id, model, calls }
    }
}

#[async_trait]
impl Provider for CountedMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: 1.0,
                output_per_million: 2.0,
                cached_input_per_million: None,
                cache_write_per_million: None,
                batch_input_per_million: None,
                batch_output_per_million: None,
                flex_input_per_million: None,
                flex_output_per_million: None,
                prompt_cache_min_tokens: None,
                effective_at: Utc::now(),
            })
        } else {
            None
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("answer".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

// ---------------------------------------------------------------------------
// Tier resolvers for injecting specific caller tiers
// ---------------------------------------------------------------------------

/// Always resolves to the given `CallerTier` with uncapped limits.
struct FixedTierResolver {
    tier: CallerTier,
}

impl FixedTierResolver {
    fn pro() -> Self {
        Self {
            tier: CallerTier::Pro,
        }
    }
}

#[async_trait]
impl TierResolver for FixedTierResolver {
    async fn resolve(&self, _org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
        use tt_core::budget::BudgetLimits;
        Ok(ResolvedTier {
            caller_tier: self.tier,
            limits: BudgetLimits {
                monthly_cap_usd: None,
                max_requests_per_min: None,
                monthly_request_cap: None,
                monthly_served_cap: None,
                l2_cache: self.tier != CallerTier::Free,
            },
            semantic_cache_disabled: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Issue an API key for a fresh org, returning `(plaintext_key, org_id)`.
async fn issue_key(store: &InMemoryKeyStore) -> (String, Uuid) {
    let org = Uuid::now_v7();
    let audit = InMemoryAuditWriter::new();
    let issued = issue(store, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .expect("issue key");
    (issued.plaintext, org)
}

/// Read the JSON body of a response (up to 256 KiB).
async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 256 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Build a panel request (no key — caller is anonymous = None tier = Free).
fn panel_req_anonymous() -> Request<Body> {
    panel_req_with_bearer("test")
}

/// Build a panel request with an explicit bearer token.
fn panel_req_with_bearer(bearer: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-tokentrimmer-panel", "synthesize")
        // Generous ceiling so the budget gate doesn't fire on us.
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": false,
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap()
}

/// Build an app with two models (panel-capable), panel enabled, optional
/// `min_tier`, optional key-store + tier-resolver for injecting real tiers.
fn build_panel_app(
    panel_enabled: bool,
    min_tier: Option<CallerTier>,
    key_store: Option<Arc<InMemoryKeyStore>>,
    tier_resolver: Option<Arc<dyn TierResolver>>,
) -> (axum::Router, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountedMock::new(
        "openai",
        "gpt-4o",
        Arc::clone(&calls),
    )));
    registry.register(Arc::new(CountedMock::new(
        "openai-mini",
        "gpt-4o-mini",
        Arc::clone(&calls),
    )));
    let mut state = AppState::new(registry).with_panel_enabled(panel_enabled);
    if let Some(t) = min_tier {
        state = state.with_panel_min_tier(t);
    }
    if let Some(ks) = key_store {
        state = state.with_key_store(ks);
    }
    if let Some(tr) = tier_resolver {
        state = state.with_tier_resolver(tr);
    }
    (build_router(state), calls)
}

// ============================================================================
// TEST 1: Default allow-all — no with_panel_min_tier → Free/None caller gets 200
// ============================================================================

#[tokio::test]
async fn default_allow_all_free_caller_gets_200() {
    // No min_tier set — defaults to Free (allow-all). Anonymous bearer = tier None.
    let (app, _calls) = build_panel_app(true, None, None, None);

    let resp = app.oneshot(panel_req_anonymous()).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "default allow-all: Free/None caller must get 200, got {}",
        resp.status()
    );

    let body = body_json(resp).await;
    assert!(
        body["tokentrimmer"]["panel"].is_object(),
        "response must carry tokentrimmer.panel, got {body}"
    );
}

// ============================================================================
// TEST 2a: Gate bites — Free/None caller gets 403 when min_tier = Pro
// ============================================================================

#[tokio::test]
async fn entitlement_gate_blocks_free_caller_with_zero_dispatches() {
    // min_tier = Pro; anonymous request = tier None = falls back to Free.
    let (app, calls) = build_panel_app(true, Some(CallerTier::Pro), None, None);

    let resp = app.oneshot(panel_req_anonymous()).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "min_tier=Pro gate must return 403 for Free/None caller, got {}",
        resp.status()
    );

    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "operation_not_permitted",
        "error code must be operation_not_permitted (Forbidden), got {body}"
    );

    // CRITICAL: zero dispatches — gate fires BEFORE any provider call.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "INVARIANT: entitlement gate must fire before any provider dispatch (zero calls)"
    );
}

// ============================================================================
// TEST 2b: Pro caller gets 200 when min_tier = Pro
// ============================================================================

#[tokio::test]
async fn entitlement_gate_allows_pro_caller() {
    let store = Arc::new(InMemoryKeyStore::new());
    let (bearer, _org) = issue_key(&store).await;

    let resolver: Arc<dyn TierResolver> = Arc::new(FixedTierResolver::pro());
    let (app, _calls) = build_panel_app(
        true,
        Some(CallerTier::Pro),
        Some(Arc::clone(&store)),
        Some(resolver),
    );

    let resp = app.oneshot(panel_req_with_bearer(&bearer)).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "min_tier=Pro gate must allow Pro caller (200), got {}",
        resp.status()
    );

    let body = body_json(resp).await;
    assert!(
        body["tokentrimmer"]["panel"].is_object(),
        "response must carry tokentrimmer.panel, got {body}"
    );
}

// ============================================================================
// TEST 3: Order — kill-switch fires BEFORE entitlement check.
//
// panel_enabled(false) + min_tier(Pro) + Free caller → 403 panel_disabled,
// NOT operation_not_permitted. The kill-switch must run first.
// ============================================================================

#[tokio::test]
async fn kill_switch_fires_before_entitlement_gate() {
    // panel_enabled=false; min_tier=Pro; anonymous = Free.
    let (app, calls) = build_panel_app(false, Some(CallerTier::Pro), None, None);

    let resp = app.oneshot(panel_req_anonymous()).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "kill-switch + entitlement: must 403 on kill-switch, got {}",
        resp.status()
    );

    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "panel_disabled",
        "kill-switch must fire first (panel_disabled), not entitlement. got {body}"
    );

    // Zero dispatches on kill-switch path.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "kill-switch must fire before any provider dispatch"
    );
}

// ============================================================================
// UNIT TESTS — panel_tier_rank ordering + panel_min_tier_from_env parse
// ============================================================================

#[cfg(test)]
mod unit {
    use tt_core::panel_min_tier_from_env;
    use tt_shared::CallerTier;

    #[test]
    fn panel_min_tier_from_env_parse() {
        let key = "TT_PANEL_MIN_TIER";
        let prior = std::env::var(key).ok();

        // Absent → Free (allow-all default)
        std::env::remove_var(key);
        assert_eq!(
            panel_min_tier_from_env(),
            CallerTier::Free,
            "absent => Free (allow-all)"
        );

        // "pro" → Pro
        std::env::set_var(key, "pro");
        assert_eq!(panel_min_tier_from_env(), CallerTier::Pro, "\"pro\" => Pro");

        // "PRO" → Pro (case-insensitive)
        std::env::set_var(key, "PRO");
        assert_eq!(
            panel_min_tier_from_env(),
            CallerTier::Pro,
            "\"PRO\" => Pro (case-insensitive)"
        );

        // "team" → Team
        std::env::set_var(key, "team");
        assert_eq!(
            panel_min_tier_from_env(),
            CallerTier::Team,
            "\"team\" => Team"
        );

        // "scale" → Scale
        std::env::set_var(key, "scale");
        assert_eq!(
            panel_min_tier_from_env(),
            CallerTier::Scale,
            "\"scale\" => Scale"
        );

        // "free" → Free
        std::env::set_var(key, "free");
        assert_eq!(
            panel_min_tier_from_env(),
            CallerTier::Free,
            "\"free\" => Free"
        );

        // Unknown → Free (default)
        std::env::set_var(key, "enterprise");
        assert_eq!(
            panel_min_tier_from_env(),
            CallerTier::Free,
            "unknown value => Free"
        );

        // Restore
        match prior {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
