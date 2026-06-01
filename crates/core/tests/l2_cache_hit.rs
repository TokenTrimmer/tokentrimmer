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
use tt_cache::{CacheEntry, EmbeddingProvider, InMemoryL2Cache};
use tt_core::{build_router, AppState, ProviderRegistry};
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
                },
                finish_reason: Some("stop".into()),
            }],
            usage: None,
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
    let entry = CacheEntry {
        id: Uuid::now_v7(),
        org_id: Uuid::nil(),
        embedding: entry_vec.clone(),
        response: serde_json::to_vec(&primed).unwrap(),
        model: "counting-1".into(),
        embedding_model: "fixed-embed".into(),
        input_tokens: 100,
        output_tokens: 50,
        hit_count: 0,
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
        .oneshot(chat_request_with_tier("counting-1", false, Some(CallerTier::Pro)))
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

    // ── Savings headers: an L2 hit must report full savings (saved == baseline).
    //
    // `build_hit_l2_response` calls `synthetic_baseline_from_entry` which
    // prices at $1/M input + $2/M output (the conservative default until the
    // L2 schema carries its own baseline_cost_usd column).
    //
    // The primed entry above has input_tokens=100, output_tokens=50 so:
    //   baseline = (100 × $1 + 50 × $2) / 1_000_000 = 0.0002
    //   saved    = baseline  (cost is zero — no provider call on a hit)
    let baseline_l2: f64 = response
        .headers()["x-tokentrimmer-baseline-cost-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let expected_baseline = (100.0_f64 * 1.0 + 50.0_f64 * 2.0) / 1_000_000.0;
    assert!(
        baseline_l2 > 0.0,
        "L2 hit must report a positive baseline; got {baseline_l2}"
    );
    assert!(
        (baseline_l2 - expected_baseline).abs() < 1e-9,
        "L2 hit baseline should be synthetic $1/M·$2/M estimate ({expected_baseline}); got {baseline_l2}"
    );
    let saved_l2: f64 = response
        .headers()["x-tokentrimmer-saved-usd"]
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
        hit_count: 0,
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
