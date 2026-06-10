//! GW-MESSAGES: the hosted gateway exposes a POST /v1/messages ingress that
//! accepts the Anthropic Messages API request shape, runs it through the SAME
//! cost / routing / cache / credential pipeline as /v1/chat/completions, and
//! returns the Anthropic Messages response shape (Anthropic SSE when streaming).
//!
//! These are hermetic: a mock provider stands in for the upstream, so no network
//! is required. They assert the round-trip wire shape, the x-tokentrimmer-* cost
//! headers, the Anthropic SSE event frames, and the #119 BYO-only credential
//! guard (verified org without an anthropic credential → missing_provider_credential).

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
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
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
        let chunks = vec![
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
                    },
                    finish_reason: None,
                }],
                usage: None,
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
                    },
                    finish_reason: None,
                }],
                usage: None,
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
                }],
                usage: Some(Usage {
                    prompt_tokens: 100,
                    completion_tokens: 20,
                    total_tokens: 120,
                    cached_tokens: 0,
                    cache_creation_input_tokens: None,
                }),
            },
        ];
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
    assert_eq!(v["error"]["code"], "missing_provider_credential");
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
