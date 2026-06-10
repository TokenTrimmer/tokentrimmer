//! V3d-1: cross-provider routing dispatches to the target provider with the
//! TARGET provider's upstream credential, and fails closed when the org has no
//! credential for the target.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
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
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// A provider with a fixed id serving one model; records the api_key it saw on
/// each call, and can be made to fail (for failover tests in Task 3).
struct Mock {
    id: &'static str,
    model: &'static str,
    input_price: f64,
    output_price: f64,
    seen_keys: Arc<Mutex<Vec<String>>>,
    fail: bool,
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
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: self.input_price,
                output_per_million: self.output_price,
                cached_input_per_million: None,
                cache_write_per_million: None,
                batch_input_per_million: None,
                batch_output_per_million: None,
                flex_input_per_million: None,
                flex_output_per_million: None,
                prompt_cache_min_tokens: None,
                effective_at: Utc::now(),
            })
        } else {
            None
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.seen_keys
            .lock()
            .unwrap()
            .push(ctx.credentials.api_key.expose().to_string());
        if self.fail {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "mock failure".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: "chatcmpl-mock".into(),
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
        _req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.seen_keys
            .lock()
            .unwrap()
            .push(ctx.credentials.api_key.expose().to_string());
        if self.fail {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "mock failure".into(),
            });
        }
        Ok(futures::stream::iter(vec![]).boxed())
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

fn route(target: &str, fallbacks: Vec<String>) -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "x-provider".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: target.into(),
            fallbacks,
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
        },
    }
}

fn chat(model: &str, bearer: &str, stream: bool) -> Request<Body> {
    let body =
        json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": stream });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Build an app: openai(gpt-4o) + anthropic(claude-haiku-4-5) mocks, a
/// gpt-4o -> claude-haiku-4-5 route, and a credential store the caller seeds.
fn build(
    org: Uuid,
    key_store: Arc<dyn KeyStore>,
    cred_store: Arc<dyn ProviderCredentialStore>,
    openai_keys: Arc<Mutex<Vec<String>>>,
    anthropic_keys: Arc<Mutex<Vec<String>>>,
) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "openai",
        model: "gpt-4o",
        input_price: 5.0,
        output_price: 15.0,
        seen_keys: openai_keys,
        fail: false,
    }));
    registry.register(Arc::new(Mock {
        id: "anthropic",
        model: "claude-haiku-4-5",
        input_price: 0.25,
        output_price: 1.25,
        seen_keys: anthropic_keys,
        fail: false,
    }));
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, vec![route("claude-haiku-4-5", vec![])]);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(cred_store)
            .with_routing_store(routing),
    )
}

#[tokio::test]
async fn cross_provider_uses_target_credential() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI"));
    store.insert(org, "anthropic", creds("ANT"));
    let openai_keys = Arc::new(Mutex::new(Vec::new()));
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build(
        org,
        Arc::new(raw),
        Arc::new(store),
        Arc::clone(&openai_keys),
        Arc::clone(&anthropic_keys),
    );

    let r = app.oneshot(chat("gpt-4o", &key, false)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Dispatched to anthropic (the target) with ANTHROPIC's key — not OpenAI's.
    assert_eq!(
        anthropic_keys.lock().unwrap().clone(),
        vec!["ANT".to_string()]
    );
    assert!(openai_keys.lock().unwrap().is_empty());
    assert_eq!(
        r.headers()["x-tokentrimmer-provider"].to_str().unwrap(),
        "anthropic"
    );
}

#[tokio::test]
async fn cross_provider_missing_credential_fails_closed() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI")); // no anthropic credential
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build(
        org,
        Arc::new(raw),
        Arc::new(store),
        Arc::new(Mutex::new(Vec::new())),
        Arc::clone(&anthropic_keys),
    );

    let r = app.oneshot(chat("gpt-4o", &key, false)).await.unwrap();
    // Fail closed: never forward the OpenAI/raw key to Anthropic.
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(anthropic_keys.lock().unwrap().is_empty());
}

/// Build an app whose route is `gpt-4o -> claude-haiku-4-5` with a
/// `gemini-2.5-flash` fallback. The anthropic primary can be made to fail so
/// dispatch falls over to gemini.
#[allow(clippy::too_many_arguments)]
fn build_failover(
    org: Uuid,
    key_store: Arc<dyn KeyStore>,
    cred_store: Arc<dyn ProviderCredentialStore>,
    anthropic_fail: bool,
    anthropic_keys: Arc<Mutex<Vec<String>>>,
    gemini_keys: Arc<Mutex<Vec<String>>>,
) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "openai",
        model: "gpt-4o",
        input_price: 5.0,
        output_price: 15.0,
        seen_keys: Arc::new(Mutex::new(Vec::new())),
        fail: false,
    }));
    registry.register(Arc::new(Mock {
        id: "anthropic",
        model: "claude-haiku-4-5",
        input_price: 0.25,
        output_price: 1.25,
        seen_keys: anthropic_keys,
        fail: anthropic_fail,
    }));
    registry.register(Arc::new(Mock {
        id: "gemini",
        model: "gemini-2.5-flash",
        input_price: 0.1,
        output_price: 0.4,
        seen_keys: gemini_keys,
        fail: false,
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

#[tokio::test]
async fn failover_candidate_uses_its_own_provider_credential() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI"));
    store.insert(org, "anthropic", creds("ANT"));
    store.insert(org, "gemini", creds("GEM"));
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let gemini_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build_failover(
        org,
        Arc::new(raw),
        Arc::new(store),
        /*anthropic_fail=*/ true,
        Arc::clone(&anthropic_keys),
        Arc::clone(&gemini_keys),
    );

    let r = app.oneshot(chat("gpt-4o", &key, false)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Primary (anthropic) was tried with ANT (possibly retried), failed; fell
    // over to gemini with GEM. Each candidate saw only its OWN provider's key —
    // never the source openai key.
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
    assert_eq!(
        r.headers()["x-tokentrimmer-provider"].to_str().unwrap(),
        "gemini"
    );
}

#[tokio::test]
async fn failover_skips_candidate_without_credential() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI"));
    store.insert(org, "anthropic", creds("ANT")); // no gemini credential
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let gemini_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build_failover(
        org,
        Arc::new(raw),
        Arc::new(store),
        /*anthropic_fail=*/ true,
        Arc::clone(&anthropic_keys),
        Arc::clone(&gemini_keys),
    );

    let r = app.oneshot(chat("gpt-4o", &key, false)).await.unwrap();
    // anthropic failed; gemini has no credential so it is skipped (never called);
    // no candidate succeeds -> upstream error surfaced (5xx), gemini untouched.
    assert!(gemini_keys.lock().unwrap().is_empty());
    assert!(r.status().is_server_error());
}
