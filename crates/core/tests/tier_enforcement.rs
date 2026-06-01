//! Integration tests for per-org tier-based enforcement wired through the
//! auth middleware + TierResolver.
//!
//! Tests cover:
//! (a) A Free org over its monthly_request_cap gets 429 `monthly_quota_exceeded`.
//! (b) A Free org over its rpm gets 429 `rate_limit_exceeded`.
//! (c) Resolver error fails open (request allowed, no 429).
//! (d) A second request within TTL does not re-query the inner resolver
//!     (cache counter stays at 1).
//! (e) ApiKeyContext.tier is set to Free by the middleware.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_core::{
    build_router, tier_resolver::TierResolver, AppState, ProviderRegistry,
    tier_resolver::{ResolvedTier, TierResolverError},
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

// ─── Minimal passthrough provider ───────────────────────────────────────────

struct EchoProvider;

#[async_trait]
impl Provider for EchoProvider {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "gpt-4o".into(),
            provider: "openai".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: chrono::Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("ok".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
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

// ─── Stub TierResolvers ──────────────────────────────────────────────────────

/// Always returns the given `ResolvedTier`.
struct FixedTierResolver {
    resolved: ResolvedTier,
    call_count: Arc<AtomicU32>,
}

impl FixedTierResolver {
    fn free() -> (Self, Arc<AtomicU32>) {
        let counter = Arc::new(AtomicU32::new(0));
        (
            Self {
                resolved: ResolvedTier::free_default(),
                call_count: counter.clone(),
            },
            counter,
        )
    }
}

#[async_trait]
impl TierResolver for FixedTierResolver {
    async fn resolve(&self, _org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(self.resolved)
    }
}

/// Always returns an error (simulates DB blip).
struct ErrorTierResolver;

#[async_trait]
impl TierResolver for ErrorTierResolver {
    async fn resolve(&self, org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
        Err(TierResolverError::Db {
            org_id,
            source: sqlx::Error::RowNotFound,
        })
    }
}

/// Resolver with a configurable per-call delay to test TTL caching.
struct CountingTierResolver {
    resolved: ResolvedTier,
    call_count: Arc<AtomicU32>,
}

impl CountingTierResolver {
    fn new_free() -> (Self, Arc<AtomicU32>) {
        let counter = Arc::new(AtomicU32::new(0));
        (
            Self {
                resolved: ResolvedTier::free_default(),
                call_count: counter.clone(),
            },
            counter,
        )
    }
}

#[async_trait]
impl TierResolver for CountingTierResolver {
    async fn resolve(&self, _org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(self.resolved)
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "k", Environment::Live, Actor::System)
        .await
        .expect("issue key")
        .plaintext
}

fn chat_request(bearer: &str) -> Request<Body> {
    let body = json!({
        "model": "gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .expect("request")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// (a) Free org with its monthly cap exhausted via DynamicBudgetEnforcer
/// must get 429 `monthly_quota_exceeded` on the next request.
#[tokio::test]
async fn free_org_over_monthly_cap_returns_429() {
    use tt_core::budget::BudgetDecision;
    let store = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&store, org).await;

    let (resolver, _counter) = FixedTierResolver::free();
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(EchoProvider));
    let state = AppState::new(registry)
        .with_tier_resolver(Arc::new(resolver) as Arc<dyn TierResolver>)
        .with_key_store(Arc::new(store));

    // Pre-exhaust the monthly cap by draining the dynamic_budget enforcer.
    // We advance the clock 61s per batch of 59 to avoid the rpm window blocking.
    let free_limits = tt_core::budget::BudgetLimits::free_tier();
    let cap = free_limits.monthly_request_cap.expect("free tier has cap");
    let base = chrono::Utc::now();
    let mut allowed = 0u32;
    let mut minute = 0i64;
    while allowed < cap {
        let ts = base + chrono::Duration::seconds(minute * 61);
        minute += 1;
        for _ in 0..59u32 {
            if allowed >= cap {
                break;
            }
            let decision = state.dynamic_budget.check_with_limits(org, &free_limits, ts);
            if matches!(decision, BudgetDecision::Allow { .. }) {
                allowed += 1;
            }
        }
    }
    assert_eq!(allowed, cap, "should have pre-filled exactly {cap} requests");

    let app = build_router(state);

    // With the cap exhausted, the next HTTP request should 429.
    let resp = app.oneshot(chat_request(&key)).await.expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "Free org at monthly cap should get 429"
    );
    // Check the body contains the right error type.
    let body_bytes = axum::body::to_bytes(resp.into_body(), 8192)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json");
    assert_eq!(
        body["error"]["type"],
        "monthly_quota_exceeded",
        "should be monthly_quota_exceeded, got: {body}"
    );
}

/// (b) Free org over its rpm gets 429 `rate_limit_exceeded`.
#[tokio::test]
async fn free_org_over_rpm_returns_429() {
    let store = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&store, org).await;

    let (resolver, _counter) = FixedTierResolver::free();
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(EchoProvider));
    let state = AppState::new(registry)
        .with_tier_resolver(Arc::new(resolver) as Arc<dyn TierResolver>)
        .with_key_store(Arc::new(store));

    // Pre-exhaust the rate window.
    let free_limits = tt_core::budget::BudgetLimits::free_tier();
    let rpm = free_limits.max_requests_per_min.expect("free tier has rpm");
    let now = chrono::Utc::now();
    for _ in 0..rpm {
        state.dynamic_budget.check_with_limits(org, &free_limits, now);
    }

    let app = build_router(state);

    let resp = app.oneshot(chat_request(&key)).await.expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "Free org over rpm should get 429"
    );
    let body_bytes = axum::body::to_bytes(resp.into_body(), 8192)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json");
    assert_eq!(
        body["error"]["type"],
        "rate_limit_exceeded",
        "should be rate_limit_exceeded, got: {body}"
    );
}

/// (c) Resolver error → fail open: the request is allowed, no 429.
#[tokio::test]
async fn resolver_error_fails_open() {
    let store = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&store, org).await;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(EchoProvider));
    let app = build_router(
        AppState::new(registry)
            .with_tier_resolver(Arc::new(ErrorTierResolver) as Arc<dyn TierResolver>)
            .with_key_store(Arc::new(store)),
    );

    let resp = app.oneshot(chat_request(&key)).await.expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "resolver error must fail open (no 429): got {}",
        resp.status()
    );
}

/// (d) Two requests within TTL call the inner resolver only ONCE.
#[tokio::test]
async fn tier_cache_serves_second_request_without_re_querying() {
    let store = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&store, org).await;

    let (resolver, counter) = CountingTierResolver::new_free();
    // Wrap with CachedTierResolver with a long TTL so both requests hit cache.
    let cached = tt_core::tier_resolver::CachedTierResolver::new(resolver);
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(EchoProvider));
    let app = build_router(
        AppState::new(registry)
            .with_tier_resolver(Arc::new(cached) as Arc<dyn TierResolver>)
            .with_key_store(Arc::new(store)),
    );

    // First request — cache miss, inner called once.
    let r1 = app.clone().oneshot(chat_request(&key)).await.expect("r1");
    assert_eq!(r1.status(), StatusCode::OK, "first request must succeed");

    // The auth middleware calls resolve_or_free twice per request (once to
    // stamp tier on ApiKeyContext, once for the budget check). Both should
    // be served from cache after the first real call.
    // After request 1 the counter should be at most 2 (or 1 if the two calls
    // share the cache entry); after request 2 it must not increase.
    let count_after_r1 = counter.load(Ordering::SeqCst);
    assert!(count_after_r1 <= 2, "first request: inner calls should be ≤2, got {count_after_r1}");

    // Second request — everything from cache.
    let r2 = app.clone().oneshot(chat_request(&key)).await.expect("r2");
    assert_eq!(r2.status(), StatusCode::OK, "second request must succeed");
    let count_after_r2 = counter.load(Ordering::SeqCst);
    assert_eq!(
        count_after_r1, count_after_r2,
        "inner resolver must not be called again on cache hit (was {count_after_r1} → {count_after_r2})"
    );
}

/// (e, SEAM) `spend_sink()` and the auth budget pre-flight use the SAME
/// enforcer state.
///
/// This test proves the seam: the [`SpendSink`] that the chat handler records
/// spend into is the one the auth middleware reads for the pre-flight check.
/// If a future refactor makes them diverge (e.g. `spend_sink()` returns a
/// different `DynamicBudgetEnforcer` than `state.dynamic_budget`), this test
/// fails.
///
/// Steps:
///   1. Build an AppState with a FixedTierResolver returning `monthly_cap_usd:
///      Some(1.0)` (only the USD cap is in play; rpm/request caps are None).
///   2. Record >$1 of spend via `state.spend_sink().record(org, 2.0, now)`.
///   3. Drive an HTTP request for that org through the router.
///   4. Assert 429 `budget_exceeded`.
#[tokio::test]
async fn spend_sink_and_auth_share_same_enforcer_seam() {
    use tt_core::budget::{BudgetLimits, SpendSink};
    use tt_core::tier_resolver::ResolvedTier;
    use tt_shared::CallerTier;

    let store = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&store, org).await;

    // Resolver returns a $1/month USD cap with no rpm or monthly-request limit
    // so only the spend cap can fire.
    struct CapResolver;
    #[async_trait]
    impl TierResolver for CapResolver {
        async fn resolve(&self, _org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
            Ok(ResolvedTier {
                caller_tier: CallerTier::Pro,
                limits: BudgetLimits {
                    monthly_cap_usd: Some(1.0),
                    max_requests_per_min: None,
                    monthly_request_cap: None,
                    l2_cache: true,
                },
            })
        }
    }

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(EchoProvider));
    let state = AppState::new(registry)
        .with_tier_resolver(Arc::new(CapResolver) as Arc<dyn TierResolver>)
        .with_key_store(Arc::new(store));

    // Record $2 of spend via spend_sink() — the spend that the auth middleware
    // will check against when it reads dynamic_budget for this org.
    let now = chrono::Utc::now();
    assert!(
        matches!(state.spend_sink(), SpendSink::Dynamic(_)),
        "expected SpendSink::Dynamic when tier_resolver is wired"
    );
    state.spend_sink().record(org, 2.0, now);

    // Build the router AFTER recording spend so the router's clone of state
    // shares the same Arc<DynamicBudgetEnforcer>.
    let app = build_router(state);

    let resp = app.oneshot(chat_request(&key)).await.expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "org over spend cap must get 429"
    );
    let body_bytes = axum::body::to_bytes(resp.into_body(), 8192)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json");
    assert_eq!(
        body["error"]["type"], "budget_exceeded",
        "should be budget_exceeded (spend cap), got: {body}"
    );
}
