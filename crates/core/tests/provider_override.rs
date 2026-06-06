//! `X-TokenTrimmer-Provider` pins the dispatch provider for one request.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;

use tower::util::ServiceExt;
use tt_auth::credentials::InMemoryProviderCredentialStore;
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// A provider that records its call count. `owns_model` controls whether it
/// claims `gpt-4o` in `models()` (and thus the `by_model` registry entry).
struct FakeProvider {
    id: &'static str,
    owns_model: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        if !self.owns_model {
            return vec![];
        }
        vec![ModelInfo {
            id: "gpt-4o".into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _c: &RequestContext,
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
                    content: Some(MessageContent::Text("ok".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _r: ChatCompletionRequest,
        _c: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _r: EmbeddingsRequest,
        _c: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .unwrap()
        .plaintext
}

struct Harness {
    app: axum::Router,
    key: String,
    alpha_calls: Arc<AtomicUsize>,
    beta_calls: Arc<AtomicUsize>,
}

/// Build the app with two providers: `alpha` owns `gpt-4o`; `beta` is id-only
/// (reachable only via the pin). `with_cred_store` adds an empty credential store
/// (forces the cross-provider fail-closed path).
async fn harness(with_cred_store: bool) -> Harness {
    let alpha_calls = Arc::new(AtomicUsize::new(0));
    let beta_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(FakeProvider {
        id: "alpha",
        owns_model: true,
        calls: Arc::clone(&alpha_calls),
    }));
    registry.register(Arc::new(FakeProvider {
        id: "beta",
        owns_model: false,
        calls: Arc::clone(&beta_calls),
    }));

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    let mut state = AppState::new(registry).with_key_store(key_store);
    if with_cred_store {
        state = state.with_credential_store(Arc::new(InMemoryProviderCredentialStore::new()));
    }
    Harness {
        app: build_router(state),
        key,
        alpha_calls,
        beta_calls,
    }
}

fn chat_request(provider_header: Option<&str>, key: &str) -> Request<Body> {
    let body =
        json!({ "model": "gpt-4o", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"));
    if let Some(p) = provider_header {
        b = b.header("x-tokentrimmer-provider", p);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn pin_overrides_serving_provider() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(Some("beta"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("beta")
    );
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 1);
    assert_eq!(h.alpha_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn no_header_uses_model_default_provider() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(None, &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.alpha_calls.load(Ordering::Relaxed), 1);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn pin_same_as_source_is_noop() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(Some("alpha"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.alpha_calls.load(Ordering::Relaxed), 1);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn pin_unknown_provider_is_400() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(Some("nope"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.alpha_calls.load(Ordering::Relaxed), 0);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn cross_provider_pin_without_credential_fails_closed() {
    let h = harness(true).await; // empty credential store
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(Some("beta"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0, "must not dispatch");
}
