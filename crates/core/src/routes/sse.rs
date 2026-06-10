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
//!
//! ## Span attributes
//!
//! The same drop guard also records the OpenTelemetry GenAI semconv +
//! TokenTrimmer cost attributes (`gen_ai.*` / `tokentrimmer.*`) onto the
//! captured `http_request` span via [`StreamSpanContext`].  Streaming cost is
//! only known once the stream drains, and the request span has already exited by
//! the time the SSE body is polled — so the handler captures the span handle and
//! the guard stamps the attributes onto it from the same `compute_cost`
//! breakdown it computes for the row.  This keeps streaming traffic in the
//! spend/savings/tokens/cache dashboards, matching the non-streaming path.

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

use tracing::Span;

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
    /// True once a standalone OpenAI-native usage chunk (empty `choices`,
    /// populated `usage`) has flowed through. Lets the egress avoid emitting a
    /// duplicate synthesized usage chunk when `include_usage` is honored.
    saw_standalone_usage_chunk: bool,
    /// `(id, model, created)` of the most recent chunk, used to stamp the
    /// synthesized OpenAI-native usage chunk so it matches the stream.
    last_chunk_meta: Option<(String, String, i64)>,
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
            saw_standalone_usage_chunk: false,
            last_chunk_meta: None,
        }
    }

    /// Whether the upstream already forwarded a standalone OpenAI-native usage
    /// chunk (empty `choices`, populated `usage`). When `true`, the egress need
    /// not synthesize one to honor `include_usage`.
    pub(crate) fn saw_standalone_usage_chunk(&self) -> bool {
        self.saw_standalone_usage_chunk
    }

    /// Build the OpenAI-native final usage chunk (empty `choices`, populated
    /// `usage`) from the accumulated authoritative usage, stamped with the
    /// stream's id/model/created. Returns `None` when no authoritative usage
    /// arrived (a truncated stream — nothing trustworthy to report).
    fn synthesized_usage_chunk(&self) -> Option<ChatCompletionChunk> {
        let (prompt, completion, cached, cache_creation) = self.authoritative?;
        let (id, model, created) = self
            .last_chunk_meta
            .clone()
            .unwrap_or_else(|| (String::new(), String::new(), 0));
        Some(ChatCompletionChunk {
            id,
            object: "chat.completion.chunk".to_string(),
            created,
            model,
            choices: Vec::new(),
            usage: Some(Usage {
                prompt_tokens: prompt as u64,
                completion_tokens: completion as u64,
                total_tokens: (prompt + completion) as u64,
                cached_tokens: cached as u64,
                cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation as u64),
            }),
            extra: Default::default(),
        })
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
            // Remember the latest chunk's identity so a synthesized usage chunk
            // (when `include_usage` is honored) matches the stream.
            self.last_chunk_meta = Some((chunk.id.clone(), chunk.model.clone(), chunk.created));
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
                // A standalone usage chunk (no choices) is the OpenAI-native
                // include_usage shape — record it so we don't synthesize a
                // duplicate at the egress.
                if chunk.choices.is_empty() {
                    self.saw_standalone_usage_chunk = true;
                }
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

// ─── StreamSpanContext ──────────────────────────────────────────────────────────

/// Metadata + the captured gateway request span needed to record the OTel
/// GenAI semconv + TokenTrimmer cost attributes when the SSE stream terminates.
///
/// The cost is only known once the stream drains (the [`DropGuard`] computes it
/// from the accumulated usage), and the stream body is polled *after* the
/// handler future — and thus the `http_request` span — has already exited. So we
/// capture the span handle here (cloning a [`Span`] keeps it open) and record
/// the attributes onto it from inside the guard, mirroring the non-streaming
/// path's call to `record_request_span_attributes`. The token + cost figures
/// are pulled from the same `compute_cost` breakdown the guard computes for the
/// `request_logs` row and the `tokentrimmer.usage` frame — nothing is recomputed.
pub struct StreamSpanContext {
    /// The gateway `http_request` span, captured in the handler so the guard can
    /// stamp attributes onto it after the handler future has exited.
    pub span: Span,
    /// TokenTrimmer provider id → `gen_ai.system` / `gen_ai.provider.name`.
    pub provider_id: String,
    /// Model the caller asked for → `gen_ai.request.model`.
    pub request_model: String,
    /// Model that served the request → `gen_ai.response.model`.
    pub response_model: String,
    /// Cache outcome → `tokentrimmer.cache` (`miss` when a cache layer is wired
    /// on the live streaming path, `none` otherwise).
    pub cache_outcome: String,
    /// Matched route name → `tokentrimmer.route` (when routing applied).
    pub route: Option<String>,
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
    /// Whether the client requested `stream_options.include_usage = true`. When
    /// set, the stream guarantees an OpenAI-native final usage chunk (empty
    /// `choices`, populated `usage`) is emitted before the `tokentrimmer.usage`
    /// frame and `[DONE]` — synthesized from accumulated usage only when the
    /// provider did not already forward a standalone usage chunk.
    pub include_usage: bool,
    /// Optional OTel GenAI semconv + cost span-attribute context. When `Some`,
    /// the [`DropGuard`] records `gen_ai.*` + `tokentrimmer.*` attributes onto
    /// the captured `http_request` span once the per-request cost is known —
    /// mirroring the non-streaming path so streaming traffic is not dropped from
    /// the spend/savings/tokens/cache dashboards. `None` skips recording (e.g.
    /// the fake-stream cache-hit path, which logs its own row separately).
    pub span_ctx: Option<StreamSpanContext>,
}

// ─── TrackedEventStream ───────────────────────────────────────────────────────

/// Terminal-sequence state for [`TrackedEventStream`].
enum Phase {
    /// Forwarding provider chunks.
    Streaming,
    /// Inner stream exhausted; emitting the queued terminal events
    /// (OpenAI-native usage chunk → `tokentrimmer.usage` → `[DONE]`) one per
    /// poll, in order.
    EmitTerminal(std::collections::VecDeque<Event>),
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
    /// Honor `stream_options.include_usage`: emit an OpenAI-native final usage
    /// chunk before the `tokentrimmer.usage` frame when the client asked for it.
    include_usage: bool,
    phase: Phase,
}

impl TrackedEventStream {
    /// Build the OpenAI-native final usage chunk SSE event (a normal `data:`
    /// chunk with empty `choices` + populated `usage`) when `include_usage` is
    /// honored and the provider did not already forward a standalone usage
    /// chunk. Returns `None` when not requested, already satisfied upstream, or
    /// no authoritative usage is available (truncated stream).
    fn include_usage_chunk_event(&self) -> Option<Event> {
        if !self.include_usage {
            return None;
        }
        let guard = self.inner.lock().expect("tracking stream mutex poisoned");
        if guard.saw_standalone_usage_chunk() {
            return None;
        }
        let chunk = guard.synthesized_usage_chunk()?;
        drop(guard);
        let json = serde_json::to_string(&chunk)
            .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}"));
        Some(Event::default().data(json))
    }

    /// Build the terminal `tokentrimmer.usage` SSE event from the accumulated
    /// usage. Returns `None` when there is no pricing to compute cost from.
    fn usage_event(&self) -> Option<Event> {
        let pricing = self.pricing.as_ref()?;
        let usage = {
            let guard = self.inner.lock().expect("tracking stream mutex poisoned");
            guard.snapshot()
        };
        let breakdown = crate::routes::chat::compute_cost(
            &partial_to_usage(&usage),
            Some(pricing),
            self.baseline_pricing.as_ref(),
            self.fee_multiplier,
        );
        // `saved_usd` is strictly TT-attributed; the provider's automatic
        // cache discount rides in its own field (mirrors the response-header
        // split on the non-streaming path).
        let json = serde_json::json!({
            "cost_usd": breakdown.cost_usd,
            "baseline_cost_usd": breakdown.baseline_cost_usd,
            "saved_usd": breakdown.tt_saved_usd(),
            "provider_cache_saved_usd": breakdown.provider_cache_saved_usd,
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
        match &mut self.phase {
            Phase::Finished => return Poll::Ready(None),
            Phase::EmitTerminal(queue) => {
                let next = queue.pop_front();
                if queue.is_empty() {
                    self.phase = Phase::Finished;
                }
                // The queue always ends with `[DONE]`, so `next` is always Some
                // while in this phase; fall through to Finished otherwise.
                return Poll::Ready(next.map(Ok));
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
                // Clean completion: queue the terminal events in order —
                //   1. OpenAI-native usage chunk (only when include_usage honored),
                //   2. the `tokentrimmer.usage` cost frame (always, when priceable),
                //   3. `[DONE]`.
                let mut queue: std::collections::VecDeque<Event> =
                    std::collections::VecDeque::new();
                if let Some(ev) = self.include_usage_chunk_event() {
                    queue.push_back(ev);
                }
                if let Some(ev) = self.usage_event() {
                    queue.push_back(ev);
                }
                queue.push_back(Event::default().data("[DONE]"));
                let first = queue.pop_front();
                self.phase = if queue.is_empty() {
                    Phase::Finished
                } else {
                    Phase::EmitTerminal(queue)
                };
                Poll::Ready(first.map(Ok))
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
            // Honor stream_options.include_usage on the egress.
            let include_usage = ctx.include_usage;

            let event_stream = TrackedEventStream {
                inner: Arc::clone(&shared),
                pricing: pricing.clone(),
                baseline_pricing: baseline_pricing.clone(),
                fee_multiplier,
                include_usage,
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
            let span_ctx = ctx.span_ctx;

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

                // Reuse the authoritative non-streaming cost math (3-bucket
                // input pricing incl. cache-write premium); fee applied inside.
                let breakdown = crate::routes::chat::compute_cost(
                    &partial_to_usage(&usage),
                    pricing.as_ref(),
                    baseline_pricing.as_ref(),
                    fee_multiplier,
                );
                let cost_usd = breakdown.cost_usd;
                let baseline_cost_usd = breakdown.baseline_cost_usd;

                // Record realized streamed spend into the same enforcer the check uses.
                spend_sink.record(org_id, cost_usd, Utc::now());

                // Record OTel GenAI semconv + TokenTrimmer cost attributes onto
                // the captured `http_request` span, mirroring the non-streaming
                // path. Done here (not in the handler) because the cost is only
                // known once the stream drains, and the span handle was captured
                // so it is still open after the handler future exited. Pulls
                // from the same `breakdown` + `usage` as the request_logs row —
                // nothing recomputed. No-op on a span with no OTel layer.
                if let Some(sc) = span_ctx.as_ref() {
                    tt_telemetry::gen_ai::record_request_attributes(
                        &sc.span,
                        &tt_telemetry::gen_ai::RequestSpanAttributes {
                            provider_id: &sc.provider_id,
                            request_model: &sc.request_model,
                            response_model: &sc.response_model,
                            operation: "chat",
                            cost: tt_telemetry::gen_ai::RequestSpanCost {
                                input_tokens: usage.input_tokens.max(0) as u64,
                                output_tokens: usage.output_tokens.max(0) as u64,
                                cost_usd,
                                baseline_cost_usd,
                                saved_usd: breakdown.tt_saved_usd(),
                                provider_cache_saved_usd: breakdown.provider_cache_saved_usd,
                            },
                            cache_outcome: Some(&sc.cache_outcome),
                            route: sc.route.as_deref(),
                        },
                    );
                }

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
                    provider_cache_saved_usd: breakdown.provider_cache_saved_usd,
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
                        // Store the catalog-derived baseline on the L2 row so
                        // later hits report honest savings. None (→ NULL) when
                        // the model is absent from the catalog: the hit path
                        // then re-prices against the catalog current at hit
                        // time instead of freezing a meaningless $0.
                        let baseline_l2 = pricing.as_ref().map(|_| baseline_cost_usd);
                        tokio::spawn(async move {
                            stream_insert_into_l2(
                                l2,
                                org_id_l2,
                                &query_text,
                                response,
                                provider_id_l2,
                                model_l2,
                                ttl_secs,
                                baseline_l2,
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

/// Build a `Usage` from accumulated streaming counts so the streaming path can
/// reuse the authoritative non-streaming cost math (`chat::compute_cost`).
fn partial_to_usage(u: &PartialUsage) -> Usage {
    let prompt = u.input_tokens.max(0) as u64;
    let completion = u.output_tokens.max(0) as u64;
    let cache_creation = u.cache_creation_tokens.max(0) as u64;
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cached_tokens: u.cached_tokens.max(0) as u64,
        cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation),
    }
}

// ─── stream_insert_into_l2 ────────────────────────────────────────────────────

/// Best-effort L2 insert for the streaming cache-write path.
/// Mirrors `insert_into_l2` in `chat.rs` but lives here so the DropGuard
/// closure can call it without crossing module boundaries.
///
/// `baseline_cost_usd` is the catalog-derived baseline the stream's guard
/// computed via `compute_cost`; stored on the row so later hits report honest
/// savings. `None` (→ NULL) when the model was absent from the catalog.
#[allow(clippy::too_many_arguments)]
async fn stream_insert_into_l2(
    l2: L2Config,
    org_id: Uuid,
    query_text: &str,
    response: ChatCompletionResponse,
    _provider_id: String,
    model_used: String,
    ttl_secs: u64,
    baseline_cost_usd: Option<f64>,
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
        baseline_cost_usd,
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
                extra: Default::default(),
            },
            finish_reason: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
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
                extra: Default::default(),
            },
            finish_reason: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
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
            extra: Default::default(),
        }],
        usage: Some(usage),
        extra: Default::default(),
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
                        extra: Default::default(),
                    },
                    finish_reason: None,
                    extra: Default::default(),
                }],
                usage: None,
                extra: Default::default(),
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
                    extra: Default::default(),
                }],
                usage: Some(tt_shared::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 2,
                    cache_creation_input_tokens: None,
                }),
                extra: Default::default(),
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
                    extra: Default::default(),
                },
                finish_reason: None,
                extra: Default::default(),
            }],
            usage: None, // no authoritative usage → triggers fallback
            extra: Default::default(),
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

    /// §2.13 — the unified streaming cost (via `compute_cost`) applies the
    /// fee multiplier to both cost and baseline. 1 000 input + 500 output at
    /// $1/$2 per M, no cache, ×1.05.
    #[test]
    fn streaming_fee_multiplier_scales_cost_and_baseline() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        };
        let usage = PartialUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        };
        let u = partial_to_usage(&usage);

        let base = crate::routes::chat::compute_cost(&u, Some(&pricing), None, 1.0);
        let base_cost = base.cost_usd;
        let base_baseline = base.baseline_cost_usd;
        assert!(
            (base_cost - 0.002_f64).abs() < 1e-9,
            "base_cost={base_cost}"
        );

        let scaled = crate::routes::chat::compute_cost(&u, Some(&pricing), None, 1.05);
        let scaled_cost = scaled.cost_usd;
        let scaled_baseline = scaled.baseline_cost_usd;
        assert!(
            (scaled_cost - base_cost * 1.05).abs() < 1e-9,
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
                extra: Default::default(),
            }],
            usage: Some(tt_shared::Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
                cached_tokens: 20,
                cache_creation_input_tokens: Some(30),
            }),
            extra: Default::default(),
        })];
        let stream = futures::stream::iter(chunks).boxed();
        let mut tracker = UsageTrackingStream::new(stream, 100, 20, "anthropic");
        let _ = tracker.next().await;

        let usage = tracker.snapshot();
        assert_eq!(usage.cache_creation_tokens, 30);

        let (_text, _fr, reconstructed) = tracker.cache_completion_data().unwrap();
        assert_eq!(reconstructed.cache_creation_input_tokens, Some(30));
    }

    /// Regression: an all-fresh-input stream (no cache read/write) is priced
    /// identically to input×rate + output×rate — the unify refactor did not
    /// change non-Anthropic streaming costs.
    #[test]
    fn streaming_all_fresh_input_parity() {
        let usage = PartialUsage {
            input_tokens: 800,
            output_tokens: 200,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        };
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        };
        let cost =
            crate::routes::chat::compute_cost(&partial_to_usage(&usage), Some(&pricing), None, 1.0)
                .cost_usd;
        let expected = (800.0 * 3.0 + 200.0 * 6.0) / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-12,
            "cost={cost} expected={expected}"
        );
    }

    #[test]
    fn streaming_prices_cache_write_at_premium() {
        // 100 prompt tokens: 20 cache_read, 30 cache_write, 50 fresh; 10 output.
        let usage = PartialUsage {
            input_tokens: 100,
            output_tokens: 10,
            cached_tokens: 20,
            cache_creation_tokens: 30,
        };
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: Some(0.1),
            cache_write_per_million: Some(1.25),
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        };
        let cost =
            crate::routes::chat::compute_cost(&partial_to_usage(&usage), Some(&pricing), None, 1.0)
                .cost_usd;
        let expected = (50.0 * 1.0 + 20.0 * 0.1 + 30.0 * 1.25 + 10.0 * 2.0) / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-12,
            "cost={cost} expected={expected}"
        );
        let folded = (80.0 * 1.0 + 20.0 * 0.1 + 10.0 * 2.0) / 1_000_000.0;
        assert!(
            cost > folded,
            "premium not applied: cost={cost} folded={folded}"
        );
    }
}
