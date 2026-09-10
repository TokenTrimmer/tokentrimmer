//! A05: routing-triggered panels. The header-panel path has thorough coverage
//! (panel_engine.rs / panel_dispatch.rs) but the ROUTE trigger — `then.panel`
//! on a matched route — and its header-wins priority had no dedicated test.
//! Invariants: (1) a matched route with a panel fans out to the ROUTE's member
//! models with no header present, (2) an explicit header beats the route's
//! panel (header members dispatch, route members stay idle), (3) an unmatched
//! route never triggers a panel.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, InMemoryProviderCredentialStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutePanel,
    RoutingStore,
};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Mock serving exactly one model; counts its own dispatches.
struct CountedMock {
    id: &'static str,
    model: &'static str,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Provider for CountedMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
            verified_at: None,
        })
        .filter(|_| model == self.model)
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(serde_json::from_value(json!({
            "id": format!("{}-resp", self.model), "object": "chat.completion", "created": 0,
            "model": self.model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": format!("from {}", self.model)}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        }))
        .unwrap())
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        unreachable!("non-streaming panel test")
    }
    async fn embeddings(
        &self,
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        unreachable!("no embeddings")
    }
}

fn panel_route(panel: RoutePanel) -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "panel-route".into(),
        priority: 100,
        enabled: true,
        paused: false,
        when: RouteConditions {
            prompt_contains_any_of: vec!["fan me out".into()],
            ..Default::default()
        },
        then: RouteAction {
            panel: Some(panel),
            ..Default::default()
        },
    }
}

fn chat(bearer: &str, text: &str, panel_header: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        // Panel pre-dispatch admission budget (must exceed the fan-out estimate).
        .header("x-tokentrimmer-cost-limit-usd", "10.0");
    if let Some(value) = panel_header {
        builder = builder.header("x-tokentrimmer-panel", value);
    }
    builder
        .body(Body::from(
            json!({ "model": "gpt-4o", "max_tokens": 64, "messages": [{"role":"user","content": text}] })
                .to_string(),
        ))
        .unwrap()
}

/// Router with: gpt-4o (source member, provider "openai"), gpt-4o-mini (source
/// member alt, provider "openai-mini"), route-panel members {a-model, b-model}
/// + arbiter arb-model, and the given route panel.
#[allow(clippy::too_many_arguments)] // test harness wiring; each arg is a distinct provider counter
fn build(
    org: Uuid,
    key_store: Arc<InMemoryKeyStore>,
    backing: Arc<InMemoryRoutingStore>,
    openai_calls: Arc<AtomicUsize>,
    mini_calls: Arc<AtomicUsize>,
    vendor_a_calls: Arc<AtomicUsize>,
    vendor_b_calls: Arc<AtomicUsize>,
    arb_calls: Arc<AtomicUsize>,
) -> axum::Router {
    let credential_store = Arc::new(InMemoryProviderCredentialStore::new());
    credential_store.insert(
        org,
        "openai",
        tt_shared::context::ProviderCredentials {
            api_key: tt_shared::context::SecretString::new("openai-key".to_string()),
            base_url: None,
            extra_headers: Vec::new(),
        },
    );
    credential_store.insert(
        org,
        "openai-mini",
        tt_shared::context::ProviderCredentials {
            api_key: tt_shared::context::SecretString::new("mini-key".to_string()),
            base_url: None,
            extra_headers: Vec::new(),
        },
    );
    credential_store.insert(
        org,
        "vendor-a",
        tt_shared::context::ProviderCredentials {
            api_key: tt_shared::context::SecretString::new("a-key".to_string()),
            base_url: None,
            extra_headers: Vec::new(),
        },
    );
    credential_store.insert(
        org,
        "vendor-b",
        tt_shared::context::ProviderCredentials {
            api_key: tt_shared::context::SecretString::new("b-key".to_string()),
            base_url: None,
            extra_headers: Vec::new(),
        },
    );
    credential_store.insert(
        org,
        "arb-provider",
        tt_shared::context::ProviderCredentials {
            api_key: tt_shared::context::SecretString::new("arb-key".to_string()),
            base_url: None,
            extra_headers: Vec::new(),
        },
    );
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountedMock {
        id: "openai",
        model: "gpt-4o",
        calls: openai_calls,
    }));
    registry.register(Arc::new(CountedMock {
        id: "openai-mini",
        model: "gpt-4o-mini",
        calls: mini_calls,
    }));
    registry.register(Arc::new(CountedMock {
        id: "vendor-a",
        model: "a-model",
        calls: vendor_a_calls,
    }));
    registry.register(Arc::new(CountedMock {
        id: "vendor-b",
        model: "b-model",
        calls: vendor_b_calls,
    }));
    registry.register(Arc::new(CountedMock {
        id: "arb-provider",
        model: "arb-model",
        calls: arb_calls,
    }));
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(credential_store)
            .with_routing_store(routing)
            .with_panel_enabled(true),
    )
}

async fn harness_with_route(
    panel: RoutePanel,
) -> (
    axum::Router,
    String,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let org = Uuid::now_v7();
    let keys = Arc::new(InMemoryKeyStore::new());
    let key = issue(
        keys.as_ref(),
        &InMemoryAuditWriter::new(),
        org,
        "panel-route",
        Environment::Live,
        Actor::System,
    )
    .await
    .unwrap()
    .plaintext;
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, vec![panel_route(panel)]);
    let openai = Arc::new(AtomicUsize::new(0));
    let mini = Arc::new(AtomicUsize::new(0));
    let vendor_a = Arc::new(AtomicUsize::new(0));
    let vendor_b = Arc::new(AtomicUsize::new(0));
    let arb = Arc::new(AtomicUsize::new(0));
    let app = build(
        org,
        keys,
        backing,
        Arc::clone(&openai),
        Arc::clone(&mini),
        Arc::clone(&vendor_a),
        Arc::clone(&vendor_b),
        Arc::clone(&arb),
    );
    (app, key, openai, mini, vendor_a, vendor_b, arb)
}

#[tokio::test]
async fn matched_route_triggers_panel_fanout_with_route_members() {
    let (app, key, openai, mini, vendor_a, vendor_b, arb) = harness_with_route(RoutePanel {
        strategy: "best-of-n".into(),
        members: vec!["a-model".into(), "b-model".into()],
        arbiter: Some("arb-model".into()),
        ..Default::default()
    })
    .await;

    // Matching text, NO panel header: the ROUTE is the trigger.
    let response = app
        .oneshot(chat(&key, "please fan me out across vendors", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "route panel dispatches");
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // The arbiter picks one of the member outputs.
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("a-model") || content.contains("b-model"),
        "arbiter merged member output: {content}"
    );

    // The route's PANEL members dispatched (fan-out happened)...
    assert!(
        vendor_a.load(Ordering::SeqCst) >= 1,
        "route member a-model must dispatch"
    );
    assert!(
        vendor_b.load(Ordering::SeqCst) >= 1,
        "route member b-model must dispatch"
    );
    // ...the arbiter arbitrated (best-of-n needs a judge call)...
    assert!(
        arb.load(Ordering::SeqCst) >= 1,
        "arbiter must dispatch on best-of-n"
    );
    // ...and the caller-requested single-model path never dual-dispatched
    // (the panel REPLACES the single-model dispatch).
    assert_eq!(
        openai.load(Ordering::SeqCst),
        0,
        "panel replaces the single-model path"
    );
    assert_eq!(mini.load(Ordering::SeqCst), 0);
}

fn chat_with_extras(bearer: &str, text: &str, strategy: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .header("x-tokentrimmer-panel", strategy)
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "max_tokens": 64,
                "messages": [{"role":"user","content": text}],
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn explicit_panel_header_beats_route_panel() {
    let (app, key, openai, mini, vendor_a, vendor_b, _arb) = harness_with_route(RoutePanel {
        strategy: "best-of-n".into(),
        members: vec!["a-model".into(), "b-model".into()],
        arbiter: Some("arb-model".into()),
        ..Default::default()
    })
    .await;

    // Matching text AND an explicit header: the HEADER wins. The header's
    // tt_extras panel resolves to gpt-4o + gpt-4o-mini instead of the route's
    // vendor members.
    let response = app
        .oneshot(chat_with_extras(
            &key,
            "please fan me out across vendors",
            "best-of-n",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "header panel dispatches");

    // The HEADER-sourced members dispatched (via env defaults)...
    assert!(
        openai.load(Ordering::SeqCst) >= 1,
        "header member gpt-4o must dispatch"
    );
    assert!(
        mini.load(Ordering::SeqCst) >= 1,
        "header member gpt-4o-mini must dispatch"
    );
    // ...and the ROUTE's panel members stayed IDLE — the header replaces,
    // it never stacks a second fan-out.
    assert_eq!(
        vendor_a.load(Ordering::SeqCst),
        0,
        "route member must not dispatch when the header wins"
    );
    assert_eq!(vendor_b.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unmatched_route_leaves_single_model_path_untouched() {
    let (app, key, openai, mini, vendor_a, vendor_b, arb) = harness_with_route(RoutePanel {
        strategy: "best-of-n".into(),
        members: vec!["a-model".into(), "b-model".into()],
        arbiter: Some("arb-model".into()),
        ..Default::default()
    })
    .await;

    // NON-matching text: no route ⇒ no panel ⇒ single-model dispatch.
    let response = app
        .oneshot(chat(&key, "unrelated request text", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        openai.load(Ordering::SeqCst),
        1,
        "single-model path dispatches exactly once"
    );
    assert_eq!(vendor_a.load(Ordering::SeqCst), 0);
    assert_eq!(vendor_b.load(Ordering::SeqCst), 0);
    assert_eq!(arb.load(Ordering::SeqCst), 0);
    assert_eq!(mini.load(Ordering::SeqCst), 0);
}
