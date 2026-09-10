//! R02: the bounded routing-decision outcome must land on every
//! `request_logs` row. Proves the end-to-end wiring — `apply_routing` →
//! `Prepared` → the row written by the dispatch/cache-hit paths — for all
//! four Ok-path wire names, plus NULL-for-no-store semantics.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
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
use tt_telemetry::request_logs::{InMemoryRequestLogWriter, RequestLogRow, RequestLogWriter};
use uuid::Uuid;

/// Static mock serving two models; records every dispatch.
struct StaticProvider(Mutex<Vec<String>>);

#[async_trait]
impl Provider for StaticProvider {
    fn id(&self) -> &'static str {
        "static"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["big-model", "small-model"]
            .into_iter()
            .map(|m| ModelInfo {
                id: m.into(),
                provider: "static".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: if model == "big-model" { 5.0 } else { 0.5 },
            output_per_million: if model == "big-model" { 15.0 } else { 1.5 },
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
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.0.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "static".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("ok".into())),
                    name: None,
                    tool_calls: vec![],
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        unreachable!("non-streaming R02 test")
    }
    async fn embeddings(
        &self,
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        unreachable!("no embeddings")
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    writer: Arc<InMemoryRequestLogWriter>,
}

/// Build a gateway with one route as configured. `with_store: false` builds
/// an UNROUTED app (dev/free tier) — the column must stay NULL.
async fn harness(with_store: bool, route: Option<Route>) -> Harness {
    let provider = Arc::new(StaticProvider(Mutex::new(Vec::new())));
    let mut registry = ProviderRegistry::new();
    registry.register(provider);
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let raw_store = Arc::new(InMemoryKeyStore::new());
    let org_id = Uuid::now_v7();
    let key = issue(
        raw_store.as_ref(),
        &InMemoryAuditWriter::new(),
        org_id,
        "r02",
        Environment::Live,
        Actor::System,
    )
    .await
    .unwrap()
    .plaintext;

    let mut state = AppState::new(registry)
        .with_key_store(raw_store)
        .with_request_log_writer(writer.clone());
    if with_store {
        let backing = Arc::new(InMemoryRoutingStore::new());
        backing.set_routes(org_id, route.map(|r| vec![r]).unwrap_or_default());
        state = state.with_routing_store(Arc::new(CachingRoutingStore::new(
            backing as Arc<dyn RoutingStore>,
        )));
    }
    Harness {
        app: build_router(state),
        key,
        writer,
    }
}

/// A passthrough route (model_in: big-model) with the given pause state.
fn route(paused: bool) -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "cap-passthrough".into(),
        priority: 10,
        enabled: true,
        paused,
        when: RouteConditions {
            model_in: vec!["big-model".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: Some("small-model".into()),
            ..Default::default()
        },
    }
}

/// A route whose TARGET lacks the request's required capability (vision):
/// the rewrite is suppressed but the (safety) route still matches.
fn capability_blocked_route() -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "vision-block".into(),
        priority: 10,
        enabled: true,
        paused: false,
        when: RouteConditions {
            model_in: vec!["big-model".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: Some("small-model".into()),
            ..Default::default()
        },
    }
}

fn chat(h: &Harness) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(
            json!({"model": "big-model", "messages": [{"role": "user", "content": "hello"}]})
                .to_string(),
        ))
        .unwrap()
}

fn multimodal_chat(h: &Harness) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(
            json!({
                "model": "big-model",
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "describe"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
                ]}]
            })
            .to_string(),
        ))
        .unwrap()
}

async fn await_row(writer: &InMemoryRequestLogWriter) -> RequestLogRow {
    for _ in 0..100 {
        if let Some(row) = writer.rows().into_iter().next() {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no request_logs row was written");
}

#[tokio::test]
async fn matched_route_row_carries_accepted_outcome() {
    let h = harness(true, Some(route(false))).await;
    let resp = h.app.clone().oneshot(chat(&h)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = to_bytes(resp.into_body(), 4096).await.unwrap();
    let row = await_row(&h.writer).await;
    assert_eq!(
        row.route_decision_outcome.as_deref(),
        Some("accepted_for_action_pipeline"),
        "routed request must stamp the accepted outcome"
    );
    assert!(row.route_id.is_some(), "routed row carries route_id");
}

#[tokio::test]
async fn unmatched_request_row_carries_no_match_outcome() {
    let h = harness(true, None).await;
    let resp = h.app.clone().oneshot(chat(&h)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = to_bytes(resp.into_body(), 4096).await.unwrap();
    let row = await_row(&h.writer).await;
    assert_eq!(
        row.route_decision_outcome.as_deref(),
        Some("no_match"),
        "unrouted request with a configured store must stamp no_match"
    );
    assert!(row.route_id.is_none());
}

#[tokio::test]
async fn paused_route_row_carries_paused_outcome() {
    let h = harness(true, Some(route(true))).await;
    let resp = h.app.clone().oneshot(chat(&h)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = to_bytes(resp.into_body(), 4096).await.unwrap();
    let row = await_row(&h.writer).await;
    assert_eq!(
        row.route_decision_outcome.as_deref(),
        Some("paused"),
        "sticky-paused route must stamp the paused outcome"
    );
    assert!(row.route_paused, "paused marker also set");
    assert!(row.route_id.is_some(), "paused row still attributes");
}

#[tokio::test]
async fn capability_suppressed_row_carries_suppressed_outcome() {
    let h = harness(true, Some(capability_blocked_route())).await;
    // Multimodal request on a text-only target: the rewrite is suppressed
    // (capability guard), the request passthrough-serves on the caller model.
    let resp = h.app.clone().oneshot(multimodal_chat(&h)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = to_bytes(resp.into_body(), 4096).await.unwrap();
    let row = await_row(&h.writer).await;
    assert_eq!(
        row.route_decision_outcome.as_deref(),
        Some("capability_suppressed"),
        "capability-guarded passthrough must stamp the suppressed outcome"
    );
    assert!(row.route_id.is_some());
    assert!(!row.route_paused);
}

#[tokio::test]
async fn no_routing_store_row_carries_null_outcome() {
    // Dev/free tier: no routing store at all — the column must be NULL, not
    // a guessed `no_match` (routing was never evaluated).
    let h = harness(false, None).await;
    let resp = h.app.clone().oneshot(chat(&h)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = to_bytes(resp.into_body(), 4096).await.unwrap();
    let row = await_row(&h.writer).await;
    assert_eq!(
        row.route_decision_outcome, None,
        "no-store request must leave the outcome NULL (never guessed)"
    );
}

#[test]
fn legacy_row_json_deserializes_without_outcome() {
    // Pre-0052 row JSON (no route_decision_outcome key) must deserialize
    // with `None` — the serde default contract for rolling deploys.
    let legacy = json!({
        "id": Uuid::now_v7().to_string(),
        "org_id": Uuid::now_v7().to_string(),
        "api_key_id": Uuid::now_v7().to_string(),
        "ts": "2026-09-05T00:00:00Z",
        "provider": "static",
        "model": "big-model",
        "input_tokens": 1,
        "output_tokens": 1,
        "cached_tokens": 0,
        "cost_usd": 0.0,
        "baseline_cost_usd": 0.0,
        "cached": false,
        "route_id": null,
        "latency_ms": 1,
        "status": 200
    });
    let row: RequestLogRow = serde_json::from_value(legacy).expect("legacy row must deserialize");
    assert_eq!(row.route_decision_outcome, None);
}
