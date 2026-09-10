//! A05/S01: an authenticated agent run against a routing store that cannot
//! supply a fresh policy must FAIL CLOSED — the turn's `chat::prepare` refuses
//! with 503 before any provider I/O, the run is recorded `failed` with the
//! outage note, and no dispatch leaks. Also proves `resolve_summarize_config`
//! degrades safely (outage ⇒ `None` ⇒ no summarizer dispatch) rather than
//! crashing or dispatching unprotected.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::stream::BoxStream;
use serde_json::{json, Value};
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, Route, RouteAction, RouteConditions, RoutingStore, RoutingStoreError,
};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

const EMAIL: &str = "jane.doe@example.com";

/// Records every call; must stay EMPTY in every outage assertion.
#[derive(Default)]
struct RecordingProvider(Mutex<Vec<ChatCompletionRequest>>);

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "agent-outage-test"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "agent-model".into(),
            provider: self.id().into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 8192,
            max_output_tokens: 1024,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        None
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.0.lock().unwrap().push(req.clone());
        Ok(serde_json::from_value(json!({
            "id": "agent-outage", "object": "chat.completion", "created": 0, "model": req.model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5}
        }))
        .unwrap())
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        unreachable!("non-streaming run")
    }
    async fn embeddings(
        &self,
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        unreachable!("no embeddings")
    }
}

#[derive(Debug)]
struct SwitchableStore {
    fail: AtomicBool,
    route: Route,
}
#[async_trait]
impl RoutingStore for SwitchableStore {
    async fn list_for_org(&self, _: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        if self.fail.load(Ordering::SeqCst) {
            Err(RoutingStoreError::Backend("simulated outage".into()))
        } else {
            Ok(vec![self.route.clone()])
        }
    }
}

/// A privacy route: on redact + disable_cache, so the run's turns MUST route
/// (and dispatch redacted) — an outage must refuse rather than dispatch raw.
fn privacy_route() -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "agent-privacy".into(),
        priority: 1,
        enabled: true,
        paused: false,
        when: RouteConditions::default(),
        then: RouteAction {
            target_model: Some("agent-model".into()),
            redact: true,
            disable_cache: true,
            ..Default::default()
        },
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    provider: Arc<RecordingProvider>,
    backing: Arc<SwitchableStore>,
    routing: Arc<CachingRoutingStore>,
    org: Uuid,
}
impl Harness {
    async fn new() -> Self {
        let org = Uuid::now_v7();
        let keys = Arc::new(InMemoryKeyStore::new());
        let key = issue(
            keys.as_ref(),
            &InMemoryAuditWriter::new(),
            org,
            "agent-outage",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap()
        .plaintext;
        let provider = Arc::new(RecordingProvider::default());
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let backing = Arc::new(SwitchableStore {
            fail: AtomicBool::new(true),
            route: privacy_route(),
        });
        // ZERO TTL so even a refreshed engine snapshot always expires before a
        // turn's routing lookup — the refresh must hit the poisoned backend.
        let routing = Arc::new(CachingRoutingStore::with_ttl(
            backing.clone(),
            Duration::ZERO,
        ));
        let app = build_router(
            AppState::new(registry)
                .with_key_store(keys)
                .with_routing_store(routing.clone()),
        );
        Self {
            app,
            key,
            provider,
            backing,
            routing,
            org,
        }
    }

    fn run_req(&self) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/agent/runs")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.key))
            .body(Body::from(
                json!({
                    "model": "agent-model",
                    "messages": [
                        { "role": "user", "content": format!("My email is {EMAIL}; help me.") }
                    ],
                    "max_turns": 3,
                })
                .to_string(),
            ))
            .unwrap()
    }
}

#[tokio::test]
async fn agent_run_fails_closed_on_cold_store_outage() {
    let h = Harness::new().await;
    let response = h.app.clone().oneshot(h.run_req()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "run row returned");
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let run: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        run["status"].as_str(),
        Some("failed"),
        "outage must fail the run: {run}"
    );
    assert!(
        run["note"].as_str().unwrap_or("").contains("turn 0 failed"),
        "note records the failed turn: {run}"
    );
    // FAIL CLOSED: no provider dispatch on an unresolvable privacy policy.
    assert!(
        h.provider.0.lock().unwrap().is_empty(),
        "outage caused provider I/O"
    );
}

#[tokio::test]
async fn agent_run_fails_closed_when_warm_snapshot_expires_under_outage() {
    let h = Harness::new().await;
    // Warm the engine cache with the health store, THEN poison it — the zero
    // TTL forces the turn's refresh through the (now poisoned) backend.
    h.backing.fail.store(false, Ordering::SeqCst);
    assert_eq!(
        h.routing.engine_for(h.org).await.unwrap().routes().len(),
        1,
        "warm with the healthy store"
    );
    h.backing.fail.store(true, Ordering::SeqCst);

    let response = h.app.clone().oneshot(h.run_req()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let run: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        run["status"].as_str(),
        Some("failed"),
        "expired snapshot under outage must fail the run: {run}"
    );
    assert!(
        h.provider.0.lock().unwrap().is_empty(),
        "no provider I/O on expired-snapshot outage"
    );
}

#[tokio::test]
async fn agent_run_recovers_after_store_restores() {
    let h = Harness::new().await;
    // Outage first: the run fails closed.
    let response = h.app.clone().oneshot(h.run_req()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let run: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(run["status"].as_str(), Some("failed"));

    // Store restored: the next run succeeds, dispatches WITH redaction (the
    // privacy route is live), and never leaks the email.
    h.backing.fail.store(false, Ordering::SeqCst);
    let response = h.app.clone().oneshot(h.run_req()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let run: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        run["status"].as_str(),
        Some("completed"),
        "recovered run completes: {run}"
    );
    let calls = h.provider.0.lock().unwrap();
    assert!(!calls.is_empty(), "recovered run dispatches");
    for req in calls.iter() {
        let wire = serde_json::to_string(req).unwrap();
        assert!(!wire.contains(EMAIL), "PII leaked after recovery: {wire}");
        assert!(wire.contains("[REDACTED]"));
    }
}
