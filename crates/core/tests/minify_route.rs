//! End-to-end (hermetic) tests for the minified-JSON output steering route
//! action (`RouteAction::minify_json`, research Phase 3.1):
//!
//! (a) an opted route → the dispatched request carries the deterministic
//!     instruction suffix; a minified-JSON response books a POSITIVE estimate
//!     on `X-TokenTrimmer-Minify-Saved-Est-Usd` (and the request_logs row)
//!     while `X-TokenTrimmer-Saved-Usd` EXCLUDES it (estimate, not headline);
//! (b) a prose response books a 0.000000 estimate (not valid JSON → no claim);
//! (c) streaming: the instruction + warning still apply (pre-dispatch), but
//!     v1 books $0 (metered only) — the streamed row carries a 0 estimate;
//! (d) an unrouted request dispatches byte-identical (no instruction) and
//!     books nothing.
//!
//! No network: a mock provider records the upstream messages and returns a
//! configurable completion, mirroring `flex_route.rs`.

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
    messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use tt_telemetry::request_logs::{InMemoryRequestLogWriter, RequestLogRow, RequestLogWriter};
use uuid::Uuid;

const MODEL: &str = "minify-model";
const INSTRUCTION_FRAGMENT: &str = "emit it minified";

/// A sizable JSON value so the pretty-vs-minified tokenizer delta is clearly
/// positive even under the chars/4 heuristic for this unknown test provider.
fn big_json_value() -> serde_json::Value {
    let items: Vec<serde_json::Value> = (0..40)
        .map(|i| json!({"id": i, "name": format!("item-{i}"), "active": i % 2 == 0}))
        .collect();
    json!({"object": "list", "total": 40, "data": items})
}

/// Mock provider: records each dispatched request's messages and returns a
/// configurable completion text.
struct MinifyRecordingProvider {
    dispatched: Arc<Mutex<Vec<Vec<Message>>>>,
    response_text: Arc<Mutex<String>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for MinifyRecordingProvider {
    fn id(&self) -> &'static str {
        "minifyrec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        [MODEL, "other-model"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "minifyrec".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 8192,
            })
            .collect()
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.dispatched.lock().unwrap().push(req.messages.clone());
        let text = self.response_text.lock().unwrap().clone();
        Ok(ChatCompletionResponse {
            id: "chatcmpl-minify".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(text)),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 200,
                total_tokens: 300,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.dispatched.lock().unwrap().push(req.messages.clone());
        let text = self.response_text.lock().unwrap().clone();
        let chunks = vec![
            Ok(ChatCompletionChunk {
                id: "chatcmpl-minify-stream".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: req.model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: Some("assistant".into()),
                        content: Some(text),
                        ..Default::default()
                    },
                    finish_reason: None,
                    extra: Default::default(),
                }],
                usage: None,
                extra: Default::default(),
            }),
            Ok(ChatCompletionChunk {
                id: "chatcmpl-minify-stream".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: req.model,
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta::default(),
                    finish_reason: Some("stop".into()),
                    extra: Default::default(),
                }],
                usage: Some(Usage {
                    prompt_tokens: 100,
                    completion_tokens: 200,
                    total_tokens: 300,
                    cached_tokens: 0,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
                extra: Default::default(),
            }),
        ];
        Ok(futures::stream::iter(chunks).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    dispatched: Arc<Mutex<Vec<Vec<Message>>>>,
    response_text: Arc<Mutex<String>>,
    log_writer: Arc<InMemoryRequestLogWriter>,
}

/// Build a gateway whose org has one MODEL-matching, action-only route with
/// `minify_json: true` (no model rewrite).
async fn harness() -> Harness {
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    let response_text = Arc::new(Mutex::new(
        serde_json::to_string(&big_json_value()).unwrap(),
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MinifyRecordingProvider {
        dispatched: Arc::clone(&dispatched),
        response_text: Arc::clone(&response_text),
        calls: Arc::new(AtomicUsize::new(0)),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let audit = InMemoryAuditWriter::new();
    let key = issue(
        &raw_store,
        &audit,
        org_id,
        "k",
        Environment::Live,
        Actor::System,
    )
    .await
    .unwrap()
    .plaintext;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(
        org_id,
        vec![Route {
            paused: false,
            id: Uuid::now_v7(),
            name: "minify-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec![MODEL.into()],
                ..Default::default()
            },
            // Action-only: no model rewrite, just the minify opt-in.
            then: RouteAction {
                target_model: Some(MODEL.into()),
                minify_json: true,
                ..Default::default()
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let log_writer = Arc::new(InMemoryRequestLogWriter::new());
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing)
            .with_request_log_writer(log_writer.clone() as Arc<dyn RequestLogWriter>),
    );
    Harness {
        app,
        key,
        dispatched,
        response_text,
        log_writer,
    }
}

fn chat_request(model: &str, bearer: &str, stream: bool) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(
            json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": "You are an API."},
                    {"role": "user", "content": "List the items."}
                ],
                "stream": stream,
            })
            .to_string(),
        ))
        .unwrap()
}

fn warnings_of(resp: &axum::http::Response<Body>) -> String {
    resp.headers()
        .get("x-tokentrimmer-warnings")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

fn header_f64(resp: &axum::http::Response<Body>, name: &str) -> f64 {
    resp.headers()[name]
        .to_str()
        .unwrap()
        .parse::<f64>()
        .unwrap()
}

/// The last dispatched request's system text (concatenated).
fn last_system_text(dispatched: &Arc<Mutex<Vec<Vec<Message>>>>) -> String {
    let calls = dispatched.lock().unwrap();
    let msgs = calls.last().expect("at least one dispatch");
    msgs.iter()
        .filter_map(|m| match m {
            Message::System {
                content: MessageContent::Text(t),
            } => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait for the fire-and-forget request-log task, then snapshot the rows.
async fn rows_after(writer: &Arc<InMemoryRequestLogWriter>, n: usize) -> Vec<RequestLogRow> {
    for _ in 0..100 {
        let rows = writer.rows();
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    writer.rows()
}

/// (a) Opted route + minified-JSON response: instruction dispatched, warning
/// emitted, POSITIVE estimate on its own header + row, headline saved-usd
/// EXCLUDES it (action-only route: cost == baseline → saved 0).
#[tokio::test]
async fn minify_route_books_positive_estimate_for_json_response() {
    let h = harness().await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(MODEL, &h.key, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The dispatched system prompt carries the deterministic instruction.
    let sys = last_system_text(&h.dispatched);
    assert!(
        sys.contains(INSTRUCTION_FRAGMENT),
        "dispatched system prompt must carry the minify instruction: {sys:?}"
    );

    // Warnings token.
    let warnings = warnings_of(&resp);
    assert!(
        warnings.split(',').any(|w| w == "output_minified"),
        "expected output_minified, got {warnings:?}"
    );

    // The estimate is positive on its OWN header; the invoice-reconciled
    // headline excludes it entirely (same-model action route → saved == 0).
    let est = header_f64(&resp, "x-tokentrimmer-minify-saved-est-usd");
    assert!(
        est > 0.0,
        "minified JSON response must book a positive estimate"
    );
    let saved = header_f64(&resp, "x-tokentrimmer-saved-usd");
    assert_eq!(
        saved, 0.0,
        "the ESTIMATE must never enter the saved-usd headline"
    );

    // The request_logs row carries the estimate in its own column.
    let rows = rows_after(&h.log_writer, 1).await;
    assert_eq!(rows.len(), 1);
    assert!(
        (rows[0].minify_saved_est_usd - est).abs() < 1e-9,
        "row estimate ({}) must equal the header ({est})",
        rows[0].minify_saved_est_usd
    );
}

/// (b) A prose (non-JSON) response books a 0.000000 estimate — no claim.
#[tokio::test]
async fn minify_route_books_zero_for_prose_response() {
    let h = harness().await;
    *h.response_text.lock().unwrap() = "The list contains forty items in total.".to_string();

    let resp = h
        .app
        .clone()
        .oneshot(chat_request(MODEL, &h.key, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Instruction + warning still apply (the steering happened)…
    assert!(last_system_text(&h.dispatched).contains(INSTRUCTION_FRAGMENT));
    assert!(warnings_of(&resp)
        .split(',')
        .any(|w| w == "output_minified"));

    // …but a non-JSON answer books ZERO.
    let est = header_f64(&resp, "x-tokentrimmer-minify-saved-est-usd");
    assert_eq!(est, 0.0, "prose response must book 0 (no claim)");
    let rows = rows_after(&h.log_writer, 1).await;
    assert_eq!(rows[0].minify_saved_est_usd, 0.0);
}

/// (c) Streaming: the instruction is dispatched and the warning emitted
/// (pre-dispatch shaping), but v1 books $0 — the streamed row carries 0.
#[tokio::test]
async fn minify_route_streaming_injects_but_books_zero() {
    let h = harness().await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(MODEL, &h.key, true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(
        last_system_text(&h.dispatched).contains(INSTRUCTION_FRAGMENT),
        "streaming dispatch must carry the instruction too"
    );
    assert!(
        warnings_of(&resp)
            .split(',')
            .any(|w| w == "output_minified"),
        "streaming response must carry the warning"
    );

    // Drain the body so the DropGuard writes the row.
    let _ = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let rows = rows_after(&h.log_writer, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].minify_saved_est_usd, 0.0,
        "streaming books $0 in v1 (metered only)"
    );
}

/// (d) An unrouted request dispatches byte-identical: no instruction, no
/// warning, 0.000000 estimate.
#[tokio::test]
async fn unrouted_request_is_untouched() {
    let h = harness().await;
    // `other-model` matches no route; the registry resolves any model to the
    // single mock provider, so dispatch still succeeds.
    let resp = h
        .app
        .clone()
        .oneshot(chat_request("other-model", &h.key, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let sys = last_system_text(&h.dispatched);
    assert!(
        !sys.contains(INSTRUCTION_FRAGMENT),
        "unrouted dispatch must not carry the instruction: {sys:?}"
    );
    assert!(
        !warnings_of(&resp).contains("output_minified"),
        "no warning on unrouted traffic"
    );
    let est = header_f64(&resp, "x-tokentrimmer-minify-saved-est-usd");
    assert_eq!(est, 0.0);
    let rows = rows_after(&h.log_writer, 1).await;
    assert_eq!(rows[0].minify_saved_est_usd, 0.0);
}
