//! End-to-end test: plant a route, send a chat request, assert the gateway
//! rewrote the model and dispatched to the routed target.
//!
//! Mirrors the dashboard → tt-api → routes table → gateway loop without the
//! DB roundtrip: an [`InMemoryRoutingStore`] stands in for the Postgres
//! backend, wrapped in the production [`CachingRoutingStore`].

use std::sync::atomic::{AtomicUsize, Ordering};
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
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Provider that records the model it was asked to serve.
struct RecordingProvider {
    served_models: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "gpt-4o".into(),
                provider: "recording".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            },
            ModelInfo {
                id: "gpt-4o-mini".into(),
                provider: "recording".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            },
        ]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        // Model-aware so a downgrade route produces a measurable saving:
        // gpt-4o is ~33x the price of gpt-4o-mini on input, 25x on output.
        let (input_per_million, output_per_million) = match model {
            "gpt-4o" => (5.0, 15.0),
            "gpt-4o-mini" => (0.15, 0.6),
            _ => (1.0, 2.0),
        };
        Some(ModelPricing {
            input_per_million,
            output_per_million,
            cached_input_per_million: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.served_models.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "chatcmpl-rec".into(),
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
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
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

fn chat_request(model: &str, bearer: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello world" }],
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Provision an org with an issued live key + plumbed key store.
async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    let issued = issue(
        store,
        &audit,
        org_id,
        "test-key",
        Environment::Live,
        Actor::System,
    )
    .await
    .expect("issue tt_live_ key");
    issued.plaintext
}

#[tokio::test]
async fn route_rewrites_model_when_org_has_matching_rule() {
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served_models: Arc::clone(&served),
        calls: Arc::clone(&calls),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    // Plant a route: gpt-4o → gpt-4o-mini for this org.
    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    let route_id = Uuid::now_v7();
    routes_backing.set_routes(
        org_id,
        vec![Route {
            id: route_id,
            name: "downgrade-4o".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );

    let resp = app
        .oneshot(chat_request("gpt-4o", &plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider should be called exactly once"
    );

    // Provider observed the REWRITTEN model — the dashboard wrote a route, the
    // gateway read it, the rewrite landed before dispatch.
    let saw = served.lock().unwrap().clone();
    assert_eq!(saw, vec!["gpt-4o-mini".to_string()]);

    // The response advertises the served model (which is now the routed target).
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o-mini"
    );
}

#[tokio::test]
async fn routed_request_reports_savings_against_original_model() {
    // A downgrade route (gpt-4o → gpt-4o-mini) must report the saving the
    // customer actually got: baseline priced against the ORIGINAL requested
    // model, cost against the routed-to model. Before the routing-baseline
    // fix this reported saved == 0 because baseline was priced against the
    // cheap routed model.
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served_models: Arc::clone(&served),
        calls: Arc::clone(&calls),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(
        org_id,
        vec![Route {
            id: Uuid::now_v7(),
            name: "downgrade-4o".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );

    let resp = app
        .oneshot(chat_request("gpt-4o", &plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let header_f64 = |name: &str| -> f64 {
        resp.headers()[name]
            .to_str()
            .unwrap()
            .parse::<f64>()
            .unwrap()
    };
    let cost = header_f64("x-tokentrimmer-cost-usd");
    let baseline = header_f64("x-tokentrimmer-baseline-cost-usd");
    let saved = header_f64("x-tokentrimmer-saved-usd");

    // Cost is the routed (cheap) model; baseline is the original (expensive)
    // model; the saving is the difference and must be positive.
    assert!(
        baseline > cost,
        "baseline ({baseline}) should exceed routed cost ({cost})"
    );
    assert!(
        saved > 0.0,
        "routing a flagship model down should report a positive saving, got {saved}"
    );
    assert!(
        (saved - (baseline - cost)).abs() < 1e-9,
        "saved ({saved}) should equal baseline - cost ({})",
        baseline - cost
    );
}

#[tokio::test]
async fn no_routing_store_passes_request_through_unchanged() {
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served_models: Arc::clone(&served),
        calls: Arc::clone(&calls),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    // No `with_routing_store(...)`.
    let app = build_router(AppState::new(registry).with_key_store(key_store));

    let resp = app
        .oneshot(chat_request("gpt-4o", &plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served.lock().unwrap().clone(), vec!["gpt-4o".to_string()]);
}

#[tokio::test]
async fn route_for_other_org_does_not_match() {
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served_models: Arc::clone(&served),
        calls: Arc::clone(&calls),
    }));

    let raw_store = InMemoryKeyStore::new();
    let our_org = Uuid::now_v7();
    let other_org = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, our_org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    // Route belongs to a DIFFERENT org → must not fire for our caller.
    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(
        other_org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "wrong-org".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );

    let resp = app
        .oneshot(chat_request("gpt-4o", &plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        served.lock().unwrap().clone(),
        vec!["gpt-4o".to_string()],
        "route from another org must not bleed into this org's request"
    );
}

#[tokio::test]
async fn route_skipped_when_no_resolvable_org() {
    // No auth ctx → org_id == Uuid::nil() → routing skips even if a store is
    // wired (otherwise every unauthed request would share an "anonymous" route
    // pool, which is a footgun).
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served_models: Arc::clone(&served),
        calls: Arc::clone(&calls),
    }));

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(
        Uuid::nil(),
        vec![Route {
            id: Uuid::now_v7(),
            name: "anonymous-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions::default(),
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let app = build_router(AppState::new(registry).with_routing_store(routing));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "gpt-4o",
                        "messages": [{"role": "user", "content": "hi"}],
                        "stream": false,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served.lock().unwrap().clone(), vec!["gpt-4o".to_string()]);
}
