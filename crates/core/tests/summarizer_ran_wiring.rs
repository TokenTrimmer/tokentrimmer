//! C01: the summarizer-evidence wiring — a run whose route opts into the
//! agentic context budget (`elide_stale_tools`) must stamp
//! `summarizer_ran = true` on every TURN's `request_logs` row (the step
//! executed), while a plain run keeps `false`. The `$0 tax on a ran row`
//! combination is the honest "unmetered / nothing summarized" signal
//! migration 0051 exists for — never a phantom "free".

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::{json, Value};
use tower::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    AgenticBudget, CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions,
    RoutingStore,
};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use tt_telemetry::request_logs::{InMemoryRequestLogWriter, RequestLogRow};
use uuid::Uuid;

#[derive(Default)]
struct StaticProvider(Mutex<Vec<String>>);

#[async_trait]
impl Provider for StaticProvider {
    fn id(&self) -> &'static str {
        "static"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "work-model".into(),
            provider: "static".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 1024,
        }]
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
            verified_at: None,
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.0.lock().unwrap().push(req.model.clone());
        Ok(serde_json::from_value(json!({
            "id": "static", "object": "chat.completion", "created": 0, "model": req.model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        }))
        .unwrap())
    }
    async fn chat_completion_stream(
        &self,
        _: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        unreachable!("agent runs are non-streaming")
    }
    async fn embeddings(
        &self,
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        unreachable!("no embeddings")
    }
}

fn agentic_route(with_budget: bool) -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "agentic".into(),
        priority: 1,
        enabled: true,
        paused: false,
        when: RouteConditions::default(),
        then: RouteAction {
            target_model: Some("work-model".into()),
            agentic_budget: with_budget.then_some(AgenticBudget {
                cache_prefix: false,
                elide_stale_tools: true,
                keep_recent_pairs: 1,
                clear_at_least_tokens: 0,
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    writer: Arc<InMemoryRequestLogWriter>,
}

async fn harness(with_budget: bool) -> Harness {
    let org = Uuid::now_v7();
    let keys = Arc::new(InMemoryKeyStore::new());
    let key = issue(
        keys.as_ref(),
        &InMemoryAuditWriter::new(),
        org,
        "c01",
        Environment::Live,
        Actor::System,
    )
    .await
    .unwrap()
    .plaintext;
    let provider = Arc::new(StaticProvider::default());
    let mut registry = ProviderRegistry::new();
    registry.register(provider);
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, vec![agentic_route(with_budget)]);
    let routing = Arc::new(CachingRoutingStore::with_ttl(
        backing as Arc<dyn RoutingStore>,
        Duration::ZERO,
    ));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(keys)
            .with_routing_store(routing)
            .with_request_log_writer(writer.clone()),
    );
    Harness { app, key, writer }
}

fn run_req(h: &Harness) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/agent/runs")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(
            json!({
                "model": "work-model",
                "messages": [{ "role": "user", "content": "hi" }],
                "max_turns": 1,
            })
            .to_string(),
        ))
        .unwrap()
}

async fn await_rows(writer: &InMemoryRequestLogWriter) -> Vec<RequestLogRow> {
    for _ in 0..100 {
        let rows = writer.rows();
        if !rows.is_empty() {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no request_logs row was written");
}

#[tokio::test]
async fn agentic_budget_run_stamps_summarizer_ran_on_turn_row() {
    let h = harness(true).await;
    let response = h.app.clone().oneshot(run_req(&h)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let run: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(run["status"].as_str(), Some("completed"), "{run}");

    let rows = await_rows(&h.writer).await;
    assert!(!rows.is_empty(), "turn row must be written");
    for row in rows {
        // The summarizer STEP ran (the route opted into elide_stale_tools).
        // With the default NeverCommit gate nothing summarizes — the honest
        // combination is ran=true with a $0 tax, the exact "unmetered /
        // nothing-eligible" signal migration 0051 distinguishes from
        // "did not run" (false).
        assert!(row.summarizer_ran, "turn row must flag the step ran");
    }
}

#[tokio::test]
async fn plain_run_keeps_summarizer_ran_false() {
    let h = harness(false).await;
    let response = h.app.clone().oneshot(run_req(&h)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let run: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(run["status"].as_str(), Some("completed"), "{run}");

    let rows = await_rows(&h.writer).await;
    for row in rows {
        assert!(
            !row.summarizer_ran,
            "default-path rows must keep summarizer_ran=false"
        );
        assert_eq!(row.summarizer_tax_usd, 0.0);
    }
}
