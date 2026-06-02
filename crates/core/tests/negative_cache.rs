//! End-to-end tests for negative caching of deterministic 4xx errors
//! (rv-cache-key-canonicalization §2.20).
//!
//! Covers:
//! (a) A deterministic 400/InvalidRequest is negative-cached; a repeat
//!     identical request is served from the negative cache WITHOUT a second
//!     provider call.
//! (b) A 429/RateLimited error is NEVER negative-cached; a repeat request
//!     still dispatches to the provider.
//! (c) A 5xx/server error is NEVER negative-cached; a repeat still calls
//!     the provider.
//! (d) Bypass mode (`tt_extras.cache.mode = "bypass"`) skips negative caching
//!     entirely — every request dispatches to the provider.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

// ---------------------------------------------------------------------------
// Provider stubs
// ---------------------------------------------------------------------------

/// Always returns `ProviderError::InvalidRequest` (deterministic 400-like error).
struct InvalidRequestProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for InvalidRequestProvider {
    fn id(&self) -> &'static str {
        "bad"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "bad-model".into(),
            provider: "bad".into(),
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
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ProviderError::InvalidRequest(
            "prompt violates content policy".into(),
        ))
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ProviderError::InvalidRequest("stream not supported".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// Always returns `ProviderError::RateLimited` (transient 429 — must NOT be cached).
struct RateLimitedProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for RateLimitedProvider {
    fn id(&self) -> &'static str {
        "ratelimited"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "rl-model".into(),
            provider: "ratelimited".into(),
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
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ProviderError::RateLimited {
            retry_after_ms: 1000,
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ProviderError::RateLimited {
            retry_after_ms: 1000,
        })
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// Always returns a 500 upstream server error (must NOT be negative-cached).
struct ServerErrorProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for ServerErrorProvider {
    fn id(&self) -> &'static str {
        "server-error"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "se-model".into(),
            provider: "server-error".into(),
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
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ProviderError::ProviderUpstream {
            status: 500,
            message: "internal server error".into(),
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(ProviderError::ProviderUpstream {
            status: 500,
            message: "internal server error".into(),
        })
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
// Helper: build a deterministic (temperature=0) chat POST request body.
// Negative caching only engages when the request is cache-eligible.
// ---------------------------------------------------------------------------

fn det_chat_request(model: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "repeat me" }],
        "temperature": 0,
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn bypass_chat_request(model: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "repeat me" }],
        "temperature": 0,
        "stream": false,
        "tt_extras": { "cache": { "mode": "bypass" } }
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// (a) Deterministic 400 is negative-cached; repeat is served from neg cache
//     without a second provider call.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deterministic_invalid_request_is_negative_cached() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(InvalidRequestProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1, None);
    let app = build_router(state);

    // First request — hits provider, receives error.
    let r1 = app
        .clone()
        .oneshot(det_chat_request("bad-model"))
        .await
        .unwrap();
    let status1 = r1.status();
    // With_retry will retry the error if retriable; InvalidRequest is not retriable.
    // The handler returns a 4xx.
    assert!(
        status1.is_client_error(),
        "first request should return a client error; got {status1}"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1, "provider called once");

    // Allow the spawned negative-cache insert to land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second request — MUST be served from negative cache, NOT call provider.
    let r2 = app.oneshot(det_chat_request("bad-model")).await.unwrap();
    let status2 = r2.status();
    assert!(
        status2.is_client_error(),
        "negative-cache hit should still return a client error; got {status2}"
    );

    // The cache header should be "neg-hit" to indicate negative cache hit.
    let cache_header = r2
        .headers()
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        cache_header,
        Some("neg-hit"),
        "second request should be served from negative cache; got {cache_header:?}"
    );

    // Provider call count must be exactly 1 — the second request was short-circuited.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider must NOT be called on negative-cache hit"
    );
}

// ---------------------------------------------------------------------------
// (b) 429/RateLimited is NEVER negative-cached — repeat still calls provider.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_error_is_never_negative_cached() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RateLimitedProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1, None);
    let app = build_router(state);

    // First request.
    let r1 = app
        .clone()
        .oneshot(det_chat_request("rl-model"))
        .await
        .unwrap();
    let status1 = r1.status();
    // RateLimited -> 429 from the handler.
    assert_eq!(
        status1,
        StatusCode::TOO_MANY_REQUESTS,
        "first request should return 429; got {status1}"
    );
    // with_retry will have attempted up to max_attempts on RateLimited.
    let calls_after_first = calls.load(Ordering::Relaxed);
    assert!(
        calls_after_first >= 1,
        "provider must have been called at least once"
    );

    // Allow any async tasks to settle.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second request — must NOT be served from any cache; provider must be called again.
    let r2 = app.oneshot(det_chat_request("rl-model")).await.unwrap();
    let status2 = r2.status();
    assert_eq!(
        status2,
        StatusCode::TOO_MANY_REQUESTS,
        "second request should still return 429; got {status2}"
    );

    // Cache header must NOT be "neg-hit".
    let cache_header = r2
        .headers()
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok());
    assert_ne!(
        cache_header,
        Some("neg-hit"),
        "rate-limited error must not be served from negative cache"
    );

    let calls_after_second = calls.load(Ordering::Relaxed);
    assert!(
        calls_after_second > calls_after_first,
        "provider must be called again on repeat 429 (not short-circuited); \
         calls_after_first={calls_after_first}, calls_after_second={calls_after_second}"
    );
}

// ---------------------------------------------------------------------------
// (c) 5xx server error is NEVER negative-cached — repeat still calls provider.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_error_is_never_negative_cached() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(ServerErrorProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1, None);
    let app = build_router(state);

    // First request.
    let r1 = app
        .clone()
        .oneshot(det_chat_request("se-model"))
        .await
        .unwrap();
    assert!(
        r1.status().is_server_error(),
        "first request should return 5xx; got {}",
        r1.status()
    );
    let calls_after_first = calls.load(Ordering::Relaxed);
    assert!(
        calls_after_first >= 1,
        "provider must be called at least once"
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second request — must NOT be short-circuited from negative cache.
    let r2 = app.oneshot(det_chat_request("se-model")).await.unwrap();
    assert!(
        r2.status().is_server_error(),
        "second request should still return 5xx; got {}",
        r2.status()
    );

    let cache_header = r2
        .headers()
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok());
    assert_ne!(
        cache_header,
        Some("neg-hit"),
        "5xx server error must not be served from negative cache"
    );

    let calls_after_second = calls.load(Ordering::Relaxed);
    assert!(
        calls_after_second > calls_after_first,
        "provider must be called again on repeat 5xx; \
         calls_after_first={calls_after_first}, calls_after_second={calls_after_second}"
    );
}

// ---------------------------------------------------------------------------
// (d) Bypass mode skips negative caching — every request dispatches to provider.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bypass_mode_skips_negative_caching() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(InvalidRequestProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1, None);
    let app = build_router(state);

    // First request WITH bypass mode.
    let r1 = app
        .clone()
        .oneshot(bypass_chat_request("bad-model"))
        .await
        .unwrap();
    assert!(
        r1.status().is_client_error(),
        "first bypass request should return client error; got {}",
        r1.status()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second request WITH bypass mode — must call provider again; NOT served
    // from negative cache.
    let r2 = app.oneshot(bypass_chat_request("bad-model")).await.unwrap();
    assert!(
        r2.status().is_client_error(),
        "second bypass request should return client error; got {}",
        r2.status()
    );

    let cache_header = r2
        .headers()
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok());
    assert_ne!(
        cache_header,
        Some("neg-hit"),
        "bypass mode must not produce a negative-cache hit"
    );

    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "provider must be called on each bypass request, not short-circuited; \
         got {} calls",
        calls.load(Ordering::Relaxed)
    );
}

// ---------------------------------------------------------------------------
// Negative-cache does not interfere with positive-cache happy path.
// ---------------------------------------------------------------------------

/// A successful response followed by a second request must hit the positive L1
/// cache (not negative cache) and no provider call should happen on the second
/// request.
#[tokio::test]
async fn positive_cache_unaffected_by_negative_cache_logic() {
    struct SuccessProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for SuccessProvider {
        fn id(&self) -> &'static str {
            "success"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "ok-model".into(),
                provider: "success".into(),
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
                id: "ok-id".into(),
                object: "chat.completion".into(),
                created: 0,
                model: req.model,
                choices: vec![Choice {
                    index: 0,
                    message: Message::Assistant {
                        content: Some(MessageContent::Text("great success".into())),
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
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
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

    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(SuccessProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1, None);
    let app = build_router(state);

    let body = json!({
        "model": "ok-model",
        "messages": [{ "role": "user", "content": "repeat me" }],
        "temperature": 0,
        "stream": false,
    });
    let make_req = || {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // First request — provider called, positive cache populated.
    let r1 = app.clone().oneshot(make_req()).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second request — positive L1 hit, provider NOT called.
    let r2 = app.oneshot(make_req()).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "second request should hit positive L1"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider must not be called on positive L1 hit"
    );
}
