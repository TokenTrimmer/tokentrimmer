//! A05: cross-provider failover combined with STREAMING dispatch. The
//! buffered failover path (`cross_provider.rs`) is well-covered, but the
//! streaming entry (`dispatch_stream_with_failover`) had no test proving a
//! failed primary stream establishment falls over to the next candidate —
//! still streaming, with the fallback provider's own credential, and a
//! well-formed SSE body to the client.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, InMemoryProviderCredentialStore, KeyStore, ProviderCredentialStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{ChunkChoice, ChunkDelta},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Provider mock whose streaming path records the credential it saw and can
/// be made to fail stream ESTABLISHMENT (the only phase failover can act on).
struct Mock {
    id: &'static str,
    model: &'static str,
    seen_keys: Arc<Mutex<Vec<String>>>,
    fail_stream: bool,
    /// Stream a single content delta so the SSE body has real data.
    emit_content: Option<&'static str>,
}
#[async_trait]
impl Provider for Mock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
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
        _: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        unreachable!("streaming test")
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.seen_keys
            .lock()
            .unwrap()
            .push(ctx.credentials.api_key.expose().to_string());
        if self.fail_stream {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "primary stream unavailable".into(),
            });
        }
        let content = self.emit_content.unwrap_or("");
        let chunk = ChatCompletionChunk {
            id: format!("{}-chunk", self.id),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: self.model.into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: Some("assistant".into()),
                    content: Some(content.into()),
                    tool_calls: Vec::new(),
                    extra: Default::default(),
                },
                finish_reason: Some("stop".into()),
                extra: Default::default(),
            }],
            usage: Some(Usage {
                prompt_tokens: 4,
                completion_tokens: 2,
                total_tokens: 6,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            extra: Default::default(),
        };
        Ok(futures::stream::iter(vec![Ok(chunk)]).boxed())
    }
    async fn embeddings(
        &self,
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        unreachable!("no embeddings")
    }
}

fn creds(key: &str) -> ProviderCredentials {
    ProviderCredentials {
        api_key: SecretString::new(key.to_string()),
        base_url: None,
        extra_headers: Vec::new(),
    }
}

fn route(target: &str, fallbacks: Vec<String>) -> Route {
    Route {
        paused: false,
        id: Uuid::now_v7(),
        name: "x-provider-stream".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: Some(target.into()),
            fallbacks,
            ..Default::default()
        },
    }
}

fn chat(model: &str, bearer: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(
            json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": true })
                .to_string(),
        ))
        .unwrap()
}

/// openai(gpt-4o, healthy) + anthropic(claude target, streams fail) +
/// gemini(fallback, streams a chunk). Route: gpt-4o → claude with gemini
/// fallback.
fn build(
    org: Uuid,
    key_store: Arc<dyn KeyStore>,
    cred_store: Arc<dyn ProviderCredentialStore>,
    anthropic_keys: Arc<Mutex<Vec<String>>>,
    gemini_keys: Arc<Mutex<Vec<String>>>,
) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "openai",
        model: "gpt-4o",
        seen_keys: Arc::new(Mutex::new(Vec::new())),
        fail_stream: false,
        emit_content: None,
    }));
    registry.register(Arc::new(Mock {
        id: "anthropic",
        model: "claude-haiku-4-5",
        seen_keys: anthropic_keys,
        fail_stream: true,
        emit_content: None,
    }));
    registry.register(Arc::new(Mock {
        id: "gemini",
        model: "gemini-2.5-flash",
        seen_keys: gemini_keys,
        fail_stream: false,
        emit_content: Some("gemini here"),
    }));
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![route("claude-haiku-4-5", vec!["gemini-2.5-flash".into()])],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(cred_store)
            .with_routing_store(routing),
    )
}

/// Parse the raw SSE body into the JSON payloads of every `data:` frame.
fn sse_data_frames(body: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(body);
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("valid SSE data frame"))
        .collect()
}

#[tokio::test]
async fn streaming_falls_over_cross_provider_with_own_credential_and_sse_body() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue(
        &raw,
        &InMemoryAuditWriter::new(),
        org,
        "x-stream",
        Environment::Live,
        Actor::System,
    )
    .await
    .unwrap()
    .plaintext;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI"));
    store.insert(org, "anthropic", creds("ANT"));
    store.insert(org, "gemini", creds("GEM"));
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let gemini_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build(
        org,
        Arc::new(raw),
        Arc::new(store),
        Arc::clone(&anthropic_keys),
        Arc::clone(&gemini_keys),
    );

    let response = app.oneshot(chat("gpt-4o", &key)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The response must be an SSE stream.
    assert_eq!(
        response.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    // Served by the FALLBACK provider after the primary's stream establishment
    // failed — cross-provider attribution is correct on the streaming path.
    assert_eq!(
        response.headers()["x-tokentrimmer-provider"]
            .to_str()
            .unwrap(),
        "gemini"
    );
    // Each candidate saw only its OWN provider's credential — the source
    // provider's key (or the caller's raw bearer) never leaks cross-provider.
    let ant = anthropic_keys.lock().unwrap().clone();
    assert!(
        !ant.is_empty() && ant.iter().all(|k| k == "ANT"),
        "anthropic must see only its own key, got {ant:?}"
    );
    let gem = gemini_keys.lock().unwrap().clone();
    assert!(
        !gem.is_empty() && gem.iter().all(|k| k == "GEM"),
        "gemini must see only its own key, got {gem:?}"
    );

    // The streamed body is well-formed SSE carrying the fallback's content.
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("data: "), "no SSE frames in body");
    let frames = sse_data_frames(&body);
    assert!(!frames.is_empty(), "at least one data frame");
    let saw_content = frames.iter().any(|frame| {
        frame["choices"][0]["delta"]["content"]
            .as_str()
            .is_some_and(|c| c.contains("gemini here"))
    });
    assert!(
        saw_content,
        "fallback content not streamed: {}",
        frames
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(body_str.contains("data: [DONE]"), "stream must terminate");
}

#[tokio::test]
async fn streaming_all_candidates_fail_surfaces_upstream_error() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue(
        &raw,
        &InMemoryAuditWriter::new(),
        org,
        "x-stream-2",
        Environment::Live,
        Actor::System,
    )
    .await
    .unwrap()
    .plaintext;
    let store = InMemoryProviderCredentialStore::new();
    // No credentials anywhere: every candidate fails closed BEFORE dispatch
    // (resolve_credentials_for returns None), so nothing reaches either
    // provider and the client sees the structured failure.
    let app = build(
        org,
        Arc::new(raw),
        Arc::new(store),
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    );

    let response = app.oneshot(chat("gpt-4o", &key)).await.unwrap();
    assert!(
        response.status() != StatusCode::OK,
        "expected failure, got {}",
        response.status()
    );
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        body.to_string().contains("error"),
        "structured error body: {body}"
    );
}
