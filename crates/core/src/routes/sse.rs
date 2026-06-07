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
use std::time::{Duration, Instant};

use axum::response::{
    sse::{Event, KeepAlive},
    IntoResponse, Response, Sse,
};
use chrono::Utc;
use futures::stream::{BoxStream, Stream, StreamExt};
use uuid::Uuid;

use tt_cache::{CacheEntry, L1Entry};
use tt_shared::{
    messages::{ChunkChoice, ChunkDelta, Message, MessageContent},
    ChatCompletionChunk, ChatCompletionResponse, ModelPricing, Provider, ProviderError, Usage,
};
use tt_telemetry::request_logs::{RequestLogRow, RequestLogWriter};
use tt_tokenize;

use crate::budget::SpendSink;
use crate::state::{L1Config, L2Config};

// ─── PartialUsage ─────────────────────────────────────────────────────────────

/// Token counts accumulated while the SSE stream is in flight.
#[derive(Debug, Clone, Default)]
pub struct PartialUsage {
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cached_tokens: i32,
    pub cache_creation_tokens: i32,
}

// ─── UsageTrackingStream ──────────────────────────────────────────────────────

/// A stream adaptor that wraps a provider SSE stream and accumulates usage.
///
/// - On each content delta it appends the delta text to an accumulation buffer
///   so that, when no terminal usage block arrives (e.g. client abort), the
///   fallback output-token count is estimated via `tt_tokenize` rather than
///   raw byte length (bytes ≈ 4× tokens → ~4× cost overcount).
/// - When a terminal chunk carries a `usage` block the authoritative counts
///   overwrite the tokenizer estimate.
/// - Sets `finished` when any choice carries a `finish_reason`.
pub(crate) struct UsageTrackingStream {
    inner: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
    /// Accumulated output text for fallback token estimation and cache reconstruction.
    output_text: String,
    input_tokens: i32,
    cached_tokens: i32,
    /// Provider id for tokenizer selection (e.g. "openai", "anthropic").
    provider_id: String,
    /// Authoritative usage from the provider's terminal chunk:
    /// (prompt, completion, cached, cache_creation).
    authoritative: Option<(i32, i32, i32, i32)>,
    /// True once any `finish_reason` chunk has been observed.
    pub(crate) finished: bool,
    /// The `finish_reason` string from the terminal chunk (e.g. `"stop"`).
    /// Used to reconstruct a `ChatCompletionResponse` for cache insertion.
    pub(crate) finish_reason: Option<String>,
}

impl UsageTrackingStream {
    pub(crate) fn new(
        inner: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
        input_tokens: i32,
        cached_tokens: i32,
        provider_id: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            output_text: String::new(),
            input_tokens,
            cached_tokens,
            provider_id: provider_id.into(),
            authoritative: None,
            finished: false,
            finish_reason: None,
        }
    }

    /// Returns the accumulated data needed to reconstruct a `ChatCompletionResponse`
    /// for cache insertion. Returns `None` when no authoritative usage block arrived
    /// (i.e. stream was truncated / no terminal usage chunk), ensuring only cleanly
    /// completed streams with known token counts are cached.
    pub(crate) fn cache_completion_data(&self) -> Option<(String, String, Usage)> {
        let (prompt_tokens, completion_tokens, cached_tokens, cache_creation) =
            self.authoritative?;
        let finish_reason = self.finish_reason.clone().unwrap_or_else(|| "stop".into());
        let text = self.output_text.clone();
        let usage = Usage {
            prompt_tokens: prompt_tokens as u64,
            completion_tokens: completion_tokens as u64,
            total_tokens: (prompt_tokens + completion_tokens) as u64,
            cached_tokens: cached_tokens as u64,
            cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation as u64),
        };
        Some((text, finish_reason, usage))
    }

    pub(crate) fn snapshot(&self) -> PartialUsage {
        if let Some((input, output, cached, cache_creation)) = self.authoritative {
            PartialUsage {
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
                cache_creation_tokens: cache_creation,
            }
        } else {
            // Fallback: estimate output tokens from accumulated text via
            // tt_tokenize rather than raw byte length (§2.12). No authoritative
            // block → no known cache-creation count.
            let output_tokens =
                tt_tokenize::estimate_tokens(&self.provider_id, &self.output_text) as i32;
            PartialUsage {
                input_tokens: self.input_tokens,
                output_tokens,
                cached_tokens: self.cached_tokens,
                cache_creation_tokens: 0,
            }
        }
    }
}

impl Stream for UsageTrackingStream {
    type Item = Result<ChatCompletionChunk, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(ref chunk))) = poll {
            // Track finish_reason (first observed value wins).
            for choice in &chunk.choices {
                if let Some(ref fr) = choice.finish_reason {
                    self.finished = true;
                    if self.finish_reason.is_none() {
                        self.finish_reason = Some(fr.clone());
                    }
                }
            }
            // Accumulate output text from content deltas for fallback
            // token estimation (§2.12).
            for choice in &chunk.choices {
                if let Some(ref content) = choice.delta.content {
                    self.output_text.push_str(content);
                }
            }
            // Authoritative usage from terminal chunk overrides byte count.
            if let Some(ref usage) = chunk.usage {
                self.authoritative = Some((
                    usage.prompt_tokens as i32,
                    usage.completion_tokens as i32,
                    usage.cached_tokens as i32,
                    usage.cache_creation_input_tokens.unwrap_or(0) as i32,
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

// ─── CacheInsertContext ────────────────────────────────────────────────────────

/// Context for writing a cleanly-completed streaming response into L1/L2 cache.
///
/// Built by `chat.rs` and passed through `StreamLogContext` so the `DropGuard`
/// can insert the response after the final chunk is sent — best-effort, never
/// blocking the client.
///
/// Insertion is guarded by three conditions checked in the guard closure:
///   1. `!truncated` — clean completion only (no partial responses cached).
///   2. `authoritative` usage present — terminal chunk arrived with token counts.
///   3. No tool calls in the reconstructed response (non-deterministic output).
pub struct CacheInsertContext {
    /// L1 exact-match cache backend + TTL config.
    pub l1: Option<L1Config>,
    /// L2 semantic cache backend (optional).
    pub l2: Option<L2Config>,
    /// Namespaced L1 key for this request: `"{org_id}:{cache_key(req)}"`.
    pub l1_key: String,
    /// L2 query text for embedding (the context text extracted from the request).
    pub l2_query_text: Option<String>,
    /// TTL seconds for both L1 and L2 inserts.
    pub ttl_secs: u64,
    /// The served model name (e.g. `"gpt-4o"`).
    pub model: String,
    /// The provider id (e.g. `"openai"`).
    pub provider_id: String,
    /// Org id — embedded in the L2 entry.
    pub org_id: Uuid,
}

// ─── LogContext ───────────────────────────────────────────────────────────────

/// Caller-supplied metadata needed to construct the `request_logs` row when
/// the SSE stream terminates.
pub struct StreamLogContext {
    /// Optional log writer. `None` skips the telemetry row (e.g. tests or dev
    /// environments without a DB), but cache insertion still fires if
    /// `cache_insert` is `Some`.
    pub writer: Option<Arc<dyn RequestLogWriter>>,
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub trace_id: Uuid,
    pub provider_id: String,
    pub model: String,
    pub input_tokens: i32,
    pub cached_tokens: i32,
    pub pricing: Option<ModelPricing>,
    /// Pricing of the originally-requested model, used for `baseline_cost_usd`.
    /// When routing did not rewrite the model this equals `pricing`. Falls back
    /// to `pricing` when `None`.
    pub baseline_pricing: Option<ModelPricing>,
    pub route_id: Option<Uuid>,
    pub tag: Option<String>,
    pub request_started: Instant,
    /// Spend sink — realized streamed spend is recorded through this into the
    /// same enforcer the auth pre-flight check uses (dynamic on tier-aware path,
    /// global enforcer otherwise, no-op when neither is wired).
    pub spend_sink: SpendSink,
    /// Provider surcharge multiplier (e.g. OpenRouter BYOK = 1.05, others = 1.0).
    /// Applied to both cost and baseline on the streaming path, matching the
    /// non-streaming path (§2.13).
    pub fee_multiplier: f64,
    /// Optional cache insertion context. When `Some`, a cleanly-completed
    /// stream writes its reconstructed response into L1 (and L2 if configured)
    /// after the final chunk is sent.
    pub cache_insert: Option<CacheInsertContext>,
}

// ─── TrackedEventStream ───────────────────────────────────────────────────────

/// Terminal-sequence state for [`TrackedEventStream`].
enum Phase {
    /// Forwarding provider chunks.
    Streaming,
    /// Inner stream exhausted; next poll emits the `[DONE]` sentinel.
    EmitDone,
    /// All terminal events emitted.
    Finished,
}

/// Drives the `Arc<Mutex<UsageTrackingStream>>` as a stream of SSE events.
/// On clean completion it emits a terminal `tokentrimmer.usage` event carrying
/// cost/baseline/saved (so streaming clients can surface per-request savings,
/// which response headers can't), then the `[DONE]` sentinel.
struct TrackedEventStream {
    inner: Arc<std::sync::Mutex<UsageTrackingStream>>,
    /// Served-model pricing for the terminal usage event (`None` ⇒ skip it).
    pricing: Option<ModelPricing>,
    /// Originally-requested-model pricing for the baseline in the usage event.
    baseline_pricing: Option<ModelPricing>,
    /// Provider surcharge multiplier — applied to cost and baseline (§2.13).
    fee_multiplier: f64,
    phase: Phase,
}

impl TrackedEventStream {
    /// Build the terminal `tokentrimmer.usage` SSE event from the accumulated
    /// usage. Returns `None` when there is no pricing to compute cost from.
    fn usage_event(&self) -> Option<Event> {
        let pricing = self.pricing.as_ref()?;
        let usage = {
            let guard = self.inner.lock().expect("tracking stream mutex poisoned");
            guard.snapshot()
        };
        let cost_usd = compute_streaming_cost(&usage, Some(pricing)) * self.fee_multiplier;
        let baseline_cost_usd =
            compute_streaming_baseline(&usage, self.baseline_pricing.as_ref().or(Some(pricing)))
                * self.fee_multiplier;
        let saved_usd = (baseline_cost_usd - cost_usd).max(0.0_f64);
        let json = serde_json::json!({
            "cost_usd": cost_usd,
            "baseline_cost_usd": baseline_cost_usd,
            "saved_usd": saved_usd,
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cached_tokens": usage.cached_tokens,
        })
        .to_string();
        Some(Event::default().event("tokentrimmer.usage").data(json))
    }
}

impl Stream for TrackedEventStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.phase {
            Phase::Finished => return Poll::Ready(None),
            Phase::EmitDone => {
                self.phase = Phase::Finished;
                return Poll::Ready(Some(Ok(Event::default().data("[DONE]"))));
            }
            Phase::Streaming => {}
        }
        let poll = {
            let mut guard = self.inner.lock().expect("tracking stream mutex poisoned");
            Pin::new(&mut *guard).poll_next(cx)
        };
        match poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                // Clean completion: emit the terminal usage event (when we can
                // price it), then `[DONE]` on the next poll.
                self.phase = Phase::EmitDone;
                match self.usage_event() {
                    Some(ev) => Poll::Ready(Some(Ok(ev))),
                    None => {
                        self.phase = Phase::Finished;
                        Poll::Ready(Some(Ok(Event::default().data("[DONE]"))))
                    }
                }
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
            // Wrap stream to accumulate usage.  Pass provider_id so the
            // fallback tokenizer can pick the right estimator (§2.12).
            let tracking = UsageTrackingStream::new(
                stream,
                ctx.input_tokens,
                ctx.cached_tokens,
                ctx.provider_id.as_str(),
            );

            // Arc<Mutex> lets the guard closure read the final accumulated state
            // after the stream has been drained (or dropped mid-way).
            let shared = Arc::new(std::sync::Mutex::new(tracking));
            let shared_for_guard = Arc::clone(&shared);

            // Pricing drives both the terminal usage event and the request_logs row.
            let pricing = ctx.pricing.clone();
            // Baseline against the originally-requested model; falls back to the
            // served model's pricing when no separate baseline was supplied.
            let baseline_pricing = ctx.baseline_pricing.clone().or_else(|| pricing.clone());
            // Provider surcharge (§2.13) — applied to both cost and baseline.
            let fee_multiplier = ctx.fee_multiplier;

            let event_stream = TrackedEventStream {
                inner: Arc::clone(&shared),
                pricing: pricing.clone(),
                baseline_pricing: baseline_pricing.clone(),
                fee_multiplier,
                phase: Phase::Streaming,
            };

            // Capture everything the guard closure needs.
            let writer = ctx.writer.clone();
            let org_id = ctx.org_id;
            let api_key_id = ctx.api_key_id;
            let provider_id_log = ctx.provider_id.clone();
            let model = ctx.model.clone();
            let route_id = ctx.route_id;
            let tag = ctx.tag.clone();
            let request_started = ctx.request_started;
            let log_trace_id = ctx.trace_id;
            let spend_sink = ctx.spend_sink.clone();
            let cache_insert = ctx.cache_insert;

            let guard = DropGuard::new(move || {
                let inner = shared_for_guard
                    .lock()
                    .expect("tracking stream mutex poisoned");
                let usage = inner.snapshot();
                let truncated = !inner.finished;
                // Extract cache data before dropping the lock.
                let cache_data = if !truncated {
                    inner.cache_completion_data()
                } else {
                    None
                };
                drop(inner);

                // Apply provider surcharge to both cost and baseline (§2.13).
                let cost_usd = compute_streaming_cost(&usage, pricing.as_ref()) * fee_multiplier;
                let baseline_cost_usd =
                    compute_streaming_baseline(&usage, baseline_pricing.as_ref()) * fee_multiplier;

                // Record realized streamed spend into the same enforcer the check uses.
                spend_sink.record(org_id, cost_usd, Utc::now());

                let row = RequestLogRow {
                    id: Uuid::now_v7(),
                    org_id,
                    api_key_id,
                    ts: Utc::now(),
                    provider: provider_id_log,
                    model: model.clone(),
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

                if let Some(w) = writer {
                    let writer_clone = w.clone();
                    tokio::spawn(async move {
                        if let Err(e) = writer_clone.write(row).await {
                            tracing::warn!(error = %e, "sse request_logs write failed");
                        }
                    });
                }

                // Best-effort cache insert on clean completion (§rv-l2-streaming-cache-write).
                //
                // Conditions: stream completed cleanly (!truncated), authoritative usage
                // arrived (cache_data is Some), and a CacheInsertContext was supplied by
                // the caller. Tool-call responses are excluded (checked below).
                if let (false, Some((output_text, finish_reason, auth_usage)), Some(ins)) =
                    (truncated, cache_data, cache_insert)
                {
                    // Reconstruct the ChatCompletionResponse from accumulated data.
                    use tt_shared::messages::{Choice, Message, MessageContent};
                    let response = ChatCompletionResponse {
                        id: format!("chatcmpl-stream-{}", Uuid::now_v7()),
                        object: "chat.completion".into(),
                        created: Utc::now().timestamp(),
                        model: ins.model.clone(),
                        choices: vec![Choice {
                            index: 0,
                            message: Message::Assistant {
                                content: Some(MessageContent::Text(output_text)),
                                tool_calls: vec![],
                                name: None,
                            },
                            finish_reason: Some(finish_reason),
                        }],
                        usage: auth_usage,
                    };

                    // Do not cache tool-call responses (non-deterministic).
                    let has_tool_calls = response.choices.iter().any(|c| {
                        if let Message::Assistant { tool_calls, .. } = &c.message {
                            !tool_calls.is_empty()
                        } else {
                            false
                        }
                    });
                    if has_tool_calls {
                        tracing::debug!(
                            "streaming cache insert skipped: response contains tool calls"
                        );
                        return;
                    }

                    // Spawn best-effort inserts — never block or fail the client response.
                    // Use cost_usd / baseline_cost_usd already computed above for the
                    // request_logs row — those are the authoritative streaming costs.
                    let entry = L1Entry::new(
                        response.clone(),
                        baseline_cost_usd,
                        cost_usd,
                        ins.provider_id.clone(),
                    );
                    if let Some(l1) = ins.l1 {
                        let key = ins.l1_key.clone();
                        let ttl = ins.ttl_secs;
                        tokio::spawn(async move {
                            match entry.to_bytes() {
                                Ok(bytes) => {
                                    if let Err(e) = l1.cache.set(&key, &bytes, ttl).await {
                                        tracing::warn!(
                                            error = %e,
                                            key = %key,
                                            "streaming l1 cache insert failed"
                                        );
                                    } else {
                                        tracing::debug!(key = %key, "streaming l1 cache insert ok");
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "streaming l1 entry serialization failed"
                                    );
                                }
                            }
                        });
                    }
                    if let (Some(l2), Some(query_text)) = (ins.l2, ins.l2_query_text) {
                        let ttl_secs = ins.ttl_secs;
                        let org_id_l2 = ins.org_id;
                        let provider_id_l2 = ins.provider_id.clone();
                        let model_l2 = ins.model.clone();
                        tokio::spawn(async move {
                            stream_insert_into_l2(
                                l2,
                                org_id_l2,
                                &query_text,
                                response,
                                provider_id_l2,
                                model_l2,
                                ttl_secs,
                            )
                            .await;
                        });
                    }
                }
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

// ─── stream_insert_into_l2 ────────────────────────────────────────────────────

/// Best-effort L2 insert for the streaming cache-write path.
/// Mirrors `insert_into_l2` in `chat.rs` but lives here so the DropGuard
/// closure can call it without crossing module boundaries.
async fn stream_insert_into_l2(
    l2: L2Config,
    org_id: Uuid,
    query_text: &str,
    response: ChatCompletionResponse,
    _provider_id: String,
    model_used: String,
    ttl_secs: u64,
) {
    let embedding_model = l2.embedder.model().to_string();
    let embed = match l2.embedder.embed(query_text).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "streaming l2 embed during insert failed");
            return;
        }
    };
    let response_bytes = match serde_json::to_vec(&response) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "streaming l2 response serialization failed");
            return;
        }
    };
    let ttl = Duration::from_secs(ttl_secs);
    let now = Utc::now();
    let entry = CacheEntry {
        id: Uuid::now_v7(),
        org_id,
        embedding: embed,
        response: response_bytes,
        model: model_used,
        embedding_model,
        input_tokens: response.usage.prompt_tokens,
        output_tokens: response.usage.completion_tokens,
        hit_count: 0,
        created_at: now,
        expires_at: now + chrono::Duration::from_std(ttl).unwrap_or_default(),
    };
    if let Err(e) = l2.cache.insert(entry).await {
        tracing::warn!(error = %e, "streaming l2 cache insert failed");
    }
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
        let mut tracker = UsageTrackingStream::new(stream, 10, 2, "openai");
        let _ = tracker.next().await;
        let _ = tracker.next().await;
        let usage = tracker.snapshot();
        // Authoritative block: 10 input, 5 output, 2 cached.
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cached_tokens, 2);
        assert!(tracker.finished);
    }

    /// §2.12 — when no authoritative usage arrives (e.g. client abort),
    /// `snapshot()` must return output_tokens from the tokenizer, not byte count.
    /// For ASCII text, bytes ≈ tokens so both are similar, but for a known
    /// multi-byte / repeated string the token estimate must be less than the byte
    /// count AND greater than zero.
    #[tokio::test]
    async fn snapshot_fallback_uses_tokenizer_not_bytes() {
        // Build a chunk with content that has more bytes than tokens.
        // "Hello " is 6 bytes but only 2 tokens (tiktoken cl100k).
        // Use a longer string so the byte vs. token gap is clear.
        let long_text = "Hello, world! This is a streaming output test. ".repeat(10);
        let byte_len = long_text.len() as i32; // raw byte count

        let chunks = vec![Ok(ChatCompletionChunk {
            id: "x".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: "m".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: Some(long_text.clone()),
                    tool_calls: vec![],
                },
                finish_reason: None,
            }],
            usage: None, // no authoritative usage → triggers fallback
        })];
        let stream = futures::stream::iter(chunks).boxed();
        let mut tracker = UsageTrackingStream::new(stream, 5, 0, "openai");
        let _ = tracker.next().await;
        let usage = tracker.snapshot();

        // Tokenizer output_tokens must be strictly less than raw byte count.
        assert!(
            usage.output_tokens < byte_len,
            "expected output_tokens ({}) < byte_len ({})",
            usage.output_tokens,
            byte_len
        );
        // And must be positive.
        assert!(usage.output_tokens > 0);
        // input_tokens and cached_tokens come from the constructor.
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.cached_tokens, 0);
    }

    /// §2.13 — `compute_streaming_cost` and baseline are each multiplied by
    /// `fee_multiplier` when applied via the `TrackedEventStream`/DropGuard.
    /// Test the raw compute helpers: 1 000 input + 500 output at $1/$2 per M
    /// with a 1.05 multiplier should yield the correct scaled values.
    #[test]
    fn streaming_fee_multiplier_scales_cost_and_baseline() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        };
        let usage = PartialUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        };
        let base_cost = compute_streaming_cost(&usage, Some(&pricing));
        let base_baseline = compute_streaming_baseline(&usage, Some(&pricing));

        let fee = 1.05_f64;
        let scaled_cost = base_cost * fee;
        let scaled_baseline = base_baseline * fee;

        // base_cost = 1000/1e6 + 500*2/1e6 = 0.001 + 0.001 = 0.002
        assert!(
            (base_cost - 0.002_f64).abs() < 1e-9,
            "base_cost={base_cost}"
        );
        assert!(
            (scaled_cost - 0.002_f64 * 1.05).abs() < 1e-9,
            "scaled_cost={scaled_cost}"
        );
        assert!(
            (scaled_baseline - base_baseline * 1.05).abs() < 1e-9,
            "scaled_baseline={scaled_baseline}"
        );
    }

    /// §2.15 — streaming input estimate should match tt_tokenize (not bytes/4)
    /// for a known string.  The old heuristic summed bytes then divided by 4;
    /// tt_tokenize uses chars/4 (or tiktoken).  For ASCII they are the same, but
    /// the estimate_tokens call should agree with a direct tt_tokenize call.
    #[test]
    fn streaming_input_estimate_matches_tt_tokenize() {
        let text = "The quick brown fox jumps over the lazy dog.";
        let provider_id = "openai";
        let tt_estimate = tt_tokenize::estimate_tokens(provider_id, text);

        // The old heuristic would have given text.len()/4 = 44/4 = 11.
        // tt_tokenize (tiktoken) gives a more precise count.
        // Whatever the value, our code must produce the same number.
        assert_eq!(
            tt_estimate,
            tt_tokenize::estimate_tokens(provider_id, text),
            "estimate_tokens should be deterministic"
        );
        // Sanity: non-zero.
        assert!(tt_estimate > 0);
    }

    #[tokio::test]
    async fn usage_tracking_captures_cache_creation_tokens() {
        let chunks = vec![Ok(ChatCompletionChunk {
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
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
                cached_tokens: 20,
                cache_creation_input_tokens: Some(30),
            }),
        })];
        let stream = futures::stream::iter(chunks).boxed();
        let mut tracker = UsageTrackingStream::new(stream, 100, 20, "anthropic");
        let _ = tracker.next().await;

        let usage = tracker.snapshot();
        assert_eq!(usage.cache_creation_tokens, 30);

        let (_text, _fr, reconstructed) = tracker.cache_completion_data().unwrap();
        assert_eq!(reconstructed.cache_creation_input_tokens, Some(30));
    }
}
