//! SSE streaming helper for `/v1/chat/completions` with `stream: true`.
//!
//! Wraps a `BoxStream<ChatCompletionChunk>` as an Axum [`Sse`] response,
//! following the OpenAI SSE convention (JSON chunks terminated with `data: [DONE]`).
//!
//! ## Partial-cost accounting
//!
//! [`stream_response`] wraps the provider stream in a [`UsageTrackingStream`]
//! that accumulates token usage as chunks flow through.  A [`DropGuard`] is
//! attached to the [`Sse`] response body; when the body is dropped (clean
//! completion **or** client-abort), the guard fires a closure that writes a
//! `request_logs` row with the partial usage.  The row carries
//! `truncated = true` when no `finish_reason` chunk was observed before drop.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::response::{
    sse::{Event, KeepAlive},
    IntoResponse, Response, Sse,
};
use chrono::Utc;
use futures::stream::{BoxStream, Stream, StreamExt};
use uuid::Uuid;

use tt_shared::{
    messages::{ChunkChoice, ChunkDelta, Message, MessageContent},
    ChatCompletionChunk, ChatCompletionResponse, ModelPricing, Provider, ProviderError,
};
use tt_telemetry::request_logs::{RequestLogRow, RequestLogWriter};

// ─── PartialUsage ─────────────────────────────────────────────────────────────

/// Token counts accumulated while the SSE stream is in flight.
#[derive(Debug, Clone, Default)]
pub struct PartialUsage {
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cached_tokens: i32,
}

// ─── UsageTrackingStream ──────────────────────────────────────────────────────

/// A stream adaptor that wraps a provider SSE stream and accumulates usage.
///
/// - On each content delta it bumps `output_tokens` by the **byte** length of
///   the delta text (cheap O(1) proxy when no terminal usage block is present).
/// - When a terminal chunk carries a `usage` block the authoritative counts
///   overwrite the byte-count estimate.
/// - Sets `finished` when any choice carries a `finish_reason`.
pub(crate) struct UsageTrackingStream {
    inner: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
    /// Byte-count estimate of output tokens; overwritten by authoritative block.
    output_bytes: i32,
    input_tokens: i32,
    cached_tokens: i32,
    /// Authoritative usage from the provider's terminal chunk.
    authoritative: Option<(i32, i32, i32)>,
    /// True once any `finish_reason` chunk has been observed.
    pub(crate) finished: bool,
}

impl UsageTrackingStream {
    pub(crate) fn new(
        inner: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
        input_tokens: i32,
        cached_tokens: i32,
    ) -> Self {
        Self {
            inner,
            output_bytes: 0,
            input_tokens,
            cached_tokens,
            authoritative: None,
            finished: false,
        }
    }

    pub(crate) fn snapshot(&self) -> PartialUsage {
        if let Some((input, output, cached)) = self.authoritative {
            PartialUsage {
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
            }
        } else {
            PartialUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_bytes,
                cached_tokens: self.cached_tokens,
            }
        }
    }
}

impl Stream for UsageTrackingStream {
    type Item = Result<ChatCompletionChunk, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(ref chunk))) = poll {
            // Track finish_reason.
            if chunk.choices.iter().any(|c| c.finish_reason.is_some()) {
                self.finished = true;
            }
            // Accumulate output byte count from content deltas.
            for choice in &chunk.choices {
                if let Some(ref content) = choice.delta.content {
                    self.output_bytes = self.output_bytes.saturating_add(content.len() as i32);
                }
            }
            // Authoritative usage from terminal chunk overrides byte count.
            if let Some(ref usage) = chunk.usage {
                self.authoritative = Some((
                    usage.prompt_tokens as i32,
                    usage.completion_tokens as i32,
                    usage.cached_tokens as i32,
                ));
            }
        }
        poll
    }
}

// ─── DropGuard ────────────────────────────────────────────────────────────────

/// A value that runs a closure on [`Drop`].  Attached alongside the SSE body
/// so it fires whether the stream ends cleanly or the client aborts.
struct DropGuard {
    on_drop: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl DropGuard {
    fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self {
            on_drop: Some(Box::new(f)),
        }
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

// ─── LogContext ───────────────────────────────────────────────────────────────

/// Caller-supplied metadata needed to construct the `request_logs` row when
/// the SSE stream terminates.
pub struct StreamLogContext {
    pub writer: Arc<dyn RequestLogWriter>,
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub trace_id: Uuid,
    pub provider_id: String,
    pub model: String,
    pub input_tokens: i32,
    pub cached_tokens: i32,
    pub pricing: Option<ModelPricing>,
    pub route_id: Option<Uuid>,
    pub tag: Option<String>,
    pub request_started: Instant,
}

// ─── TrackedEventStream ───────────────────────────────────────────────────────

/// Drives the `Arc<Mutex<UsageTrackingStream>>` as a stream of SSE events.
/// Emits `[DONE]` as the final item when the inner stream is exhausted.
struct TrackedEventStream {
    inner: Arc<std::sync::Mutex<UsageTrackingStream>>,
    /// Set to true after we have emitted the `[DONE]` sentinel.
    done: bool,
}

impl Stream for TrackedEventStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        let poll = {
            let mut guard = self.inner.lock().expect("tracking stream mutex poisoned");
            Pin::new(&mut *guard).poll_next(cx)
        };
        match poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(Some(Ok(Event::default().data("[DONE]"))))
            }
            Poll::Ready(Some(result)) => {
                let json = match result {
                    Ok(chunk) => serde_json::to_string(&chunk)
                        .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}")),
                    Err(e) => format!(
                        "{{\"error\":{{\"message\":{:?},\"type\":\"upstream_error\"}}}}",
                        e.to_string()
                    ),
                };
                Poll::Ready(Some(Ok(Event::default().data(json))))
            }
        }
    }
}

// ─── GuardedStream ────────────────────────────────────────────────────────────

/// Wraps an inner stream and keeps a [`DropGuard`] alive until the stream is
/// dropped — which happens when the Axum response body is dropped.
struct GuardedStream<S> {
    inner: S,
    _guard: DropGuard,
}

impl<S: Stream + Unpin> Stream for GuardedStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

// ─── stream_response ──────────────────────────────────────────────────────────

/// Convert a streaming provider response into an Axum SSE [`Response`].
///
/// Each chunk is serialized to JSON and emitted as `data: <json>`.
/// On stream error, emits `data: {"error":{"message":"…","type":"upstream_error"}}` then ends.
/// Terminates with `data: [DONE]` per OpenAI convention.
///
/// Sets `X-TokenTrimmer-Trace-Id` and `X-TokenTrimmer-Provider` on the response.
///
/// When `log_ctx` is `Some`, wraps the stream in a [`UsageTrackingStream`] and
/// attaches a [`DropGuard`] that writes a `request_logs` row on drop.
pub fn stream_response(
    stream: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
    provider: &Arc<dyn Provider>,
    trace_id: Uuid,
    log_ctx: Option<StreamLogContext>,
) -> Response {
    let provider_id = provider.id().to_string();
    let trace_id_str = trace_id.to_string();

    let response = match log_ctx {
        None => {
            // Simple passthrough — no telemetry.
            let event_stream = build_event_stream(stream);
            Sse::new(event_stream)
                .keep_alive(KeepAlive::default())
                .into_response()
        }
        Some(ctx) => {
            // Wrap stream to accumulate usage.
            let tracking = UsageTrackingStream::new(stream, ctx.input_tokens, ctx.cached_tokens);

            // Arc<Mutex> lets the guard closure read the final accumulated state
            // after the stream has been drained (or dropped mid-way).
            let shared = Arc::new(std::sync::Mutex::new(tracking));
            let shared_for_guard = Arc::clone(&shared);

            let event_stream = TrackedEventStream {
                inner: Arc::clone(&shared),
                done: false,
            };

            // Capture everything the guard closure needs.
            let writer = ctx.writer.clone();
            let org_id = ctx.org_id;
            let api_key_id = ctx.api_key_id;
            let provider_id_log = ctx.provider_id.clone();
            let model = ctx.model.clone();
            let pricing = ctx.pricing.clone();
            let route_id = ctx.route_id;
            let tag = ctx.tag.clone();
            let request_started = ctx.request_started;
            let log_trace_id = ctx.trace_id;

            let guard = DropGuard::new(move || {
                let inner = shared_for_guard
                    .lock()
                    .expect("tracking stream mutex poisoned");
                let usage = inner.snapshot();
                let truncated = !inner.finished;
                drop(inner);

                let cost_usd = compute_streaming_cost(&usage, pricing.as_ref());
                let baseline_cost_usd = compute_streaming_baseline(&usage, pricing.as_ref());

                let row = RequestLogRow {
                    id: Uuid::now_v7(),
                    org_id,
                    api_key_id,
                    ts: Utc::now(),
                    provider: provider_id_log,
                    model,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cached_tokens: usage.cached_tokens,
                    cost_usd,
                    baseline_cost_usd,
                    cached: false,
                    cache_layer: None,
                    route_id,
                    latency_ms: request_started.elapsed().as_millis().min(i32::MAX as u128) as i32,
                    upstream_latency_ms: None,
                    status: 200,
                    tag,
                    error_class: None,
                    trace_id: Some(log_trace_id.to_string()),
                    truncated,
                };

                let writer_clone = writer.clone();
                tokio::spawn(async move {
                    if let Err(e) = writer_clone.write(row).await {
                        tracing::warn!(error = %e, "sse request_logs write failed");
                    }
                });
            });

            let guarded_stream = GuardedStream {
                inner: event_stream,
                _guard: guard,
            };

            Sse::new(guarded_stream)
                .keep_alive(KeepAlive::default())
                .into_response()
        }
    };

    attach_sse_headers(response, &trace_id_str, &provider_id)
}

// ─── helpers ─────────────────────────────────────────────────────────────────

fn build_event_stream(
    stream: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream
        .map(|result| {
            let json = match result {
                Ok(chunk) => serde_json::to_string(&chunk)
                    .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}")),
                Err(e) => format!(
                    "{{\"error\":{{\"message\":{:?},\"type\":\"upstream_error\"}}}}",
                    e.to_string()
                ),
            };
            Ok::<_, Infallible>(Event::default().data(json))
        })
        .chain(futures::stream::once(async {
            Ok::<_, Infallible>(Event::default().data("[DONE]"))
        }))
}

fn attach_sse_headers(mut response: Response, trace_id_str: &str, provider_id: &str) -> Response {
    let headers = response.headers_mut();
    if let Ok(v) = trace_id_str.parse() {
        headers.insert("x-tokentrimmer-trace-id", v);
    }
    if let Ok(v) = provider_id.parse() {
        headers.insert("x-tokentrimmer-provider", v);
    }
    response
}

/// Compute actual cost from partial usage (applying cached-token discount).
fn compute_streaming_cost(usage: &PartialUsage, pricing: Option<&ModelPricing>) -> f64 {
    let Some(pricing) = pricing else {
        return 0.0;
    };
    let cached = usage.cached_tokens.min(usage.input_tokens);
    let non_cached = usage.input_tokens.saturating_sub(cached);
    let cached_rate = pricing
        .cached_input_per_million
        .unwrap_or(pricing.input_per_million);
    (non_cached as f64) * pricing.input_per_million / 1_000_000.0
        + (cached as f64) * cached_rate / 1_000_000.0
        + (usage.output_tokens as f64) * pricing.output_per_million / 1_000_000.0
}

/// Compute baseline cost (no cache discount) from partial usage.
fn compute_streaming_baseline(usage: &PartialUsage, pricing: Option<&ModelPricing>) -> f64 {
    let Some(pricing) = pricing else {
        return 0.0;
    };
    (usage.input_tokens as f64) * pricing.input_per_million / 1_000_000.0
        + (usage.output_tokens as f64) * pricing.output_per_million / 1_000_000.0
}

// ─── fake_stream_from_response ────────────────────────────────────────────────

/// Build a synthetic streaming response from a cached
/// [`ChatCompletionResponse`]. Used by `w7-fake-stream-cache` — when an
/// L1/L2 hit lands on a request with `stream: true`, we don't have a
/// real upstream stream to forward, so we synthesize one matching the
/// OpenAI SSE format the client expects.
///
/// Three chunks before `[DONE]`:
///
/// 1. `delta.role = "assistant"` — primes clients that switch on role.
/// 2. `delta.content = <full assistant text>` — single content chunk
///    carrying the whole response. Splitting into N small "typing"
///    chunks would add complexity for no behavioural win — clients
///    re-assemble by appending deltas regardless.
/// 3. `finish_reason` + the cached usage — matches OpenAI's
///    stream-with-usage shape so client SDKs can read counts off the
///    terminator.
pub fn fake_stream_from_response(
    response: ChatCompletionResponse,
) -> BoxStream<'static, Result<ChatCompletionChunk, ProviderError>> {
    let id = response.id.clone();
    let model = response.model.clone();
    let created = response.created;
    let usage = response.usage.clone();

    let assistant_text = response
        .choices
        .first()
        .and_then(|c| match &c.message {
            Message::Assistant { content, .. } => match content {
                Some(MessageContent::Text(s)) => Some(s.clone()),
                Some(MessageContent::Parts(parts)) => parts.iter().find_map(|p| match p {
                    tt_shared::ContentPart::Text { text } => Some(text.clone()),
                    _ => None,
                }),
                None => None,
            },
            _ => None,
        })
        .unwrap_or_default();
    let finish_reason = response
        .choices
        .first()
        .and_then(|c| c.finish_reason.clone())
        .unwrap_or_else(|| "stop".into());

    let role_chunk = ChatCompletionChunk {
        id: id.clone(),
        object: "chat.completion.chunk".into(),
        created,
        model: model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: Some("assistant".into()),
                content: None,
                tool_calls: Vec::new(),
            },
            finish_reason: None,
        }],
        usage: None,
    };

    let content_chunk = ChatCompletionChunk {
        id: id.clone(),
        object: "chat.completion.chunk".into(),
        created,
        model: model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: Some(assistant_text),
                tool_calls: Vec::new(),
            },
            finish_reason: None,
        }],
        usage: None,
    };

    let finish_chunk = ChatCompletionChunk {
        id,
        object: "chat.completion.chunk".into(),
        created,
        model,
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some(finish_reason),
        }],
        usage: Some(usage),
    };

    futures::stream::iter(vec![Ok(role_chunk), Ok(content_chunk), Ok(finish_chunk)]).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::{
        messages::{Choice, Message, MessageContent},
        Usage,
    };

    fn cached_response(text: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl-cached".into(),
            object: "chat.completion".into(),
            created: 1000,
            model: "gpt-4o-mini".into(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(text.into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 4,
                total_tokens: 9,
                cached_tokens: 5,
                cache_creation_input_tokens: None,
            },
        }
    }

    #[tokio::test]
    async fn fake_stream_emits_role_content_finish() {
        let stream = fake_stream_from_response(cached_response("Hello!"));
        let chunks: Vec<ChatCompletionChunk> =
            stream.filter_map(|r| async { r.ok() }).collect().await;

        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0].choices[0].delta.role.as_deref(),
            Some("assistant")
        );
        assert_eq!(chunks[0].choices[0].delta.content, None);
        assert_eq!(chunks[1].choices[0].delta.role, None);
        assert_eq!(
            chunks[1].choices[0].delta.content.as_deref(),
            Some("Hello!")
        );
        assert_eq!(chunks[2].choices[0].finish_reason.as_deref(), Some("stop"));
        assert!(chunks[2].usage.is_some());
        assert_eq!(chunks[2].usage.as_ref().unwrap().total_tokens, 9);
    }

    #[tokio::test]
    async fn fake_stream_handles_empty_content() {
        let mut resp = cached_response("");
        resp.choices[0].message = Message::Assistant {
            content: None,
            tool_calls: vec![],
            name: None,
        };
        let stream = fake_stream_from_response(resp);
        let chunks: Vec<_> = stream.filter_map(|r| async { r.ok() }).collect().await;
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn usage_tracking_accumulates_bytes_then_authoritative() {
        let chunks = vec![
            Ok(ChatCompletionChunk {
                id: "x".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: "m".into(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: None,
                        content: Some("hello ".into()),
                        tool_calls: vec![],
                    },
                    finish_reason: None,
                }],
                usage: None,
            }),
            Ok(ChatCompletionChunk {
                id: "x".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: "m".into(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta::default(),
                    finish_reason: Some("stop".into()),
                }],
                usage: Some(tt_shared::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 2,
                    cache_creation_input_tokens: None,
                }),
            }),
        ];
        let stream = futures::stream::iter(chunks).boxed();
        let mut tracker = UsageTrackingStream::new(stream, 10, 2);
        let _ = tracker.next().await;
        let _ = tracker.next().await;
        let usage = tracker.snapshot();
        // Authoritative block: 10 input, 5 output, 2 cached.
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cached_tokens, 2);
        assert!(tracker.finished);
    }
}
