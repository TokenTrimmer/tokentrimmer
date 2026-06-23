//! Integration tests for the `/metrics` Prometheus endpoint.
//!
//! The global recorder is shared across this test binary, so assertions check
//! metric/label PRESENCE, not exact counts.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;
use tt_core::{build_router, AppState, ProviderRegistry};

fn router() -> axum::Router {
    build_router(AppState::new(ProviderRegistry::new()))
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let (status, headers, body) = get(router(), "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    assert!(body.contains("tt_build_info"), "build_info missing: {body}");
    assert!(
        body.contains("process_uptime_seconds"),
        "uptime missing: {body}"
    );
}

#[tokio::test]
async fn render_is_some_after_build_router() {
    let _ = router();
    assert!(tt_core::metrics::render().is_some());
}

#[tokio::test]
async fn http_request_metrics_recorded_for_health() {
    let app = router();
    // Drive one request through the stack so the latency middleware records it.
    let (s, _h, _b) = get(app.clone(), "/health").await;
    assert_eq!(s, StatusCode::OK);
    // Now scrape.
    let (_s, _h, body) = get(app, "/metrics").await;
    assert!(
        body.contains("http_requests_total"),
        "http_requests_total missing: {body}"
    );
    assert!(
        body.contains("http_request_duration_seconds"),
        "duration histogram missing: {body}"
    );
    assert!(
        body.contains("endpoint=\"/health\""),
        "matched-path label missing: {body}"
    );
}

mod nopricing {
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use tt_shared::messages::{Choice, Message, MessageContent};
    use tt_shared::pricing::Capability;
    use tt_shared::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
        EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
        Usage,
    };

    pub struct NoPricingEcho;

    #[async_trait]
    impl Provider for NoPricingEcho {
        fn id(&self) -> &'static str {
            "nopricing"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "np-1".into(),
                provider: "nopricing".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            }]
        }
        fn pricing(&self, _: &str) -> Option<ModelPricing> {
            None
        }
        async fn chat_completion(
            &self,
            req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<ChatCompletionResponse, ProviderError> {
            Ok(ChatCompletionResponse {
                id: "chatcmpl-np".into(),
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
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
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
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            Err(ProviderError::Unsupported("n/a".into()))
        }
        async fn embeddings(
            &self,
            req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            use tt_shared::messages::EmbeddingData;
            Ok(EmbeddingsResponse {
                object: "list".into(),
                data: vec![EmbeddingData {
                    object: "embedding".into(),
                    index: 0,
                    embedding: vec![0.1, 0.2, 0.3],
                }],
                model: req.model,
                usage: Usage {
                    prompt_tokens: 5,
                    completion_tokens: 0,
                    total_tokens: 5,
                    cached_tokens: 0,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
            })
        }
    }
}

mod cachereporting {
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::stream::BoxStream;
    use tt_shared::messages::{Choice, Message, MessageContent};
    use tt_shared::pricing::Capability;
    use tt_shared::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
        EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
        Usage,
    };

    /// A provider whose usage block reports provider prompt-cache reads/writes.
    pub struct CacheReportingEcho;

    #[async_trait]
    impl Provider for CacheReportingEcho {
        fn id(&self) -> &'static str {
            "cachereporting"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "cr-1".into(),
                provider: "cachereporting".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            }]
        }
        fn pricing(&self, _: &str) -> Option<ModelPricing> {
            Some(ModelPricing {
                input_per_million: 3.0,
                output_per_million: 6.0,
                cached_input_per_million: Some(0.3),
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
                id: "chatcmpl-cr".into(),
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
                    prompt_tokens: 100,
                    completion_tokens: 10,
                    total_tokens: 110,
                    cached_tokens: 80,
                    cache_creation_input_tokens: Some(20),
                    cache_read_input_tokens: Some(80),
                },
            })
        }
        async fn chat_completion_stream(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            Err(ProviderError::Unsupported("n/a".into()))
        }
        async fn embeddings(
            &self,
            _req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Err(ProviderError::Unsupported("n/a".into()))
        }
    }
}

mod streamnousage {
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::stream::{BoxStream, StreamExt};
    use tt_shared::messages::{ChunkChoice, ChunkDelta};
    use tt_shared::pricing::Capability;
    use tt_shared::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
        EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
    };

    /// A streaming provider that finishes cleanly (finish_reason chunk) but
    /// never sends a terminal usage block — like an OpenAI-compat upstream
    /// streaming without `include_usage`.
    pub struct StreamNoUsageEcho;

    #[async_trait]
    impl Provider for StreamNoUsageEcho {
        fn id(&self) -> &'static str {
            "streamnousage"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "snu-1".into(),
                provider: "streamnousage".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            }]
        }
        fn pricing(&self, _: &str) -> Option<ModelPricing> {
            Some(ModelPricing {
                input_per_million: 3.0,
                output_per_million: 6.0,
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
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<ChatCompletionResponse, ProviderError> {
            Err(ProviderError::Unsupported("n/a".into()))
        }
        async fn chat_completion_stream(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            let content = ChatCompletionChunk {
                id: "id".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: "snu-1".into(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: None,
                        content: Some("ok".into()),
                        tool_calls: vec![],
                        extra: Default::default(),
                    },
                    finish_reason: None,
                    extra: Default::default(),
                }],
                usage: None,
                extra: Default::default(),
            };
            let finish = ChatCompletionChunk {
                id: "id".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: "snu-1".into(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta::default(),
                    finish_reason: Some("stop".into()),
                    extra: Default::default(),
                }],
                usage: None, // clean finish, but NO terminal usage block
                extra: Default::default(),
            };
            Ok(futures::stream::iter(vec![Ok(content), Ok(finish)]).boxed())
        }
        async fn embeddings(
            &self,
            _req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Err(ProviderError::Unsupported("n/a".into()))
        }
    }
}

/// A cleanly-finished stream whose provider never sent a terminal usage block
/// must still count toward the per-route denominator as result="unreported"
/// ("unreported" is a fact, not an estimate), matching the non-streaming
/// path. Token counters must NOT appear for this provider — no estimates.
#[tokio::test]
async fn streaming_clean_finish_without_usage_counts_unreported() {
    use std::sync::Arc;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(streamnousage::StreamNoUsageEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({
        "model": "snu-1",
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": true
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Drain the SSE body so the DropGuard fires (counters increment there).
    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    let line = metrics_body
        .lines()
        .find(|l| {
            l.starts_with("provider_cache_requests_total")
                && l.contains("provider=\"streamnousage\"")
        })
        .unwrap_or_else(|| {
            panic!("no provider_cache_requests_total line for streamnousage:\n{metrics_body}")
        });
    assert!(
        line.contains("result=\"unreported\""),
        "expected result=\"unreported\", got: {line}"
    );
    // No token counters from an unreported stream (estimates never counted).
    assert!(
        !metrics_body.lines().any(|l| {
            (l.starts_with("provider_cache_read_tokens_total")
                || l.starts_with("provider_cache_write_tokens_total"))
                && l.contains("provider=\"streamnousage\"")
        }),
        "token counters must not exist for an unreported stream:\n{metrics_body}"
    );
}

/// A dispatch whose provider reports prompt-cache reads/writes surfaces the
/// per-route provider-cache counters on /metrics (presence-only — the
/// recorder is shared across this test binary).
#[tokio::test]
async fn provider_cache_usage_counters_recorded() {
    use std::sync::Arc;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(cachereporting::CacheReportingEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({
        "model": "cr-1",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    assert!(
        metrics_body.contains("provider_cache_read_tokens_total"),
        "cache read counter missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("provider_cache_write_tokens_total"),
        "cache write counter missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("provider_cache_requests_total"),
        "cache requests counter missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("result=\"hit\""),
        "hit result label missing: {metrics_body}"
    );
}

/// P2 (metering durability): the synchronous `tt_requests_served_total`
/// served-counter must be emitted IN-BAND on a non-streaming chat DISPATCH —
/// the cheap sync truth to diff against the async-written `request_logs` row.
#[tokio::test]
async fn served_counter_recorded_for_chat_dispatch() {
    use std::sync::Arc;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(nopricing::NoPricingEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({
        "model": "np-1",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    let line = metrics_body
        .lines()
        .find(|l| l.starts_with("tt_requests_served_total") && l.contains("path=\"chat\""))
        .unwrap_or_else(|| panic!("no chat tt_requests_served_total line:\n{metrics_body}"));
    assert!(
        line.contains("result=\"dispatch\""),
        "chat dispatch must be labelled result=dispatch: {line}"
    );
}

/// P2: the served-counter fires for a streaming chat DISPATCH (the SSE path),
/// labelled `path="sse"`, once the response is built.
#[tokio::test]
async fn served_counter_recorded_for_sse_dispatch() {
    use std::sync::Arc;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(streamnousage::StreamNoUsageEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({
        "model": "snu-1",
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": true
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    assert!(
        metrics_body
            .lines()
            .any(|l| l.starts_with("tt_requests_served_total") && l.contains("path=\"sse\"")),
        "sse served-counter line missing:\n{metrics_body}"
    );
}

/// P2: the served-counter fires for an embeddings DISPATCH, labelled
/// `path="embeddings"`.
#[tokio::test]
async fn served_counter_recorded_for_embeddings_dispatch() {
    use std::sync::Arc;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(nopricing::NoPricingEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({ "model": "np-1", "input": "hello" });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    assert!(
        metrics_body
            .lines()
            .any(|l| l.starts_with("tt_requests_served_total") && l.contains("path=\"embeddings\"")),
        "embeddings served-counter line missing:\n{metrics_body}"
    );
}

/// P2 (agent-loop metering): `tt_requests_served_total{path="agent_run",result="dispatch"}`
/// must increment ONCE PER DISPATCHED TURN of a server-side agent run.
///
/// Strategy: drive a 2-turn `/v1/agent/runs` (turn-1 returns a gateway `find_route_for`
/// tool call → executed inline → turn-2 returns a final answer), then assert the
/// `path="agent_run"` served-counter advanced by exactly 2 (one per dispatched turn).
///
/// The global recorder is shared across the binary, so we use MONOTONIC DELTAS
/// (scrape before → scrape after → assert delta == 2) to avoid flakiness from
/// tests that ran earlier in the binary.
#[tokio::test]
async fn served_counter_increments_per_agent_run_turn() {
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::stream::BoxStream;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };
    use tt_shared::messages::{Choice, Message, MessageContent, ToolCall, ToolCallFunction};
    use tt_shared::pricing::Capability;
    use tt_shared::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
        EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
        Usage,
    };

    /// A scripted mock provider: call 1 returns a `find_route_for` tool call (a
    /// read-only gateway tool, executed inline by the loop → forces turn 2);
    /// call 2 returns a final text answer (no tool calls → run completes).
    struct ScriptedAgentProvider {
        call: AtomicU32,
    }

    #[async_trait]
    impl Provider for ScriptedAgentProvider {
        fn id(&self) -> &'static str {
            "scripted_agent"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "sa-1".into(),
                provider: "scripted_agent".into(),
                // Turn-2 messages contain an Assistant tool_call + Tool result,
                // so RequiredCapabilities derives `tools=true` → must declare Tools.
                capabilities: vec![Capability::Text, Capability::Tools],
                max_input_tokens: 100_000,
                max_output_tokens: 8192,
            }]
        }
        fn pricing(&self, _: &str) -> Option<ModelPricing> {
            Some(ModelPricing {
                input_per_million: 3.0,
                output_per_million: 6.0,
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
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            let message = if call == 0 {
                // Turn 1: request the read-only `find_route_for` gateway tool.
                // The agent loop executes it inline and appends the result, then
                // loops back for turn 2 (so this forces exactly one extra turn).
                Message::Assistant {
                    content: None,
                    name: None,
                    tool_calls: vec![ToolCall {
                        id: "tc1".into(),
                        r#type: "function".into(),
                        function: ToolCallFunction {
                            name: "find_route_for".into(),
                            arguments: r#"{"task_description":"summarize my usage"}"#.into(),
                        },
                    }],
                }
            } else {
                // Turn 2+: final answer (no tool calls → run completes).
                Message::Assistant {
                    content: Some(MessageContent::Text("done".into())),
                    tool_calls: vec![],
                    name: None,
                }
            };
            Ok(ChatCompletionResponse {
                id: "chatcmpl-sa".into(),
                object: "chat.completion".into(),
                created: 0,
                model: req.model,
                choices: vec![Choice {
                    index: 0,
                    message,
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
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            Err(ProviderError::Unsupported("n/a".into()))
        }
        async fn embeddings(
            &self,
            _req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Err(ProviderError::Unsupported("n/a".into()))
        }
    }

    /// Sum all `tt_requests_served_total` lines that carry BOTH
    /// `path="agent_run"` AND `result="dispatch"`. Prometheus text format:
    /// `tt_requests_served_total{path="agent_run",result="dispatch",...} <val>`.
    fn agent_run_dispatch_count(rendered: &str) -> u64 {
        rendered
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter(|l| {
                l.starts_with("tt_requests_served_total")
                    && l.contains("path=\"agent_run\"")
                    && l.contains("result=\"dispatch\"")
            })
            .filter_map(|l| {
                l.split_whitespace()
                    .nth_back(0)
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|f| f as u64)
            })
            .sum()
    }

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(ScriptedAgentProvider {
        call: AtomicU32::new(0),
    }));
    let app = build_router(AppState::new(registry));

    // Baseline scrape BEFORE the run (counter may already be >0 from prior tests).
    let before = {
        let (_s, _h, body) = get(app.clone(), "/metrics").await;
        agent_run_dispatch_count(&body)
    };

    // POST /v1/agent/runs — 2-turn run (tool call + final answer).
    let body = serde_json::json!({
        "model": "sa-1",
        "messages": [{ "role": "user", "content": "hi" }],
        "max_turns": 4
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/agent/runs")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "agent run must return 200");

    // Confirm the run really did complete with 2 turns (1 tool + 1 final).
    let run_body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let run: serde_json::Value = serde_json::from_slice(&run_body).unwrap();
    assert_eq!(
        run["status"].as_str(),
        Some("completed"),
        "run must be completed: {run}"
    );
    assert_eq!(
        run["turns"].as_u64(),
        Some(2),
        "run must have taken exactly 2 turns: {run}"
    );

    // After scrape — the delta must be exactly 2 (one per dispatched turn).
    let after = {
        let (_s, _h, body) = get(app, "/metrics").await;
        agent_run_dispatch_count(&body)
    };
    assert_eq!(
        after - before,
        2,
        "tt_requests_served_total{{path=agent_run,result=dispatch}} must have \
         incremented by exactly 2 (one per dispatched turn; before={before}, after={after})"
    );
}

#[tokio::test]
async fn provider_and_catalog_metrics_recorded() {
    use std::sync::Arc;
    use tt_auth::ApiKeyContext;
    use uuid::Uuid;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(nopricing::NoPricingEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({
        "model": "np-1",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut().insert(ApiKeyContext {
        key_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        tier: None,
    });
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    assert!(
        metrics_body.contains("provider_request_duration_seconds"),
        "provider latency missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("catalog_zero_price_total"),
        "catalog miss missing: {metrics_body}"
    );
}
