//! `X-TokenTrimmer-Fallback` supplies/overrides the failover chain.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry, DOGFOOD_ORG_ID};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use uuid::Uuid;

struct MockProvider {
    id: &'static str,
    models: &'static [&'static str],
    fails: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|m| ModelInfo {
                id: (*m).to_string(),
                provider: self.id.to_string(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 128_000,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.1,
            output_per_million: 0.1,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fails {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "down".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: "chatcmpl-fb".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("served".into())),
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
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Err(ProviderError::Unsupported("no streaming".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

fn req(model: &str, fallback_header: Option<&str>) -> Request<Body> {
    let body =
        json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": false });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(f) = fallback_header {
        b = b.header("x-tokentrimmer-fallback", f);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn served_model(resp: &axum::http::Response<Body>) -> Option<String> {
    resp.headers()
        .get("x-tokentrimmer-model-used")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

#[tokio::test]
async fn fallback_header_enables_failover_without_route() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let backup_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["m-primary"],
        fails: true,
        calls: Arc::clone(&primary_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "backup",
        models: &["m-backup"],
        fails: false,
        calls: Arc::clone(&backup_calls),
    }));
    // No routing store — the header alone supplies the chain.
    let app = build_router(AppState::new(registry));

    let resp = app
        .oneshot(req("m-primary", Some("m-backup")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served_model(&resp).as_deref(), Some("m-backup"));
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("backup")
    );
    assert!(primary_calls.load(Ordering::Relaxed) >= 1, "primary tried");
    assert_eq!(
        backup_calls.load(Ordering::Relaxed),
        1,
        "backup served once"
    );
}

#[tokio::test]
async fn provider_pin_suppresses_fallback_header() {
    // A provider pin wins over X-TokenTrimmer-Fallback: the request dispatches to
    // the pinned provider with no failover (the failover path can't honor a pinned
    // primary), so the header's chain is ignored.
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let pinned_calls = Arc::new(AtomicUsize::new(0));
    let other_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["m-primary"],
        fails: false,
        calls: Arc::clone(&primary_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "pinned",
        models: &["m-pinned"],
        fails: false,
        calls: Arc::clone(&pinned_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "other",
        models: &["m-other"],
        fails: false,
        calls: Arc::clone(&other_calls),
    }));
    let app = build_router(AppState::new(registry));

    // Pin to `pinned`; also supply a fallback header → header must be ignored.
    let body = json!({ "model": "m-primary", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-provider", "pinned")
        .header("x-tokentrimmer-fallback", "m-other")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("pinned"),
        "pinned provider must serve"
    );
    assert_eq!(pinned_calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        other_calls.load(Ordering::Relaxed),
        0,
        "fallback header must be ignored under a provider pin"
    );
}

#[tokio::test]
async fn fallback_header_overrides_route_chain() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let routefb_calls = Arc::new(AtomicUsize::new(0));
    let hdrfb_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    // Primary answers the inbound `gpt-4o` (pre-routing resolve) + the routed
    // `primary-model`, and 503s.
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: true,
        calls: Arc::clone(&primary_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "routefb",
        models: &["route-fb"],
        fails: false,
        calls: Arc::clone(&routefb_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "hdrfb",
        models: &["hdr-fb"],
        fails: false,
        calls: Arc::clone(&hdrfb_calls),
    }));

    // Route rewrites gpt-4o → primary-model with its OWN fallback chain [route-fb].
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        DOGFOOD_ORG_ID,
        vec![Route {
            paused: false,
            id: Uuid::now_v7(),
            name: "r".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            then: RouteAction {
                format_switch: None,
                diff: false,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                agentic_budget: None,
                target_model: Some("primary-model".into()),
                fallbacks: vec!["route-fb".into()],
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                panel: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_routing_store(routing)
            .with_dogfood_enabled(),
    );

    // Header supplies hdr-fb → it must replace the route's [route-fb].
    let resp = app.oneshot(req("gpt-4o", Some("hdr-fb"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served_model(&resp).as_deref(), Some("hdr-fb"));
    assert_eq!(
        hdrfb_calls.load(Ordering::Relaxed),
        1,
        "header fallback served"
    );
    assert_eq!(
        routefb_calls.load(Ordering::Relaxed),
        0,
        "route's own fallback must NOT be used when the header overrides it"
    );
}
