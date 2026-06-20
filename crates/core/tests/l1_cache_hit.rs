//! End-to-end tests for the L1 exact-match cache integration in the chat route.
//!
//! Covers:
//! 1. First request misses, dispatches to provider, sets `X-TokenTrimmer-Cache: miss`.
//! 2. Same request again hits cache, returns cached body with `hit-l1` header,
//!    and the provider is NOT called a second time.
//! 3. Streaming variant of a cached prompt fake-streams the cached body as SSE
//!    (`w7-fake-stream-cache`).
//! 4. Streaming request with no L1 entry still dispatches live.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

struct CountingProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &'static str {
        "counting"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "counting-1".into(),
            provider: "counting".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "chatcmpl-live".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("live from provider".into())),
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        let chunks = vec![Ok(ChatCompletionChunk {
            id: "c1".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: "counting-1".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: Some("assistant".into()),
                    content: Some("hi".into()),
                    tool_calls: vec![],
                    extra: Default::default(),
                },
                finish_reason: Some("stop".into()),
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        })];
        Ok(futures::stream::iter(chunks).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

fn chat_request(model: &str, stream: bool) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello there" }],
        "stream": stream,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn l1_miss_then_hit_serves_cached_response_without_second_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1.clone(), None);
    let app = build_router(state);

    // First request — miss path, dispatches to provider.
    let r1 = app
        .clone()
        .oneshot(chat_request("counting-1", false))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(
        r1.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("miss"),
        "first request should be a cache miss"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // Allow the spawned L1 insert to land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second request — should hit L1, bypass the provider.
    let r2 = app
        .oneshot(chat_request("counting-1", false))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "second request should hit L1"
    );
    assert_eq!(
        r2.headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("cache"),
        "L1 hits report provider=cache"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider must NOT be called on L1 hit"
    );

    // Baseline-from-envelope: the hit response should report the SAME baseline
    // that the original miss computed via real pricing — not the conservative
    // $1/M-input + $2/M-output synthetic fallback. The CountingProvider above
    // bills $3/M input + $6/M output, so a 100/50-token usage yields a
    // baseline of $3/M × 100 + $6/M × 50 = 0.0006.
    let baseline_hit: f64 = r2.headers()["x-tokentrimmer-baseline-cost-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let expected_baseline = (100.0 * 3.0 + 50.0 * 6.0) / 1_000_000.0;
    assert!(
        (baseline_hit - expected_baseline).abs() < 1e-9,
        "L1 hit should report envelope baseline ({expected_baseline}); got {baseline_hit}"
    );
    // saved_usd on a hit equals baseline (cost is 0).
    let saved_hit: f64 = r2.headers()["x-tokentrimmer-saved-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (saved_hit - expected_baseline).abs() < 1e-9,
        "L1 hit saved should equal baseline ({expected_baseline}); got {saved_hit}"
    );

    // The cached body should still be valid JSON with the original id/choices.
    let bytes = to_bytes(r2.into_body(), 8 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["id"], "chatcmpl-live");
    assert!(!body["choices"].as_array().unwrap().is_empty());
}

/// Backward-compat: a pre-envelope L1 entry (raw `ChatCompletionResponse`
/// bytes written by an older gateway version) must still resolve to a usable
/// hit. The baseline falls back to the synthetic-from-usage estimate so the
/// response carries a non-zero saved figure rather than silently zeroing out.
#[tokio::test]
async fn l1_legacy_entry_falls_back_to_synthetic_baseline() {
    use tt_cache::L1Cache;
    use tt_shared::messages::{Choice, Message, MessageContent};
    use tt_shared::{ChatCompletionResponse, Usage};

    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1.clone(), None);
    let app = build_router(state);

    // Pre-stuff the cache with a raw ChatCompletionResponse (no envelope) —
    // this is what entries written before the schema bump look like.
    let legacy_response = ChatCompletionResponse {
        id: "chatcmpl-legacy".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "counting-1".into(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text("legacy body".into())),
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
    };
    let legacy_bytes = serde_json::to_vec(&legacy_response).unwrap();

    // The chat handler namespaces L1 keys as "{org_id}:{cache_key(req)}". The
    // test path runs with org_id = Uuid::nil(); reproduce that key here.
    let req = serde_json::from_str::<tt_shared::ChatCompletionRequest>(
        &json!({
            "model": "counting-1",
            "messages": [{ "role": "user", "content": "hello there" }],
            "stream": false,
        })
        .to_string(),
    )
    .unwrap();
    let key = format!("{}:{}", uuid::Uuid::nil(), tt_cache::key::cache_key(&req));
    l1.set(&key, &legacy_bytes, 60).await.unwrap();

    // Request now hits the legacy entry — handler must not panic, must return
    // hit-l1 with a non-zero synthetic baseline, and must NOT call the provider.
    let resp = app
        .oneshot(chat_request("counting-1", false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "legacy entry must still hit"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "legacy hit must not dispatch to provider"
    );
    let baseline: f64 = resp.headers()["x-tokentrimmer-baseline-cost-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    // Synthetic baseline = $1/M × 100 + $2/M × 50 = 0.0002.
    let synthetic_expected = (100.0 * 1.0 + 50.0 * 2.0) / 1_000_000.0;
    assert!(
        (baseline - synthetic_expected).abs() < 1e-9,
        "legacy hit should fall back to synthetic baseline ({synthetic_expected}); got {baseline}"
    );
}

#[tokio::test]
async fn streaming_request_dispatches_when_no_l1_entry() {
    // Stream:true with an empty L1 must dispatch live — the cache is
    // populated only by non-stream requests, so a brand-new prompt has
    // nothing to fake-stream from.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1, None);
    let app = build_router(state);

    let r = app.oneshot(chat_request("counting-1", true)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // Must be SSE (provider live-stream wrapped by sse::stream_response).
    let ct = r.headers()["content-type"].to_str().unwrap();
    assert!(
        ct.contains("text/event-stream"),
        "expected text/event-stream, got {ct}"
    );
}

#[tokio::test]
async fn streaming_request_fake_streams_from_l1_hit() {
    // Populate L1 with a non-stream request, then issue the same prompt
    // with stream:true — the second request should NOT call the provider,
    // and should return an SSE body assembled from the cached response.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1, None);
    let app = build_router(state);

    // Prime cache with stream:false.
    let r1 = app
        .clone()
        .oneshot(chat_request("counting-1", false))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Same prompt with stream:true.
    let r2 = app.oneshot(chat_request("counting-1", true)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    // Provider must NOT have been called again — the body came from L1.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "L1 fake-stream must not dispatch to the provider"
    );

    let ct = r2.headers()["content-type"].to_str().unwrap();
    assert!(
        ct.contains("text/event-stream"),
        "expected text/event-stream, got {ct}"
    );

    // Body should carry SSE-encoded synthetic chunks ending with [DONE],
    // including the cached assistant text "live from provider".
    let bytes = to_bytes(r2.into_body(), 16 * 1024).await.unwrap();
    let body = std::str::from_utf8(&bytes).unwrap();
    assert!(
        body.contains("[DONE]"),
        "SSE body should terminate with [DONE]; got:\n{body}"
    );
    assert!(
        body.contains("live from provider"),
        "SSE body should carry the cached assistant text; got:\n{body}"
    );
    assert!(
        body.contains("\"role\":\"assistant\""),
        "first synthetic chunk should declare role=assistant; got:\n{body}"
    );
    assert!(
        body.contains("\"finish_reason\":\"stop\""),
        "terminator chunk should carry finish_reason=stop; got:\n{body}"
    );
}

// ── Task 2 helper: a provider that reports cached_tokens in its Usage. ───────
//
// The CountingProvider above always returns cached_tokens=0. This one returns
// cached_tokens=50 out of 100 prompt tokens so the cached-token discount
// (chat.rs compute_cost, ~line 1345) kicks in for a live (non-cache-hit)
// request. The provider prices $3/M input, $6/M output, $0.3/M cached — the
// same as CountingProvider so math is comparable.

struct CachedDiscountProvider;

#[async_trait]
impl Provider for CachedDiscountProvider {
    fn id(&self) -> &'static str {
        "cached-discount"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "cached-discount-1".into(),
            provider: "cached-discount".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse {
            id: "chatcmpl-cached-discount".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("with cached discount".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                // 50 of 100 prompt tokens were read from the provider's KV
                // cache — only the remaining 50 are charged at the fresh rate.
                cached_tokens: 50,
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
        Err(ProviderError::Unsupported("no".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// A single (non-L1-hit) request where the *provider* itself reports
/// `cached_tokens > 0` (i.e. its own KV-cache read discount, distinct from the
/// TokenTrimmer L1/L2 cache) and NO TokenTrimmer optimization was applied (no
/// routing, no TT cache hit). The provider's automatic discount must NOT be
/// claimed as TokenTrimmer savings: `saved_usd == 0`, with the discount
/// surfaced separately as `x-tokentrimmer-provider-cache-saved-usd`.
///
/// Pricing (compute_cost, crates/core/src/routes/chat.rs):
///   cache_read   = 50 (cached_tokens), cache_write = 0, fresh_input = 50
///   cost_usd     = 50 × $3/M + 50 × $0.3/M + 50 × $6/M
///                = 0.000150 + 0.000015 + 0.000300 = 0.000465
///   baseline_usd = 100 × $3/M + 50 × $6/M = 0.000300 + 0.000300 = 0.000600
///   provider_cache_saved_usd = 0.000600 − 0.000465 = 0.000135
///   saved_usd    = 0 (nothing TokenTrimmer-attributed)
#[tokio::test]
async fn provider_cache_discount_excluded_from_tt_saved() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CachedDiscountProvider));
    // No L1 cache — we want a live provider dispatch, not a cache hit.
    let app = build_router(AppState::new(registry));

    let body = json!({
        "model": "cached-discount-1",
        "messages": [{ "role": "user", "content": "hello" }],
        "stream": false,
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Must NOT be a cache hit — verify the provider actually ran.
    assert_ne!(
        resp.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "should be a live request, not an L1 hit"
    );

    let header_f64 = |name: &str| -> f64 {
        resp.headers()[name]
            .to_str()
            .unwrap()
            .parse::<f64>()
            .unwrap()
    };
    let cost = header_f64("x-tokentrimmer-cost-usd");
    let baseline = header_f64("x-tokentrimmer-baseline-cost-usd");
    let saved = header_f64("x-tokentrimmer-saved-usd");
    let provider_cache_saved = header_f64("x-tokentrimmer-provider-cache-saved-usd");

    // Pricing derivation (see docstring above):
    //   fresh_input=50, cache_read=50, output=50
    //   $3/M × 50  + $0.3/M × 50  + $6/M × 50
    let expected_cost = (50.0 * 3.0 + 50.0 * 0.3 + 50.0 * 6.0) / 1_000_000.0;
    // Baseline: all 100 prompt tokens at the plain input rate, no cache
    // discount.
    let expected_baseline = (100.0 * 3.0 + 50.0 * 6.0) / 1_000_000.0;
    let expected_provider_saved = expected_baseline - expected_cost;

    assert!(
        (cost - expected_cost).abs() < 1e-9,
        "cost should be {expected_cost} (cached-token discount applied); got {cost}"
    );
    assert!(
        (baseline - expected_baseline).abs() < 1e-9,
        "baseline should be {expected_baseline} (no cache discount); got {baseline}"
    );
    assert!(
        baseline > cost,
        "baseline ({baseline}) should exceed discounted cost ({cost})"
    );
    // No TT optimization happened — TT must not claim the provider's discount.
    assert!(
        saved.abs() < 1e-9,
        "saved must be 0 with no TT optimization (provider discount is not ours); got {saved}"
    );
    assert!(
        (provider_cache_saved - expected_provider_saved).abs() < 1e-9,
        "provider-cache-saved should equal baseline − cost = {expected_provider_saved}; \
         got {provider_cache_saved}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// M1: route-level settle-count tests (P0-1 / P0-3).
//
// The enforcer unit tests in `budget.rs` only exercise `check`/`settle`
// directly — nothing drove a real request through the chat handler to assert
// `settle` fires with the right `cached` flag. That gap is exactly why the
// streaming L1 cache-hit settle (B1) went missing: the fake-stream path passes
// `None` log_ctx to `sse::stream_response`, taking the no-DropGuard branch, so
// the streamed-dispatch settle never ran for a streaming cache hit.
//
// These tests drive a real request through the chat handler and assert the
// (billed, served) counts the handler advanced via `settle`, using the public
// `InMemoryBudgetEnforcer::monthly_counts` accessor.
//
// To get a non-nil `ctx.org_id` (the spend sink no-ops on the nil org) WITHOUT
// paying argon2's deliberately-expensive key verification on every request
// (which is glacial in debug builds), the harness uses the dogfood loopback
// stamp: `with_dogfood_enabled()` makes the auth middleware stamp an anonymous
// (no-bearer) request with the fixed `DOGFOOD_ORG_ID` and run the pre-flight
// budget `check` against the wired `budget` enforcer — no key store, no argon2.
// `with_budget(InMemoryBudgetEnforcer)` makes `spend_sink()` resolve to that
// SAME enforcer (Global), so `settle` lands where `monthly_counts` reads.
// ─────────────────────────────────────────────────────────────────────────────

/// An anonymous (no-bearer) chat request. With dogfood loopback enabled the auth
/// middleware stamps it with `DOGFOOD_ORG_ID` — no key store / argon2 verify.
fn dogfood_chat_request(stream: bool) -> Request<Body> {
    let body = json!({
        "model": "counting-1",
        "messages": [{ "role": "user", "content": "hello there" }],
        "stream": stream,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Build a dogfood-loopback router over a `CountingProvider` + L1 with an
/// injected [`tt_core::InMemoryBudgetEnforcer`] (Free-tier caps, far above what
/// these single-request tests reach). Returns the app, the dogfood org id, the
/// enforcer (for `monthly_counts`), and the provider call counter.
fn dogfood_app_with_l1() -> (
    axum::Router,
    uuid::Uuid,
    Arc<tt_core::InMemoryBudgetEnforcer>,
    Arc<AtomicUsize>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let enforcer = Arc::new(tt_core::InMemoryBudgetEnforcer::new(
        tt_core::budget::BudgetLimits::free_tier(),
    ));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry)
        .with_l1(l1, None)
        .with_dogfood_enabled()
        .with_budget(enforcer.clone() as Arc<dyn tt_core::budget::BudgetEnforcer>);
    let app = build_router(state);
    (app, tt_core::DOGFOOD_ORG_ID, enforcer, calls)
}

/// B1 regression: a STREAMING L1 cache hit must advance `served_request_count`
/// (the COGS guard) but NOT the billed `month_request_count` — and the body must
/// be drained so any settle has run. Before the fix the fake-stream path never
/// settled, so a free `stream:true` tenant served unbounded cache hits.
#[tokio::test]
async fn streaming_l1_hit_advances_served_not_billed() {
    let (app, org, enforcer, calls) = dogfood_app_with_l1();

    // Prime the cache with a non-streaming request (1 dispatch → billed+served).
    let r1 = app
        .clone()
        .oneshot(dogfood_chat_request(false))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    // Drain the non-streaming body and let the spawned L1 insert land.
    let _ = to_bytes(r1.into_body(), 16 * 1024).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(calls.load(Ordering::Relaxed), 1, "prime dispatched once");
    let (billed_after_prime, served_after_prime) = enforcer.monthly_counts(org);
    assert_eq!(
        (billed_after_prime, served_after_prime),
        (1, 1),
        "the priming non-streaming dispatch advances both counters"
    );

    // Same prompt with stream:true — must fake-stream from L1, NOT dispatch.
    let r2 = app.oneshot(dogfood_chat_request(true)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    // Fully drain the SSE body so any DropGuard / inline settle has executed.
    let bytes = to_bytes(r2.into_body(), 16 * 1024).await.unwrap();
    let body = std::str::from_utf8(&bytes).unwrap();
    assert!(body.contains("[DONE]"), "fake-stream should emit [DONE]");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "streaming L1 hit must NOT dispatch to the provider"
    );

    let (billed, served) = enforcer.monthly_counts(org);
    assert_eq!(
        billed, 1,
        "a streaming CACHE HIT must NOT advance the billed counter (stayed at the prime's 1)"
    );
    assert_eq!(
        served, 2,
        "a streaming cache hit MUST advance the served counter (COGS guard): prime + hit"
    );
}

/// A STREAMING provider dispatch (no cache entry) must advance BOTH counters.
/// The settle fires from the SSE DropGuard, so the body must be drained.
#[tokio::test]
async fn streaming_dispatch_advances_both() {
    let (app, org, enforcer, calls) = dogfood_app_with_l1();

    // Brand-new prompt, stream:true, empty L1 → live dispatch.
    let r = app.oneshot(dogfood_chat_request(true)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let ct = r.headers()["content-type"].to_str().unwrap();
    assert!(ct.contains("text/event-stream"), "expected SSE, got {ct}");
    // Drain to fire the DropGuard settle.
    let bytes = to_bytes(r.into_body(), 16 * 1024).await.unwrap();
    assert!(
        std::str::from_utf8(&bytes).unwrap().contains("[DONE]"),
        "streamed dispatch should terminate with [DONE]"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1, "must dispatch once");

    let (billed, served) = enforcer.monthly_counts(org);
    assert_eq!(
        billed, 1,
        "streaming dispatch must advance the billed counter"
    );
    assert_eq!(
        served, 1,
        "streaming dispatch must advance the served counter"
    );
}

/// A NON-streaming L1 cache hit must advance `served` but NOT `billed` —
/// mirrors the streaming case through the `CompletionOutcome::CacheHit` arm.
#[tokio::test]
async fn non_streaming_l1_hit_advances_served_not_billed() {
    let (app, org, enforcer, calls) = dogfood_app_with_l1();

    // First request: dispatch (billed+served = 1,1).
    let r1 = app
        .clone()
        .oneshot(dogfood_chat_request(false))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let _ = to_bytes(r1.into_body(), 16 * 1024).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(enforcer.monthly_counts(org), (1, 1));

    // Second identical request: L1 hit, no dispatch.
    let r2 = app.oneshot(dogfood_chat_request(false)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "second request should hit L1"
    );
    let _ = to_bytes(r2.into_body(), 16 * 1024).await.unwrap();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "non-streaming L1 hit must NOT dispatch"
    );

    let (billed, served) = enforcer.monthly_counts(org);
    assert_eq!(billed, 1, "non-streaming cache hit must NOT advance billed");
    assert_eq!(served, 2, "non-streaming cache hit MUST advance served");
}
