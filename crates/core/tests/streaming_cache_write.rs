//! Tests for §rv-l2-streaming-cache-write: a live streaming miss that
//! completes cleanly populates L1 so a subsequent identical streaming request
//! gets a fake-stream hit.
//!
//! Four scenarios:
//!
//! (a) Clean streaming miss → L1 populated → subsequent request is an L1 hit
//!     (provider NOT called a second time).
//! (b) Truncated stream (client abort / no finish_reason + no usage) → L1 NOT
//!     populated → provider called again on the follow-up request.
//! (c) Ineligible stream (temperature > 0) → L1 NOT populated.
//! (d) Existing L1-hit fake-stream path still works and does NOT re-insert.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{ChunkChoice, ChunkDelta},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

// ─── helpers ─────────────────────────────────────────────────────────────────

fn stream_request(model: &str) -> Request<Body> {
    stream_request_with(model, json!({}))
}

fn stream_request_with(model: &str, extra: serde_json::Value) -> Request<Body> {
    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello streaming cache" }],
        "stream": true,
    });
    // Merge in any extras (e.g. temperature).
    if let (Some(obj), Some(ext)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in ext {
            obj.insert(k.clone(), v.clone());
        }
    }
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn non_stream_request(model: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello streaming cache" }],
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Drain the response body and return the body text.
async fn drain(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

// ─── StreamingCountingProvider ────────────────────────────────────────────────

/// Provider that emits a clean 3-chunk stream (role, content, finish+usage).
struct CleanStreamProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CleanStreamProvider {
    fn id(&self) -> &'static str {
        "streaming-clean"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "sc-model".into(),
            provider: "streaming-clean".into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
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
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "chatcmpl-nonstream".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![tt_shared::messages::Choice {
                index: 0,
                message: tt_shared::messages::Message::Assistant {
                    content: Some(tt_shared::messages::MessageContent::Text(
                        "non-stream response".into(),
                    )),
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Emit: role chunk, content chunk, finish+usage chunk.
        let chunks = vec![
            Ok(ChatCompletionChunk {
                id: "stream-id".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: "sc-model".into(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: Some("assistant".into()),
                        content: None,
                        tool_calls: vec![],
                    },
                    finish_reason: None,
                }],
                usage: None,
            }),
            Ok(ChatCompletionChunk {
                id: "stream-id".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: "sc-model".into(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: None,
                        content: Some("hello from stream".into()),
                        tool_calls: vec![],
                    },
                    finish_reason: None,
                }],
                usage: None,
            }),
            Ok(ChatCompletionChunk {
                id: "stream-id".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: "sc-model".into(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta::default(),
                    finish_reason: Some("stop".into()),
                }],
                usage: Some(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    total_tokens: 14,
                    cached_tokens: 0,
                    cache_creation_input_tokens: None,
                }),
            }),
        ];
        Ok(futures::stream::iter(chunks).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

/// Provider that emits NO finish_reason and NO usage (simulates truncated stream).
struct TruncatedStreamProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for TruncatedStreamProvider {
    fn id(&self) -> &'static str {
        "streaming-truncated"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "trunc-model".into(),
            provider: "streaming-truncated".into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        None
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("n/a".into()))
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Only content chunk — no finish_reason, no usage.
        let chunks = vec![Ok(ChatCompletionChunk {
            id: "trunc".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: "trunc-model".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: Some("partial".into()),
                    tool_calls: vec![],
                },
                finish_reason: None, // no finish_reason → truncated
            }],
            usage: None, // no authoritative usage
        })];
        Ok(futures::stream::iter(chunks).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

// ─── (a) Clean stream miss → L1 populated → subsequent request is cache hit ───

#[tokio::test]
async fn streaming_miss_populates_l1_and_subsequent_stream_is_fake_stream_hit() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CleanStreamProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1.clone(), None);
    let app = build_router(state);

    // First streaming request — live dispatch, clean completion.
    let r1 = app
        .clone()
        .oneshot(stream_request("sc-model"))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "first request should dispatch to provider"
    );

    // Drain the body to ensure the DropGuard fires and L1 insert completes.
    let body1 = drain(r1).await;
    assert!(
        body1.contains("[DONE]"),
        "first stream should complete with [DONE]"
    );

    // Give the spawned L1 insert time to complete.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Second streaming request with same prompt — should hit L1 fake-stream.
    let r2 = app.oneshot(stream_request("sc-model")).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);

    // Provider must NOT have been called again.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "second streaming request should be served from L1 cache, not from provider"
    );

    // Must be SSE.
    let ct = r2.headers()["content-type"].to_str().unwrap();
    assert!(
        ct.contains("text/event-stream"),
        "expected text/event-stream, got {ct}"
    );

    // Body should contain the cached content.
    let body2 = drain(r2).await;
    assert!(
        body2.contains("[DONE]"),
        "L1 fake-stream should end with [DONE]"
    );
    assert!(
        body2.contains("hello from stream"),
        "L1 fake-stream should carry the cached assistant text; got:\n{body2}"
    );
}

// ─── (b) Truncated stream → L1 NOT populated → provider called again ─────────

#[tokio::test]
async fn truncated_stream_does_not_populate_l1() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(TruncatedStreamProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1.clone(), None);
    let app = build_router(state);

    // First streaming request — truncated (no finish_reason/usage).
    let r1 = app
        .clone()
        .oneshot(stream_request("trunc-model"))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let _body1 = drain(r1).await; // drain to fire DropGuard

    // Brief pause to confirm no L1 insert was spawned.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Second request — should dispatch live again (not from L1).
    let r2 = app.oneshot(stream_request("trunc-model")).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let _body2 = drain(r2).await;

    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "truncated stream must NOT populate L1; provider should be called twice"
    );
}

// ─── (c) Ineligible stream (temperature > 0) → L1 NOT populated ──────────────

#[tokio::test]
async fn ineligible_stream_temperature_does_not_populate_l1() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CleanStreamProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1.clone(), None);
    let app = build_router(state);

    // Request with temperature=0.7 → not cache-eligible.
    let r1 = app
        .clone()
        .oneshot(stream_request_with("sc-model", json!({"temperature": 0.7})))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let _body1 = drain(r1).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Second identical request → should also dispatch live (no L1 hit).
    let r2 = app
        .oneshot(stream_request_with("sc-model", json!({"temperature": 0.7})))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let _body2 = drain(r2).await;

    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "ineligible (temperature>0) stream must NOT populate L1; provider called twice"
    );
}

// ─── (d) L1-hit fake-stream path still works and doesn't re-insert ────────────

#[tokio::test]
async fn fake_stream_from_l1_hit_does_not_re_insert_or_dispatch() {
    // Populate L1 via a non-stream request, then issue two streaming requests.
    // The first streaming request should hit L1 (fake-stream). The second one
    // should also hit L1. Provider should only be called once (the non-stream seeder).
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CleanStreamProvider {
        calls: Arc::clone(&calls),
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1.clone(), None);
    let app = build_router(state);

    // Seed via non-stream request.
    let seed = app
        .clone()
        .oneshot(non_stream_request("sc-model"))
        .await
        .unwrap();
    assert_eq!(seed.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // First stream request — hits L1, fake-streams.
    let r1 = app
        .clone()
        .oneshot(stream_request("sc-model"))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let body1 = drain(r1).await;
    assert!(body1.contains("[DONE]"));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "streaming L1 hit must not dispatch to provider"
    );

    // Wait to see if a spurious second insert happens.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Second stream request — still hits L1.
    let r2 = app.oneshot(stream_request("sc-model")).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let body2 = drain(r2).await;
    assert!(body2.contains("[DONE]"));

    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider must never be called after L1 is populated (non-stream seeded)"
    );
}
