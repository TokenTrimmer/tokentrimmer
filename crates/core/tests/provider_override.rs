//! `X-TokenTrimmer-Provider` pins the dispatch provider for one request.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;

use tower::util::ServiceExt;
use tt_auth::credentials::{InMemoryProviderCredentialStore, ProviderCredentialStore};
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{Choice, EmbeddingData, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// A provider that records its call count. `model` is the id it claims in
/// `models()` (and thus the `by_model` registry entry); `None` makes it
/// reachable only by id (the pin path).
struct FakeProvider {
    id: &'static str,
    model: Option<&'static str>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        let Some(model) = self.model else {
            return vec![];
        };
        vec![ModelInfo {
            id: model.into(),
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
        req: EmbeddingsRequest,
        _c: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(EmbeddingsResponse {
            object: "list".into(),
            data: vec![EmbeddingData {
                object: "embedding".into(),
                index: 0,
                embedding: vec![0.0, 0.1, 0.2],
            }],
            model: req.model,
            usage: Usage {
                prompt_tokens: 3,
                completion_tokens: 0,
                total_tokens: 3,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
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
        model: Some("gpt-4o"),
        calls: Arc::clone(&alpha_calls),
    }));
    registry.register(Arc::new(FakeProvider {
        id: "beta",
        model: None,
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

fn embeddings_request(provider_header: Option<&str>, key: &str) -> Request<Body> {
    let body = json!({ "model": "gpt-4o", "input": "hello" });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"));
    if let Some(p) = provider_header {
        b = b.header("x-tokentrimmer-provider", p);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn embeddings_pin_overrides_serving_provider() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(embeddings_request(Some("beta"), &h.key))
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
async fn embeddings_cross_provider_pin_without_credential_fails_closed() {
    let h = harness(true).await; // empty credential store
    let resp = h
        .app
        .clone()
        .oneshot(embeddings_request(Some("beta"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0, "must not dispatch");
}

/// Regression for the cross-provider credential leak: a route that crosses
/// providers AND declares fallbacks defers per-candidate credential resolution,
/// leaving `ctx.credentials` holding the SOURCE key. Pinning the already-routed
/// target must still re-resolve the target's credential and fail closed — never
/// forward the source key. (Pre-fix, the `pinned == current` short-circuit left
/// the stale source key and dispatched it to the target.)
#[tokio::test]
async fn cross_provider_route_with_fallbacks_pin_does_not_leak_source_credential() {
    let alpha_calls = Arc::new(AtomicUsize::new(0));
    let beta_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(FakeProvider {
        id: "alpha",
        model: Some("gpt-4o"),
        calls: Arc::clone(&alpha_calls),
    }));
    registry.register(Arc::new(FakeProvider {
        id: "beta",
        model: Some("gpt-4o-mini"),
        calls: Arc::clone(&beta_calls),
    }));

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    // Credential store has ONLY the source provider's key (alpha), not beta's.
    let cred_store = InMemoryProviderCredentialStore::new();
    cred_store
        .put(
            org,
            "alpha",
            "k",
            ProviderCredentials {
                api_key: SecretString::new("alpha-secret".to_string()),
                base_url: None,
                extra_headers: vec![],
            },
        )
        .await
        .unwrap();

    // Route: tag=sensitive → gpt-4o-mini (beta), WITH a fallback chain (non-empty
    // fallbacks trigger the deferred per-candidate credential path).
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "cross".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                tag_equals: Some("sensitive".into()),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: vec!["gpt-4o".into()],
                force_cache_layer: None,
                disable_cache: false,
                max_cost_usd: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(Arc::new(cred_store))
            .with_routing_store(routing),
    );

    // model gpt-4o (source=alpha), tag=sensitive (routes to beta), pin=beta.
    let body =
        json!({ "model": "gpt-4o", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .header("x-tokentrimmer-tag", "sensitive")
        .header("x-tokentrimmer-provider", "beta")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    // beta has no stored credential → must fail closed, never dispatch with alpha's key.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        beta_calls.load(Ordering::Relaxed),
        0,
        "beta must not be dispatched with the source provider's credential"
    );
}
