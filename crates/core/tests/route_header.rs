//! `X-TokenTrimmer-Route` forces a named route; `X-TokenTrimmer-Route-Matched`
//! reports the applied route's name.

use std::sync::Arc;

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

const MODELS: [&str; 3] = ["m1", "m2", "m3"];

struct MultiModelProvider;

#[async_trait]
impl Provider for MultiModelProvider {
    fn id(&self) -> &'static str {
        "multi"
    }
    fn models(&self) -> Vec<ModelInfo> {
        MODELS
            .iter()
            .map(|id| ModelInfo {
                id: (*id).into(),
                provider: "multi".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
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
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
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
                cache_read_input_tokens: None,
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

/// Build an app whose org has `routes`. Returns (app, key).
async fn app_with_routes(routes: Vec<Route>) -> (axum::Router, String) {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MultiModelProvider));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, routes);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );
    (app, key)
}

fn route(name: &str, priority: u32, tag: Option<&str>, target: &str) -> Route {
    Route {
        paused: false,
        id: Uuid::now_v7(),
        name: name.into(),
        priority,
        enabled: true,
        when: RouteConditions {
            tag_equals: tag.map(String::from),
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
            target_model: target.into(),
            fallbacks: vec![],
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
        },
    }
}

fn chat_req(model: &str, force_route: Option<&str>, key: &str) -> Request<Body> {
    let body =
        json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": false });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"));
    if let Some(r) = force_route {
        b = b.header("x-tokentrimmer-route", r);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn hdr(resp: &axum::http::Response<Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

#[tokio::test]
async fn forced_route_applies_ignoring_conditions() {
    // Route conditions require tag "other" (not present), so it would NOT match
    // normally. Forcing it by name applies it anyway.
    let (app, key) = app_with_routes(vec![route("force-me", 100, Some("other"), "m2")]).await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", Some("force-me"), &key))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hdr(&r, "x-tokentrimmer-model-used").as_deref(), Some("m2"));
    assert_eq!(
        hdr(&r, "x-tokentrimmer-route-matched").as_deref(),
        Some("force-me")
    );
}

#[tokio::test]
async fn forced_route_overrides_normal_match() {
    // `auto` would match (no tag condition); `manual` would not (needs tag).
    // Forcing `manual` wins.
    let (app, key) = app_with_routes(vec![
        route("auto", 100, None, "m2"),
        route("manual", 50, Some("never"), "m3"),
    ])
    .await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", Some("manual"), &key))
        .await
        .unwrap();
    assert_eq!(hdr(&r, "x-tokentrimmer-model-used").as_deref(), Some("m3"));
    assert_eq!(
        hdr(&r, "x-tokentrimmer-route-matched").as_deref(),
        Some("manual")
    );
}

#[tokio::test]
async fn unknown_forced_route_is_400() {
    let (app, key) = app_with_routes(vec![route("auto", 100, None, "m2")]).await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", Some("nope"), &key))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn condition_matched_route_emits_route_matched() {
    let (app, key) = app_with_routes(vec![route("auto", 100, None, "m2")]).await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", None, &key))
        .await
        .unwrap();
    assert_eq!(hdr(&r, "x-tokentrimmer-model-used").as_deref(), Some("m2"));
    assert_eq!(
        hdr(&r, "x-tokentrimmer-route-matched").as_deref(),
        Some("auto")
    );
}

#[tokio::test]
async fn no_route_no_header() {
    // A route that requires a tag that isn't present → no match, no header.
    let (app, key) = app_with_routes(vec![route("auto", 100, Some("never"), "m2")]).await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", None, &key))
        .await
        .unwrap();
    assert_eq!(hdr(&r, "x-tokentrimmer-model-used").as_deref(), Some("m1"));
    assert_eq!(hdr(&r, "x-tokentrimmer-route-matched"), None);
}
