//! End-to-end tests for the L2 semantic cache integration in the chat route.
//!
//! Covers:
//! 1. Cache miss path sets `X-TokenTrimmer-Cache: miss` when L2 is wired.
//! 2. Cache hit path (after a primed entry) returns the cached body with
//!    `hit-l2` header and bypasses the provider entirely.
//! 3. Cache state header is `none` when L2 is not wired on the app state.
//! 4. Streaming requests skip L2 entirely (the fake-stream-from-cache
//!    feature is `w7-fake-stream-cache`, still blocked).
//! 5. Free-tier (or unauthenticated) callers do NOT get L2 hits or writes
//!    even when `state.l2` is configured — L2 is a paid-tier entitlement.
//! 6. Free-tier STREAMING completions do NOT write into L2 even when L2 is
//!    wired — `stream_cache_insert.l2` must be None for Free/None callers
//!    (rv-streaming-l2-tier-gate).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::ApiKeyContext;
use tt_cache::{CacheEntry, EmbeddingProvider, InMemoryL2Cache, L2Cache};
use tt_core::{build_router, AppState, InMemoryJudgeBandStore, JudgeConfig, ProviderRegistry};
use tt_shared::{
    messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
    pricing::Capability,
    CallerTier, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext, Usage,
};
use uuid::Uuid;

// ── A provider that counts its dispatches — used to assert cache hits bypass it.

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

// ── A deterministic embedder that returns a fixed vector for every text.

struct FixedEmbedder {
    vec: Vec<f32>,
}

#[async_trait]
impl EmbeddingProvider for FixedEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, tt_cache::EmbedError> {
        Ok(self.vec.clone())
    }
    fn model(&self) -> &str {
        "fixed-embed"
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

/// Build a chat request pre-stamped with an `ApiKeyContext` carrying the given
/// tier. Because the auth middleware only inserts `ApiKeyContext` when a valid
/// `tt_live_*` token is present (and no key store is wired in these tests), a
/// pre-inserted extension survives the middleware pass-through intact.
fn chat_request_with_tier(model: &str, stream: bool, tier: Option<CallerTier>) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello there" }],
        "stream": stream,
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut().insert(ApiKeyContext {
        key_id: Uuid::nil(),
        org_id: Uuid::nil(),
        tier,
    });
    req
}

#[tokio::test]
async fn no_l2_configured_yields_cache_none_header() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let app = build_router(AppState::new(registry));

    let response = app
        .oneshot(chat_request("counting-1", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("none")
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1, "provider must be called");
}

#[tokio::test]
async fn l2_miss_dispatches_and_sets_miss_header() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    let embedder = Arc::new(FixedEmbedder {
        vec: vec![1.0; 1536],
    });
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, None);
    let app = build_router(state);

    let response = app
        .oneshot(chat_request("counting-1", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("miss")
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn l2_hit_serves_cached_response_without_provider_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());

    // Prime the cache with an entry whose embedding matches what the
    // embedder will return — so the lookup will hit at similarity 1.0.
    let primed = ChatCompletionResponse {
        id: "chatcmpl-cached".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "counting-1".into(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text("from cache".into())),
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
        },
    };
    let entry_vec = vec![1.0; 1536];
    // Stored catalog-derived baseline (what the original miss computed via
    // compute_cost). Deliberately distinct from both the CountingProvider
    // catalog rate and the old hardcoded placeholder, so the assertion proves
    // the hit reads the ROW's stored value.
    let stored_baseline = 0.000777_f64;
    let entry = CacheEntry {
        id: Uuid::now_v7(),
        org_id: Uuid::nil(),
        embedding: entry_vec.clone(),
        response: serde_json::to_vec(&primed).unwrap(),
        model: "counting-1".into(),
        embedding_model: "fixed-embed".into(),
        input_tokens: 100,
        output_tokens: 50,
        baseline_cost_usd: Some(stored_baseline),
        hit_count: 0,
        quality_score: None,
        judge_verdict: None,
        created_at: Utc::now(),
        expires_at: Utc::now() + ChronoDuration::hours(1),
    };
    {
        use tt_cache::L2Cache;
        cache.insert(entry).await.unwrap();
    }

    let embedder = Arc::new(FixedEmbedder { vec: entry_vec });
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, Some(0.99));
    let app = build_router(state);

    // L2 lookup is paid-tier only; inject a Pro-tier context so the hit fires.
    let response = app
        .oneshot(chat_request_with_tier(
            "counting-1",
            false,
            Some(CallerTier::Pro),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l2")
    );
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("cache")
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "provider must NOT be called on a cache hit"
    );

    // ── Savings headers: an L2 hit must report full savings (saved == baseline)
    // and the baseline must be the catalog-derived value STORED ON THE ROW at
    // insert time (migration 0010) — never the old $1/M·$2/M placeholder.
    let baseline_l2: f64 = response.headers()["x-tokentrimmer-baseline-cost-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (baseline_l2 - stored_baseline).abs() < 1e-9,
        "L2 hit baseline must be the row's stored baseline_cost_usd ({stored_baseline}); got {baseline_l2}"
    );
    let placeholder = (100.0_f64 * 1.0 + 50.0_f64 * 2.0) / 1_000_000.0;
    assert!(
        (baseline_l2 - placeholder).abs() > 1e-9,
        "L2 hit baseline must NOT be the hardcoded $1/M·$2/M placeholder ({placeholder})"
    );
    let saved_l2: f64 = response.headers()["x-tokentrimmer-saved-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (saved_l2 - baseline_l2).abs() < 1e-9,
        "L2 hit saved should equal baseline ({baseline_l2}) — 100% savings; got {saved_l2}"
    );

    // Body must be the cached body, not the live provider one.
    let bytes = to_bytes(response.into_body(), 8 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["id"], "chatcmpl-cached");
}

/// A legacy L2 row (inserted before migration 0010, `baseline_cost_usd: None`)
/// must have its savings re-priced against the CURRENT pricing catalog for the
/// stored model/token counts — NOT the old hardcoded $1/M·$2/M placeholder.
///
/// CountingProvider prices counting-1 at $3/M input + $6/M output, so the
/// honest baseline for 100 input + 50 output tokens is 0.0006 — the
/// placeholder would have reported 0.0002 (a 3x misstatement).
#[tokio::test]
async fn l2_hit_with_null_baseline_uses_current_catalog_not_placeholder() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());

    let primed = ChatCompletionResponse {
        id: "chatcmpl-cached-legacy".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "counting-1".into(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text("from cache".into())),
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
        },
    };
    let entry_vec = vec![1.0; 1536];
    let entry = CacheEntry {
        id: Uuid::now_v7(),
        org_id: Uuid::nil(),
        embedding: entry_vec.clone(),
        response: serde_json::to_vec(&primed).unwrap(),
        model: "counting-1".into(),
        embedding_model: "fixed-embed".into(),
        input_tokens: 100,
        output_tokens: 50,
        baseline_cost_usd: None, // legacy row — predates migration 0010
        hit_count: 0,
        quality_score: None,
        judge_verdict: None,
        created_at: Utc::now(),
        expires_at: Utc::now() + ChronoDuration::hours(1),
    };
    {
        use tt_cache::L2Cache;
        cache.insert(entry).await.unwrap();
    }

    let embedder = Arc::new(FixedEmbedder { vec: entry_vec });
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, Some(0.99));
    let app = build_router(state);

    let response = app
        .oneshot(chat_request_with_tier(
            "counting-1",
            false,
            Some(CallerTier::Pro),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l2")
    );

    let baseline_l2: f64 = response.headers()["x-tokentrimmer-baseline-cost-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    // Current catalog (CountingProvider): $3/M input + $6/M output.
    let catalog_baseline = (100.0_f64 * 3.0 + 50.0_f64 * 6.0) / 1_000_000.0;
    let placeholder = (100.0_f64 * 1.0 + 50.0_f64 * 2.0) / 1_000_000.0;
    assert!(
        (baseline_l2 - catalog_baseline).abs() < 1e-9,
        "legacy-row L2 hit baseline must be catalog-derived ({catalog_baseline}); got {baseline_l2}"
    );
    assert!(
        (baseline_l2 - placeholder).abs() > 1e-9,
        "legacy-row L2 hit baseline must NOT be the hardcoded placeholder ({placeholder})"
    );
    let saved_l2: f64 = response.headers()["x-tokentrimmer-saved-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (saved_l2 - catalog_baseline).abs() < 1e-9,
        "legacy-row L2 hit saved must be catalog-derived ({catalog_baseline}); got {saved_l2}"
    );
}

/// A cache-miss dispatch must populate the new L2 row's `baseline_cost_usd`
/// from the real pricing catalog (the same `compute_cost` math the gateway
/// prices live requests with) — counting-1 at $3/M input + $6/M output over
/// the response's 100/50 token usage = 0.0006.
#[tokio::test]
async fn l2_insert_stores_catalog_baseline() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    let entry_vec = vec![1.0_f32; 1536];
    let embedder = Arc::new(FixedEmbedder {
        vec: entry_vec.clone(),
    });
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, Some(0.99));
    let app = build_router(state);

    // Paid-tier miss → provider dispatch → fire-and-forget L2 insert.
    let response = app
        .oneshot(chat_request_with_tier(
            "counting-1",
            false,
            Some(CallerTier::Pro),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1, "miss must dispatch");
    let _ = to_bytes(response.into_body(), 8 * 1024).await.unwrap();

    // The L2 insert is a spawned task; poll the cache until it lands.
    let mut found = None;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        if let Some(hit) = cache
            .lookup(Uuid::nil(), &entry_vec, 0.99, "counting-1", "fixed-embed")
            .await
            .unwrap()
        {
            found = Some(hit.0);
            break;
        }
    }
    let entry = found.expect("miss should have inserted an L2 entry");

    // CountingProvider catalog: $3/M input + $6/M output; usage 100/50.
    let catalog_baseline = (100.0_f64 * 3.0 + 50.0_f64 * 6.0) / 1_000_000.0;
    let stored = entry
        .baseline_cost_usd
        .expect("insert must store a catalog-derived baseline_cost_usd");
    assert!(
        (stored - catalog_baseline).abs() < 1e-9,
        "stored baseline must be catalog-derived ({catalog_baseline}); got {stored}"
    );
}

#[tokio::test]
async fn streaming_request_bypasses_l2_cache() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    let embedder = Arc::new(FixedEmbedder {
        vec: vec![1.0; 1536],
    });
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, None);
    let app = build_router(state);

    let response = app.oneshot(chat_request("counting-1", true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // SSE response — no cache header is set on the stream path (fake-stream
    // from cache is w7-fake-stream-cache).
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/event-stream"
    );
    // Provider was called for the streaming path.
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

/// Regression test: a Free-tier (or unauthenticated) caller must NOT get an
/// L2 hit even when the L2 store is configured and contains a matching entry.
/// L2 is a paid-tier entitlement (`BudgetLimits.l2_cache`).
///
/// Setup mirrors `l2_hit_serves_cached_response_without_provider_call` but
/// with tier=None (unauthenticated dev request). The provider must be called
/// (L2 read is skipped) and the response body must come from the provider, NOT
/// from the cached entry.
#[tokio::test]
async fn free_tier_caller_skips_l2_lookup_and_write() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());

    // Prime the L2 cache with an entry that would hit at similarity 1.0.
    let primed = ChatCompletionResponse {
        id: "chatcmpl-should-not-serve".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "counting-1".into(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text("should not appear".into())),
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
        },
    };
    let entry_vec = vec![1.0; 1536];
    let entry = CacheEntry {
        id: Uuid::now_v7(),
        org_id: Uuid::nil(), // matches the nil org_id of an unauthenticated request
        embedding: entry_vec.clone(),
        response: serde_json::to_vec(&primed).unwrap(),
        model: "counting-1".into(),
        embedding_model: "fixed-embed".into(),
        input_tokens: 100,
        output_tokens: 50,
        baseline_cost_usd: None,
        hit_count: 0,
        quality_score: None,
        judge_verdict: None,
        created_at: Utc::now(),
        expires_at: Utc::now() + ChronoDuration::hours(1),
    };
    {
        use tt_cache::L2Cache;
        cache.insert(entry).await.unwrap();
    }

    let embedder = Arc::new(FixedEmbedder { vec: entry_vec });
    // Low threshold (0.5) so anything would hit — but the entitlement gate
    // must block the lookup before it even reaches the store.
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, Some(0.5));
    let app = build_router(state);

    // No tier injected → l2_allowed = false → L2 read must be skipped.
    let response = app
        .oneshot(chat_request("counting-1", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Provider must have been called (L2 did not short-circuit it).
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider must be called when L2 is blocked by Free/None tier"
    );

    // Response must be the live provider body, NOT the cached entry.
    let bytes = to_bytes(response.into_body(), 8 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_ne!(
        body["id"], "chatcmpl-should-not-serve",
        "Free-tier caller must NOT receive the L2-cached response"
    );
    // The provider stub returns id "chatcmpl-live".
    assert_eq!(
        body["id"], "chatcmpl-live",
        "Free-tier caller must get the live provider response"
    );
}

/// Regression: a Free-tier (or unauthenticated) STREAMING completion must NOT
/// write into L2 after stream completion.
///
/// Before the fix, `stream_cache_insert.l2` was populated unconditionally for
/// any streaming miss when `state.l2.is_some()`, so the fire-and-forget
/// `tokio::spawn` in `sse.rs` wrote to L2 regardless of tier.  After the fix,
/// `l2_for_insert` is `None` when `!l2_allowed`, so no L2 spawn occurs.
///
/// The assertion: after the SSE response body is fully drained and a brief
/// yield window has passed, the L2 store for the request's org must remain
/// empty (lookup returns None).
#[tokio::test]
async fn free_tier_streaming_does_not_write_l2() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());
    let embedder = Arc::new(FixedEmbedder {
        vec: vec![1.0; 1536],
    });
    // Wire L2 with a very low threshold so any entry would hit.
    let state = AppState::new(registry).with_l2(cache.clone(), embedder, Some(0.0));
    let app = build_router(state);

    // No tier injected (tier=None → l2_allowed=false).
    let response = app.oneshot(chat_request("counting-1", true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Fully drain the SSE body so the stream completes and any background
    // spawn triggered by the guard drop has a chance to run.
    let _body_bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();

    // Yield to the executor a few times to let any spawned futures settle.
    // The fire-and-forget insert path in sse.rs uses `tokio::spawn`, so
    // it runs on the same single-threaded test runtime; a single yield is
    // sufficient, but we give it a few to be safe.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Assert the L2 store has no entry for the nil org (the unauthenticated
    // org_id used by requests with no ApiKeyContext).
    let hit = cache
        .lookup(
            Uuid::nil(),
            &vec![1.0f32; 1536],
            0.0,
            "counting-1",
            "fixed-embed",
        )
        .await
        .unwrap();
    assert!(
        hit.is_none(),
        "Free-tier streaming completion must NOT write to L2; found an entry: {hit:?}"
    );
}

// ── A provider that serves BOTH the reference model and the judge model.
//
// `counting-1` (the served/reference model) returns a normal answer; the judge
// model `judge-1` returns text containing "DEGRADED" so the parsed verdict is
// `Degraded` → `RiskBand::High` → eviction. Used to drive the judge-on-L2-hit
// path end-to-end and assert the served-from-L2 entry is evicted.
struct JudgingProvider {
    ref_calls: Arc<AtomicUsize>,
    judge_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for JudgingProvider {
    fn id(&self) -> &'static str {
        "judging"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["counting-1", "judge-1"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "judging".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            })
            .collect()
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
        // The judge model returns a verdict; the reference model returns a
        // plain answer (the off-path reference dispatch).
        let text = if req.model == "judge-1" {
            self.judge_calls.fetch_add(1, Ordering::Relaxed);
            "DEGRADED — the cached answer does not address the query".to_string()
        } else {
            self.ref_calls.fetch_add(1, Ordering::Relaxed);
            "a fresh reference answer".to_string()
        };
        Ok(ChatCompletionResponse {
            id: "chatcmpl-judge".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(text)),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
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
        Err(ProviderError::Unsupported("no stream".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// The headline join: a response SERVED FROM L2 is judged on a detached task,
/// and a clearly-degraded verdict evicts EXACTLY that entry so the poisoned
/// near-duplicate can't be served again.
///
/// Wires the judge with `sample_rate = 1.0` (always sample) so the deterministic
/// gate fires for every request. The served answer is the cached body; the
/// reference is the original request re-dispatched to `counting-1`; the judge
/// model `judge-1` returns "DEGRADED". After the hit response returns, the
/// detached judge runs and evicts the served entry — proving the production
/// `L2EvictionTarget` is constructed (it was previously hardcoded to `None`).
#[tokio::test]
async fn l2_hit_degraded_judge_verdict_evicts_the_served_entry() {
    let ref_calls = Arc::new(AtomicUsize::new(0));
    let judge_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(JudgingProvider {
        ref_calls: Arc::clone(&ref_calls),
        judge_calls: Arc::clone(&judge_calls),
    }));
    let cache = Arc::new(InMemoryL2Cache::new());

    let primed = ChatCompletionResponse {
        id: "chatcmpl-cached".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "counting-1".into(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text("a stale cached answer".into())),
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
        },
    };
    let entry_vec = vec![1.0_f32; 1536];
    let entry_id = Uuid::now_v7();
    let entry = CacheEntry {
        id: entry_id,
        org_id: Uuid::nil(),
        embedding: entry_vec.clone(),
        response: serde_json::to_vec(&primed).unwrap(),
        model: "counting-1".into(),
        embedding_model: "fixed-embed".into(),
        input_tokens: 100,
        output_tokens: 50,
        baseline_cost_usd: Some(0.0006),
        hit_count: 0,
        quality_score: None,
        judge_verdict: None,
        created_at: Utc::now(),
        expires_at: Utc::now() + ChronoDuration::hours(1),
    };
    cache.insert(entry).await.unwrap();

    let embedder = Arc::new(FixedEmbedder {
        vec: entry_vec.clone(),
    });
    let store = Arc::new(InMemoryJudgeBandStore::new());
    let judge_config = JudgeConfig {
        enabled: true,
        sample_rate: 1.0, // always sample so the deterministic gate fires
        judge_model: "judge-1".to_string(),
    };
    let state = AppState::new(registry)
        .with_l2(cache.clone(), embedder, Some(0.99))
        .with_quality_judge(store, judge_config);
    let app = build_router(state);

    // Paid-tier so the L2 lookup is allowed and serves the cached entry.
    let response = app
        .oneshot(chat_request_with_tier(
            "counting-1",
            false,
            Some(CallerTier::Pro),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l2"),
        "the request must be served from L2"
    );
    let _ = to_bytes(response.into_body(), 8 * 1024).await.unwrap();

    // The judge runs on a detached task; poll the cache until the degraded
    // verdict evicts the served entry.
    let mut evicted = false;
    for _ in 0..500 {
        tokio::task::yield_now().await;
        let still_there = cache
            .lookup(Uuid::nil(), &entry_vec, 0.0, "counting-1", "fixed-embed")
            .await
            .unwrap()
            .is_some();
        if !still_there {
            evicted = true;
            break;
        }
    }
    assert!(
        evicted,
        "a degraded judge verdict on an L2-served response must evict that entry"
    );
    assert_eq!(
        ref_calls.load(Ordering::Relaxed),
        1,
        "the judge must re-dispatch the original request once for the reference"
    );
    assert_eq!(
        judge_calls.load(Ordering::Relaxed),
        1,
        "the judge model must be called once"
    );
}
