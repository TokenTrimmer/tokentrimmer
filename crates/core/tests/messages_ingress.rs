//! GW-MESSAGES: the hosted gateway exposes a POST /v1/messages ingress that
//! accepts the Anthropic Messages API request shape, runs it through the SAME
//! cost / routing / cache / credential pipeline as /v1/chat/completions, and
//! returns the Anthropic Messages response shape (Anthropic SSE when streaming).
//!
//! These are hermetic: a mock provider stands in for the upstream, so no network
//! is required. They assert the round-trip wire shape, the x-tokentrimmer-* cost
//! headers, the Anthropic SSE event frames, and the #119 BYO-only credential
//! guard (verified org without an anthropic credential → missing_provider_credential).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, InMemoryProviderCredentialStore, KeyStore, ProviderCredentialStore,
};
use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{
        Choice, ChunkChoice, ChunkDelta, Message, MessageContent, ToolCall, ToolCallFunction,
    },
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// A mock "anthropic" provider serving `claude-sonnet-4-6`. Records the last
/// canonical request it received so tests can assert the inbound translation,
/// and the credential it saw.
struct AnthropicMock {
    seen_request: Arc<Mutex<Option<ChatCompletionRequest>>>,
    seen_key: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl Provider for AnthropicMock {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 200_000,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        *self.seen_key.lock().unwrap() = Some(ctx.credentials.api_key.expose().to_string());
        *self.seen_request.lock().unwrap() = Some(req.clone());
        Ok(ChatCompletionResponse {
            id: "msg_mock_1".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("Hello from Claude!".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        *self.seen_key.lock().unwrap() = Some(ctx.credentials.api_key.expose().to_string());
        *self.seen_request.lock().unwrap() = Some(req.clone());
        let final_usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let chunks = if req.tools.is_empty() {
            vec![
                ChatCompletionChunk {
                    id: "msg_stream_1".into(),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: req.model.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: Some("assistant".into()),
                            content: None,
                            tool_calls: vec![],
                            extra: Default::default(),
                        },
                        finish_reason: None,
                        extra: Default::default(),
                    }],
                    usage: None,
                    extra: Default::default(),
                },
                ChatCompletionChunk {
                    id: "msg_stream_1".into(),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: req.model.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: None,
                            content: Some("Hello!".into()),
                            tool_calls: vec![],
                            extra: Default::default(),
                        },
                        finish_reason: None,
                        extra: Default::default(),
                    }],
                    usage: None,
                    extra: Default::default(),
                },
                ChatCompletionChunk {
                    id: "msg_stream_1".into(),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: req.model,
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta::default(),
                        finish_reason: Some("stop".into()),
                        extra: Default::default(),
                    }],
                    usage: Some(final_usage),
                    extra: Default::default(),
                },
            ]
        } else {
            // Tools present → stream a tool call (as the anthropic adapter does:
            // cumulative `arguments` snapshots per chunk, then a `tool_calls`
            // finish_reason). Exercises the encoder's tool_use block emission.
            let tool_chunk = |args: &str| ChatCompletionChunk {
                id: "msg_stream_tc".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: req.model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: None,
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "toolu_stream_1".into(),
                            r#type: "function".into(),
                            function: ToolCallFunction {
                                name: "get_weather".into(),
                                arguments: args.into(),
                            },
                        }],
                        extra: Default::default(),
                    },
                    finish_reason: None,
                    extra: Default::default(),
                }],
                usage: None,
                extra: Default::default(),
            };
            vec![
                tool_chunk("{\"city\""),
                tool_chunk("{\"city\":\"SF\"}"),
                ChatCompletionChunk {
                    id: "msg_stream_tc".into(),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: req.model,
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta::default(),
                        finish_reason: Some("tool_calls".into()),
                        extra: Default::default(),
                    }],
                    usage: Some(final_usage),
                    extra: Default::default(),
                },
            ]
        };
        Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

fn creds(key: &str) -> ProviderCredentials {
    ProviderCredentials {
        api_key: SecretString::new(key.to_string()),
        base_url: None,
        extra_headers: Vec::new(),
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "k", Environment::Live, Actor::System)
        .await
        .expect("issue tt_live_ key")
        .plaintext
}

fn messages_request(stream: bool) -> serde_json::Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": "You are a helpful assistant.",
        "messages": [{"role": "user", "content": "Hi"}],
        "stream": stream,
    })
}

/// An Anthropic Messages request that declares a tool, so the mock streams a
/// tool call (exercises the encoder's `tool_use` content-block emission).
fn messages_tool_request(stream: bool) -> serde_json::Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "What's the weather in SF?"}],
        "tools": [{
            "name": "get_weather",
            "description": "Get the weather for a city",
            "input_schema": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
            },
        }],
        "stream": stream,
    })
}

/// A built test harness: the router, an issued TokenTrimmer key, and the handles
/// recording what the mock provider observed.
struct Harness {
    app: axum::Router,
    key: String,
    seen_request: Arc<Mutex<Option<ChatCompletionRequest>>>,
    seen_key: Arc<Mutex<Option<String>>>,
}

/// Build a harness with an anthropic mock + key/credential stores.
async fn build(org: Uuid, seed_anthropic_cred: bool) -> Harness {
    let seen_request = Arc::new(Mutex::new(None));
    let seen_key = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(AnthropicMock {
        seen_request: Arc::clone(&seen_request),
        seen_key: Arc::clone(&seen_key),
    }));

    let key_store = InMemoryKeyStore::new();
    let cred_store = InMemoryProviderCredentialStore::new();
    if seed_anthropic_cred {
        cred_store.insert(org, "anthropic", creds("ANT_KEY"));
    }
    let key = issue_key_for(&key_store, org).await;

    let app = build_router(
        AppState::new(registry)
            .with_key_store(Arc::new(key_store) as Arc<dyn KeyStore>)
            .with_credential_store(Arc::new(cred_store) as Arc<dyn ProviderCredentialStore>),
    );
    Harness {
        app,
        key,
        seen_request,
        seen_key,
    }
}

#[tokio::test]
async fn messages_non_streaming_returns_anthropic_shape_with_cost_headers() {
    let org = Uuid::now_v7();
    let Harness {
        app,
        key,
        seen_request,
        seen_key,
    } = build(org, /*seed_anthropic_cred=*/ true).await;

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(messages_request(false).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(r.status(), StatusCode::OK);

    // Cost headers from the SAME pipeline as chat.
    for h in [
        "x-tokentrimmer-trace-id",
        "x-tokentrimmer-provider",
        "x-tokentrimmer-model-used",
        "x-tokentrimmer-cost-usd",
        "x-tokentrimmer-baseline-cost-usd",
        "x-tokentrimmer-saved-usd",
    ] {
        assert!(r.headers().contains_key(h), "missing cost header {h}");
    }
    assert_eq!(r.headers()["x-tokentrimmer-provider"], "anthropic");
    // BYO credential resolved and forwarded (not the raw bearer).
    assert_eq!(
        seen_key.lock().unwrap().clone(),
        Some("ANT_KEY".to_string())
    );

    // Inbound translation: system → leading System message, then the user turn.
    let req = seen_request
        .lock()
        .unwrap()
        .clone()
        .expect("provider called");
    assert_eq!(req.model, "claude-sonnet-4-6");
    assert_eq!(req.max_tokens, Some(64));
    assert!(matches!(&req.messages[0], Message::System { .. }));
    assert!(matches!(&req.messages[1], Message::User { .. }));

    // Anthropic-shaped response body.
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["model"], "claude-sonnet-4-6");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "Hello from Claude!");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["usage"]["input_tokens"], 100);
    assert_eq!(v["usage"]["output_tokens"], 20);
}

#[tokio::test]
async fn messages_streaming_yields_anthropic_sse_frames() {
    let org = Uuid::now_v7();
    let Harness { app, key, .. } = build(org, /*seed_anthropic_cred=*/ true).await;

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(messages_request(true).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(r.status(), StatusCode::OK);
    let ct = r.headers()["content-type"].to_str().unwrap();
    assert!(
        ct.contains("text/event-stream"),
        "expected SSE content-type, got {ct}"
    );
    // Cost headers present on the streaming response too.
    assert_eq!(r.headers()["x-tokentrimmer-provider"], "anthropic");
    assert!(r.headers().contains_key("x-tokentrimmer-trace-id"));

    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = std::str::from_utf8(&bytes).unwrap();

    // Anthropic typed SSE event frames — NOT OpenAI `data: {...}` / `[DONE]`.
    for ev in [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "event: content_block_stop",
        "event: message_delta",
        "event: message_stop",
    ] {
        assert!(body.contains(ev), "missing SSE event {ev} in:\n{body}");
    }
    assert!(body.contains("\"text_delta\""));
    assert!(body.contains("Hello!"));
    // Must NOT use the OpenAI terminator.
    assert!(
        !body.contains("data: [DONE]"),
        "Anthropic SSE must not emit the OpenAI [DONE] sentinel"
    );
}

#[tokio::test]
async fn messages_streaming_emits_tool_use_content_block() {
    let org = Uuid::now_v7();
    let Harness { app, key, .. } = build(org, /*seed_anthropic_cred=*/ true).await;

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(messages_tool_request(true).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(r.status(), StatusCode::OK);
    let ct = r.headers()["content-type"].to_str().unwrap();
    assert!(
        ct.contains("text/event-stream"),
        "expected SSE content-type, got {ct}"
    );

    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = std::str::from_utf8(&bytes).unwrap();

    // The streamed tool call MUST surface as an Anthropic tool_use content block
    // (start → input_json_delta → stop) — without this the response carries a
    // `tool_use` stop_reason but no runnable tool, breaking Claude Code's loop.
    assert!(
        body.contains("event: content_block_start"),
        "missing content_block_start in:\n{body}"
    );
    assert!(
        body.contains("\"type\":\"tool_use\""),
        "streamed tool call dropped — no tool_use block in:\n{body}"
    );
    assert!(
        body.contains("\"name\":\"get_weather\""),
        "tool_use block missing tool name in:\n{body}"
    );
    assert!(
        body.contains("\"id\":\"toolu_stream_1\""),
        "tool_use block missing tool id in:\n{body}"
    );
    // Arguments stream as input_json_delta suffixes (cumulative snapshots de-duped).
    assert!(
        body.contains("\"input_json_delta\""),
        "missing input_json_delta in:\n{body}"
    );
    assert!(
        body.contains("\"partial_json\":\"{\\\"city\\\""),
        "first arguments fragment missing in:\n{body}"
    );
    assert!(
        body.contains("\"partial_json\":\":\\\"SF\\\"}"),
        "second arguments fragment missing in:\n{body}"
    );
    assert!(
        body.contains("event: content_block_stop"),
        "tool_use block not closed in:\n{body}"
    );
    // tool_calls finish_reason maps to the Anthropic tool_use stop_reason.
    assert!(
        body.contains("\"stop_reason\":\"tool_use\""),
        "missing tool_use stop_reason in:\n{body}"
    );
    assert!(!body.contains("data: [DONE]"));
}

#[tokio::test]
async fn messages_credential_guard_fires_for_verified_org_without_anthropic_cred() {
    let org = Uuid::now_v7();
    // No anthropic credential seeded → #119 BYO-only guard must fire.
    let Harness {
        app,
        key,
        seen_request,
        ..
    } = build(org, /*seed_anthropic_cred=*/ false).await;

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(messages_request(false).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // Provider must never have been dispatched (no raw key forwarded upstream).
    assert!(seen_request.lock().unwrap().is_none());

    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // GW-MSG-ERRSHAPE: the body MUST be the Anthropic error envelope
    // ({"type":"error","error":{"type","message"}}), NOT the gateway's OpenAI-style
    // envelope — strict Anthropic SDK clients (Claude Code) parse error bodies.
    assert_eq!(v["type"], "error", "body must be Anthropic-shaped: {v}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    // The credential-guard message is preserved (mentions the provider + dashboard).
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("anthropic") && msg.contains("credential"),
        "credential-guard message not preserved: {msg}"
    );
    // The gateway-only OpenAI `code` field must not leak into the Anthropic shape.
    assert!(
        v["error"].get("code").is_none(),
        "Anthropic error body must not carry the OpenAI `code` field: {v}"
    );
}

/// A `tt_test_*` sandbox key streaming request: the chat handler short-circuits
/// to a JSON sandbox body even though `stream:true` was requested. The ingress
/// must branch on the response content-type (JSON here, not SSE) and transcode
/// to a well-formed Anthropic message body rather than an empty event stream.
#[tokio::test]
async fn messages_sandbox_streaming_transcodes_json_body() {
    let org = Uuid::now_v7();
    let Harness {
        app, seen_request, ..
    } = build(org, /*seed_anthropic_cred=*/ true).await;

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header(
                    "authorization",
                    "Bearer tt_test_abcdef0123456789abcdef0123456789",
                )
                .body(Body::from(messages_request(true).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(r.status(), StatusCode::OK);
    // Sandbox short-circuit: no provider dispatch.
    assert!(seen_request.lock().unwrap().is_none());
    assert_eq!(r.headers()["x-tokentrimmer-provider"], "sandbox");

    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert!(v["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("[sandbox]"));
}

/// An "anthropic" provider that always returns a deterministic 400-like error,
/// so a repeat request is served from the negative cache as
/// `Ok((400, Json({"error":..})))` (NOT via `?`). Records its call count.
struct AnthropicInvalidMock {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for AnthropicInvalidMock {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 200_000,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
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

/// A deterministic (temperature=0) non-streaming Messages request, so negative
/// caching engages on the first 4xx.
fn det_messages_request() -> serde_json::Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "repeat me"}],
        "temperature": 0,
        "stream": false,
    })
}

/// A negative-cache 4xx hit (an `Ok((400, Json({"error":..})))` from the chat
/// handler) must surface on `/v1/messages` with the same 400 status and the real
/// error message — NOT be mis-transcoded into a misleading 500 — but re-shaped into
/// the Anthropic error envelope (GW-MSG-ERRSHAPE). The first request populates the
/// negative cache; the second is served from it.
#[tokio::test]
async fn messages_negative_cache_4xx_passes_through_not_500() {
    let org = Uuid::now_v7();
    let calls = Arc::new(AtomicUsize::new(0));

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(AnthropicInvalidMock {
        calls: Arc::clone(&calls),
    }));

    let key_store = InMemoryKeyStore::new();
    let cred_store = InMemoryProviderCredentialStore::new();
    cred_store.insert(org, "anthropic", creds("ANT_KEY"));
    let key = issue_key_for(&key_store, org).await;

    let l1 = Arc::new(InMemoryL1Cache::new());
    let app = build_router(
        AppState::new(registry)
            .with_key_store(Arc::new(key_store) as Arc<dyn KeyStore>)
            .with_credential_store(Arc::new(cred_store) as Arc<dyn ProviderCredentialStore>)
            .with_l1(l1, None),
    );

    let make_req = || {
        Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(det_messages_request().to_string()))
            .unwrap()
    };

    // First request — provider returns InvalidRequest → 4xx; neg-cache populated.
    let r1 = app.clone().oneshot(make_req()).await.unwrap();
    assert!(
        r1.status().is_client_error(),
        "first request should be a 4xx; got {}",
        r1.status()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1, "provider called once");

    // Let the spawned negative-cache insert land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second request — served from the negative cache as Ok((4xx, Json(error))).
    // The ingress MUST pass it through verbatim, not mis-map it to a 500.
    let r2 = app.oneshot(make_req()).await.unwrap();
    assert_eq!(
        r2.status(),
        StatusCode::BAD_REQUEST,
        "neg-cache hit must pass through the 4xx, not become a 500; got {}",
        r2.status()
    );
    assert_eq!(
        r2.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("neg-hit"),
        "second request should be a negative-cache hit"
    );
    // Provider NOT called again — short-circuited by the negative cache.
    assert_eq!(calls.load(Ordering::Relaxed), 1, "provider not re-called");

    // The error is preserved (not a 'failed to parse' 500 body) and Anthropic-shaped.
    let bytes = axum::body::to_bytes(r2.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error", "body must be Anthropic-shaped: {v}");
    // 400 → Anthropic `invalid_request_error` per the status→type mapping.
    assert_eq!(v["error"]["type"], "invalid_request_error");
    // The original error message is preserved; the OpenAI-only `code` is dropped.
    assert!(
        v["error"]["message"].is_string(),
        "cached error message must be preserved; got {v}"
    );
    assert!(
        v["error"].get("code").is_none(),
        "Anthropic error body must not carry the OpenAI `code` field: {v}"
    );
}

/// An "anthropic" provider whose stream yields one good content chunk and THEN
/// fails mid-stream — exercising the in-band `upstream_error` SSE frame.
struct AnthropicMidStreamErrorMock;

#[async_trait]
impl Provider for AnthropicMidStreamErrorMock {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 200_000,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("non-streaming not used".into()))
    }
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let good = ChatCompletionChunk {
            id: "msg_err_1".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: req.model,
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: Some("assistant".into()),
                    content: Some("Partial".into()),
                    tool_calls: vec![],
                    extra: Default::default(),
                },
                finish_reason: None,
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        };
        let items: Vec<Result<ChatCompletionChunk, ProviderError>> = vec![
            Ok(good),
            Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "upstream exploded mid-stream".into(),
            }),
        ];
        Ok(futures::stream::iter(items).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// A mid-stream upstream failure on a streaming `/v1/messages` request MUST surface
/// as an Anthropic `event: error` frame — NOT be silently dropped and presented as
/// a clean (but truncated) successful termination.
#[tokio::test]
async fn messages_streaming_mid_stream_error_emits_anthropic_error_event() {
    let org = Uuid::now_v7();

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(AnthropicMidStreamErrorMock));

    let key_store = InMemoryKeyStore::new();
    let cred_store = InMemoryProviderCredentialStore::new();
    cred_store.insert(org, "anthropic", creds("ANT_KEY"));
    let key = issue_key_for(&key_store, org).await;

    let app = build_router(
        AppState::new(registry)
            .with_key_store(Arc::new(key_store) as Arc<dyn KeyStore>)
            .with_credential_store(Arc::new(cred_store) as Arc<dyn ProviderCredentialStore>),
    );

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(messages_request(true).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Initial establishment succeeded (the failure is mid-stream), so 200 + SSE.
    assert_eq!(r.status(), StatusCode::OK);
    let ct = r.headers()["content-type"].to_str().unwrap();
    assert!(ct.contains("text/event-stream"), "expected SSE, got {ct}");

    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = std::str::from_utf8(&bytes).unwrap();

    // The good prefix is still delivered…
    assert!(body.contains("Partial"), "good prefix missing in:\n{body}");
    // …then the mid-stream failure surfaces as an Anthropic error event.
    assert!(
        body.contains("event: error"),
        "mid-stream error not surfaced as an Anthropic error event in:\n{body}"
    );
    // GW-MSG-ERRSHAPE: the error frame payload MUST be the Anthropic error envelope
    // ({"type":"error","error":{"type","message"}}), not the OpenAI in-band shape.
    let err_data = body
        .split("event: error\n")
        .nth(1)
        .and_then(|tail| tail.lines().find_map(|l| l.strip_prefix("data: ")))
        .expect("error event must carry a data line");
    let v: serde_json::Value =
        serde_json::from_str(err_data).expect("error frame must be valid JSON");
    assert_eq!(v["type"], "error", "error frame not Anthropic-shaped: {v}");
    // The gateway's in-band `upstream_error` type maps to Anthropic's `api_error`.
    assert_eq!(v["error"]["type"], "api_error");
    let frame_msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        frame_msg.contains("upstream exploded mid-stream"),
        "error message not preserved in frame: {frame_msg}"
    );
    // It must NOT masquerade as a clean OpenAI terminator.
    assert!(!body.contains("data: [DONE]"));
}

/// An "anthropic" provider that fails a non-streaming request with an upstream 401.
/// This drives the `Err(ApiError::Provider(..))` path that propagates out of
/// `chat::handler` via `?` — distinct from both the credential-guard
/// (`ApiError::InvalidRequest`) and the negative-cache (`Ok((4xx, error body))`).
struct AnthropicUnauthorizedMock;

#[async_trait]
impl Provider for AnthropicUnauthorizedMock {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 200_000,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unauthorized(
            "invalid upstream api key".into(),
        ))
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Err(ProviderError::Unauthorized("no".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// An upstream error propagated from `chat::handler` (here a 401) must surface on
/// `/v1/messages` as the Anthropic error envelope with the mapped status — NOT the
/// gateway's OpenAI-style envelope (GW-MSG-ERRSHAPE).
#[tokio::test]
async fn messages_non_streaming_upstream_error_is_anthropic_shaped() {
    let org = Uuid::now_v7();

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(AnthropicUnauthorizedMock));

    let key_store = InMemoryKeyStore::new();
    let cred_store = InMemoryProviderCredentialStore::new();
    cred_store.insert(org, "anthropic", creds("ANT_KEY"));
    let key = issue_key_for(&key_store, org).await;

    let app = build_router(
        AppState::new(registry)
            .with_key_store(Arc::new(key_store) as Arc<dyn KeyStore>)
            .with_credential_store(Arc::new(cred_store) as Arc<dyn ProviderCredentialStore>),
    );

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(messages_request(false).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Upstream 401 → gateway 401.
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error", "body must be Anthropic-shaped: {v}");
    // 401 → Anthropic `authentication_error` per the status→type mapping.
    assert_eq!(v["error"]["type"], "authentication_error");
    assert!(
        v["error"]["message"].is_string(),
        "upstream error message must be present: {v}"
    );
    // No OpenAI-only `code` field in the Anthropic shape.
    assert!(v["error"].get("code").is_none(), "got {v}");
}

/// A malformed request body (not valid Anthropic JSON) fails at parse time with
/// `ApiError::InvalidRequest`. That 400 must also emit the Anthropic error envelope,
/// never reaching axum's OpenAI-style renderer (GW-MSG-ERRSHAPE).
#[tokio::test]
async fn messages_malformed_request_is_anthropic_shaped_400() {
    let org = Uuid::now_v7();
    let Harness { app, key, .. } = build(org, /*seed_anthropic_cred=*/ true).await;

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                // Missing required fields / not a valid MessagesRequest.
                .body(Body::from("{\"not\":\"a messages request\"}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error", "body must be Anthropic-shaped: {v}");
    assert_eq!(v["error"]["type"], "invalid_request_error");
}
