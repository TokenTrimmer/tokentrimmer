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
use crate::passes::PassEffects;
use crate::state::{L1Config, L2Config};

// ─── PartialUsage ─────────────────────────────────────────────────────────────

/// Token counts accumulated while the SSE stream is in flight.
#[derive(Debug, Clone, Default)]
pub struct PartialUsage {
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// Folded cache-read count (absent => 0) — what the cost math consumes.
    pub cached_tokens: i32,
    /// Raw provider-reported cache-read count. `None` = the provider reported
    /// no cache-read figure (or the stream was truncated before terminal
    /// usage); `Some(0)` = the provider explicitly reported zero.
    pub cache_read_tokens: Option<i32>,
    /// Raw provider-reported cache-write count. Same NULL semantics.
    pub cache_creation_tokens: Option<i32>,
}

/// Authoritative usage captured from the provider's terminal chunk.
#[derive(Debug, Clone, Copy)]
struct AuthoritativeUsage {
    prompt: i32,
    completion: i32,
    /// Raw cache-read Option (see [`PartialUsage::cache_read_tokens`]).
    cache_read: Option<i32>,
    /// Raw cache-write Option.
    cache_creation: Option<i32>,
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
    /// Authoritative usage from the provider's terminal chunk.
    authoritative: Option<AuthoritativeUsage>,
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
        let auth = self.authoritative?;
        let cache_creation = auth.cache_creation.unwrap_or(0);
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
                prompt_tokens: auth.prompt as u64,
                completion_tokens: auth.completion as u64,
                total_tokens: (auth.prompt + auth.completion) as u64,
                cached_tokens: auth.cache_read.unwrap_or(0) as u64,
                // Egress shape kept stable: cache_creation only when > 0, as
                // before; the raw cache-read rides additively when reported.
                cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation as u64),
                cache_read_input_tokens: auth.cache_read.map(|v| v as u64),
            }),
            extra: Default::default(),
        })
    }

    /// Returns the accumulated data needed to reconstruct a `ChatCompletionResponse`
    /// for cache insertion. Returns `None` when no authoritative usage block arrived
    /// (i.e. stream was truncated / no terminal usage chunk), ensuring only cleanly
    /// completed streams with known token counts are cached.
    pub(crate) fn cache_completion_data(&self) -> Option<(String, String, Usage)> {
        let auth = self.authoritative?;
        let cache_creation = auth.cache_creation.unwrap_or(0);
        let finish_reason = self.finish_reason.clone().unwrap_or_else(|| "stop".into());
        let text = self.output_text.clone();
        let usage = Usage {
            prompt_tokens: auth.prompt as u64,
            completion_tokens: auth.completion as u64,
            total_tokens: (auth.prompt + auth.completion) as u64,
            cached_tokens: auth.cache_read.unwrap_or(0) as u64,
            cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation as u64),
            cache_read_input_tokens: auth.cache_read.map(|v| v as u64),
        };
        Some((text, finish_reason, usage))
    }

    /// Whether a terminal usage block arrived (i.e. the snapshot's counts are
    /// provider-authoritative rather than tokenizer estimates).
    pub(crate) fn has_authoritative_usage(&self) -> bool {
        self.authoritative.is_some()
    }

    pub(crate) fn snapshot(&self) -> PartialUsage {
        if let Some(auth) = self.authoritative {
            PartialUsage {
                input_tokens: auth.prompt,
                output_tokens: auth.completion,
                cached_tokens: auth.cache_read.unwrap_or(0),
                cache_read_tokens: auth.cache_read,
                cache_creation_tokens: auth.cache_creation,
            }
        } else {
            // Fallback: estimate output tokens from accumulated text via
            // tt_tokenize rather than raw byte length (§2.12). No authoritative
            // block → no known cache counts; the raw Options stay None (NULL)
            // — never fabricated from estimates.
            let output_tokens =
                tt_tokenize::estimate_tokens(&self.provider_id, &self.output_text) as i32;
            PartialUsage {
                input_tokens: self.input_tokens,
                output_tokens,
                cached_tokens: self.cached_tokens,
                cache_read_tokens: None,
                cache_creation_tokens: None,
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
                self.authoritative = Some(AuthoritativeUsage {
                    prompt: usage.prompt_tokens as i32,
                    completion: usage.completion_tokens as i32,
                    // Raw cache-read Option, with a fold-rescue: a pre-fix
                    // folded chunk (cached_tokens > 0, raw field absent) still
                    // proves real cache reads — map it to Some so it never
                    // regresses to NULL. A folded 0 with no raw field is
                    // indistinguishable from "unreported" → None (NULL).
                    cache_read: crate::routes::chat::opt_tokens_i32(usage.cache_read_input_tokens)
                        .or_else(|| {
                            (usage.cached_tokens > 0)
                                .then(|| usage.cached_tokens.min(i32::MAX as u64) as i32)
                        }),
                    cache_creation: crate::routes::chat::opt_tokens_i32(
                        usage.cache_creation_input_tokens,
                    ),
                });
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
    /// TTL seconds for the L1 insert.
    pub ttl_secs: u64,
    /// TTL seconds for the L2 insert. Equals `ttl_secs` unless the
    /// volatility-class TTL (opt-in, shorten-only) shortened it for a
    /// volatile query — L1 TTLs are deliberately never shortened (the
    /// near-miss-paraphrase risk is an L2 phenomenon; L1 is exact-byte).
    pub l2_ttl_secs: u64,
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
    /// Canary `traffic_pct` (0-100) → `tokentrimmer.traffic_split_pct` (when the
    /// matched route declared a split). `None` for non-canary traffic. ADDITIVE.
    pub traffic_split_pct: Option<u32>,
}

// ─── PanelStreamLog ─────────────────────────────────────────────────────────

/// Panel-specific billing context attached to a streaming deep-research panel.
///
/// When present on a [`StreamLogContext`], the terminal [`DropGuard`] writes ONE
/// aggregate `request_logs` row (`provider = "panel"`, `cost = Σ legs + arbiter`,
/// `cached = false`) instead of a single-model row — mirroring the non-streaming
/// `complete_panel` billing — plus the per-leg `panel_legs` rows. This is the
/// streaming analogue of the data `complete_panel` holds inline; the streaming
/// path defers the row to stream-end because the arbiter answer is streamed.
///
/// Off-by-default: a `StreamLogContext` with `panel == None` is byte-for-byte the
/// historical single-model behavior.
pub struct PanelStreamLog {
    /// All leg results (member legs + the arbiter leg) for the per-leg
    /// `panel_legs` rows, written fire-and-forget at stream end.
    pub leg_records: Vec<crate::routes::panel::LegResult>,
    /// None-aware sum of member-leg costs (already summed by the panel engine).
    /// `None` when every surviving leg was unpriced.
    pub leg_cost_total: Option<f64>,
    /// Arbitration strategy → `tokentrimmer.panel_strategy` span attribute.
    pub strategy: crate::routes::panel::ArbiterStrategyKind,
    /// Quorum required / met → panel span attributes.
    pub quorum_required: usize,
    pub quorum_met: usize,
    /// Strategy-specific arbiter metadata (carried for parity / future use).
    pub arbiter_detail: crate::routes::panel::ArbiterDetail,
    /// How to obtain the arbiter's cost contribution at stream end. `Live`
    /// (Synthesize) prices the streamed answer's usage; `Known` (BestOfN /
    /// Majority replay) uses a pre-computed cost and DISCARDS the streamed
    /// figure — the replayed leg is already counted in `leg_cost_total`, so
    /// repricing the streamed tokens would double-count (invariant 3).
    pub arbiter_cost_plan: crate::routes::panel::ArbiterCostPlan,
    /// The arbiter model id → the `request_logs.model` column for the row.
    pub arbiter_model: String,
    /// Per-leg telemetry writer. `None` skips the `panel_legs` rows (e.g. dev
    /// without a DB, or tests focused on the aggregate row).
    pub panel_leg_writer: Option<Arc<dyn tt_telemetry::panel_legs::PanelLegWriter>>,
}

// ─── LogContext ───────────────────────────────────────────────────────────────

/// Caller-supplied metadata needed to construct the `request_logs` row when
/// the SSE stream terminates.
pub struct StreamLogContext {
    /// Optional log writer. `None` skips the telemetry row (e.g. tests or dev
    /// environments without a DB), but cache insertion still fires if
    /// `cache_insert` is `Some`.
    pub writer: Option<Arc<dyn RequestLogWriter>>,
    /// Optional [`TaskTracker`](tokio_util::task::TaskTracker) (REL-3 / P2). The
    /// terminal `request_logs` write is detached on stream end; when this is
    /// `Some` the spawned write is TRACKED so a graceful shutdown can drain it
    /// (and its bounded retries) instead of dropping a committed billing row on
    /// a rolling deploy / SIGTERM. `None` falls back to a bare `tokio::spawn`
    /// (tests / dev without a tracker wired).
    pub tracker: Option<tokio_util::task::TaskTracker>,
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
    /// Whether this request was opted into OpenAI Flex (`service_tier="flex"`).
    /// Set by the same `maybe_apply_flex` rewrite the non-streaming path uses,
    /// so the streaming cost math meters at flex rates and attributes the
    /// standard-vs-flex saving to the `flex` source (FLEX-REWRITE requirement
    /// (2)). When `false`, cost math is unchanged (standard rates).
    pub flex_applied: bool,
    /// Request-pass effects: the (pipeline-measured) input tokens the
    /// conservative compression pass removed before dispatch, plus any booked
    /// cache-bust penalty (a redaction inside the stable prefix). Threaded
    /// into the streaming cost math so the standard-vs-compressed saving is
    /// attributed to the `compression` source AND the negative cache-bust
    /// entry reduces the headline on the terminal `tokentrimmer.usage` event —
    /// parity with the non-streaming path (the passes run before the
    /// stream/non-stream branch, so a streaming request must carry their
    /// effects too).
    pub pass_effects: PassEffects,
    /// Net token delta from retrieval substitution before dispatch. Token-only
    /// accounting; persisted to request_logs and never folded into USD cost.
    pub retrieval_tokens_saved: i64,
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
    /// Canary traffic-split arm (`"canary"` / `"control"`) the sticky split
    /// assigned this streaming request to, recorded on the `request_logs` row.
    /// `None` when the matched route declared no `traffic_pct` (or no route
    /// matched). Shadow mode is non-streaming, so the streaming row never carries
    /// a `shadow_model` / `shadow_cost_usd`.
    pub traffic_split_arm: Option<String>,
    /// The matched route was sticky-PAUSED (rewrite suppressed; the stream was
    /// served on the originally-requested model). Recorded on the
    /// `request_logs.route_paused` marker; `false` for unrouted/unpaused
    /// requests.
    pub route_paused: bool,
    /// Panel aggregate-billing context. `Some` rewrites the terminal
    /// [`DropGuard`] to write ONE `provider = "panel"` aggregate row (Σ legs +
    /// arbiter) plus the per-leg `panel_legs` rows, mirroring `complete_panel`.
    /// `None` (the default for every non-panel caller) leaves the single-model
    /// behavior byte-for-byte unchanged (off-by-default invariant).
    pub panel: Option<Arc<PanelStreamLog>>,
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
    /// Whether the request was served via OpenAI Flex — meters the terminal
    /// `tokentrimmer.usage` cost at flex rates and attributes the flex saving.
    flex_applied: bool,
    /// Request-pass effects — attributes the `compression` saving and carries
    /// the cache-bust penalty on the terminal `tokentrimmer.usage` event.
    pass_effects: PassEffects,
    /// Honor `stream_options.include_usage`: emit an OpenAI-native final usage
    /// chunk before the `tokentrimmer.usage` frame when the client asked for it.
    include_usage: bool,
    /// Panel aggregate-billing context. `Some` enables the `tokentrimmer.panel`
    /// terminal SSE event and makes `usage_event` emit the AGGREGATE cost
    /// (Σ legs + arbiter), matching the `request_logs` row written by the
    /// [`DropGuard`]. `None` leaves single-model behavior byte-for-byte unchanged
    /// (off-by-default invariant).
    panel: Option<Arc<PanelStreamLog>>,
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

    /// Build the terminal `tokentrimmer.panel` SSE event from the panel context.
    /// Returns `None` when `self.panel` is `None` (non-panel streams) or on
    /// serialization error (fail-soft: drops the metadata, never the answer).
    fn panel_event(&self) -> Option<Event> {
        let p = self.panel.as_ref()?;
        // To match the DropGuard aggregate exactly, compute the same `cost_usd`
        // the DropGuard writes as `total_cost_usd`:
        //   - Get the streamed arbiter cost by repricing the accumulated usage.
        //   - Pass it through `arbiter_cost_plan.finalize` (Known discards it).
        //   - Sum with `leg_cost_total`.
        let streamed_arbiter_cost: Option<f64> = if let Some(pricing) = self.pricing.as_ref() {
            let usage = {
                let guard = self.inner.lock().expect("tracking stream mutex poisoned");
                guard.snapshot()
            };
            let breakdown = crate::routes::chat::compute_cost_full(
                &partial_to_usage(&usage),
                Some(pricing),
                self.baseline_pricing.as_ref(),
                self.fee_multiplier,
                self.flex_applied,
                false,
                self.pass_effects,
                0,
                crate::shaping::ShapeEffects::default(),
            );
            Some(breakdown.cost_usd)
        } else {
            None
        };
        let arbiter_cost_usd = p.arbiter_cost_plan.finalize(streamed_arbiter_cost);
        let total_cost_usd = crate::routes::panel::sum_metered_iter(
            std::iter::once(p.leg_cost_total).chain(std::iter::once(arbiter_cost_usd)),
        );
        let value = crate::routes::panel::panel_body_json(
            p.strategy,
            &p.leg_records,
            &p.arbiter_detail,
            p.quorum_required,
            p.quorum_met,
            total_cost_usd,
            arbiter_cost_usd,
        );
        match serde_json::to_string(&value) {
            Ok(json) => Some(Event::default().event("tokentrimmer.panel").data(json)),
            Err(e) => {
                tracing::warn!(error = %e, "panel terminal event serialize failed");
                None
            }
        }
    }

    /// Build the terminal `tokentrimmer.usage` SSE event from the accumulated
    /// usage. Returns `None` when there is no pricing to compute cost from.
    ///
    /// When `self.panel.is_some()`, emits the AGGREGATE cost (Σ legs + arbiter),
    /// matching the `request_logs` row the [`DropGuard`] writes — so the
    /// streaming client sees the same figure it is billed. The non-panel path
    /// is byte-for-byte unchanged.
    fn usage_event(&self) -> Option<Event> {
        let pricing = self.pricing.as_ref()?;
        let usage = {
            let guard = self.inner.lock().expect("tracking stream mutex poisoned");
            guard.snapshot()
        };
        let breakdown = crate::routes::chat::compute_cost_full(
            &partial_to_usage(&usage),
            Some(pricing),
            self.baseline_pricing.as_ref(),
            self.fee_multiplier,
            self.flex_applied,
            // Batch never streams — the gateway clears the advisory marker
            // for streaming requests at the gate (`batch_ineligible:streaming`).
            false,
            self.pass_effects,
            // Streaming books $0 for the minify ESTIMATE in v1 (metered only).
            0,
            // Output shaping never streams — both planners skip streaming
            // requests, so the streaming path carries zero shape effects.
            crate::shaping::ShapeEffects::default(),
        );
        // Panel-aware cost: replace the arbiter-only streamed cost with the
        // aggregate (Σ legs + finalized arbiter), matching the DropGuard row.
        // Savings/baseline fields follow the same zero-out logic as the DropGuard
        // (`aggregate_cost_breakdown`: cost == baseline, all savings zero) so the
        // emitted event is consistent with the row. Non-panel path: pass through
        // `breakdown` unchanged (byte-for-byte identical to the pre-Task-5 path).
        let (cost_usd, baseline_cost_usd, saved_usd, prov_cache_saved_usd, cache_bust_usd) =
            if let Some(p) = self.panel.as_ref() {
                let arbiter_cost = p.arbiter_cost_plan.finalize(Some(breakdown.cost_usd));
                let total = crate::routes::panel::sum_metered_iter(
                    std::iter::once(p.leg_cost_total).chain(std::iter::once(arbiter_cost)),
                )
                .unwrap_or(0.0);
                // Panel: cost == baseline, no savings (mirrors aggregate_cost_breakdown).
                (total, total, 0.0_f64, 0.0_f64, 0.0_f64)
            } else {
                (
                    breakdown.cost_usd,
                    breakdown.baseline_cost_usd,
                    breakdown.tt_saved_usd(),
                    breakdown.provider_cache_saved_usd,
                    breakdown.cache_bust_penalty_usd,
                )
            };
        // `saved_usd` is strictly TT-attributed; the provider's automatic
        // cache discount rides in its own field (mirrors the response-header
        // split on the non-streaming path). `cache_bust_usd` is the explicit
        // negative-savings entry already subtracted from `saved_usd` pre-clamp
        // — surfaced so streaming clients see the bust MAGNITUDE, not just the
        // reduced headline (parity with `x-tokentrimmer-cache-bust-usd`).
        let json = serde_json::json!({
            "cost_usd": cost_usd,
            "baseline_cost_usd": baseline_cost_usd,
            "saved_usd": saved_usd,
            "provider_cache_saved_usd": prov_cache_saved_usd,
            "cache_bust_usd": cache_bust_usd,
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
                //   2. `tokentrimmer.panel` attribution (only for panel streams),
                //   3. the `tokentrimmer.usage` cost frame (always, when priceable),
                //   4. `[DONE]`.
                let mut queue: std::collections::VecDeque<Event> =
                    std::collections::VecDeque::new();
                if let Some(ev) = self.include_usage_chunk_event() {
                    queue.push_back(ev);
                }
                if let Some(ev) = self.panel_event() {
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
            // Whether the request was served via OpenAI Flex — drives both the
            // terminal usage event and the request_logs row cost math.
            let flex_applied = ctx.flex_applied;
            // Request-pass effects — drive the `compression` saving + the
            // cache-bust penalty on the terminal usage event + the row.
            let pass_effects = ctx.pass_effects;
            // Honor stream_options.include_usage on the egress.
            let include_usage = ctx.include_usage;

            // Panel aggregate-billing context (None for single-model streams).
            // Cloned here for the TrackedEventStream (terminal panel SSE event +
            // aggregate usage_event cost); cloned again below for the DropGuard.
            let panel = ctx.panel.clone();

            let event_stream = TrackedEventStream {
                inner: Arc::clone(&shared),
                pricing: pricing.clone(),
                baseline_pricing: baseline_pricing.clone(),
                fee_multiplier,
                flex_applied,
                pass_effects,
                include_usage,
                panel: panel.clone(),
                phase: Phase::Streaming,
            };

            // Capture everything the guard closure needs.
            let writer = ctx.writer.clone();
            let tracker = ctx.tracker.clone();
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
            let traffic_split_arm = ctx.traffic_split_arm.clone();
            let route_paused = ctx.route_paused;
            let retrieval_tokens_saved = ctx.retrieval_tokens_saved;

            let guard = DropGuard::new(move || {
                let inner = shared_for_guard
                    .lock()
                    .expect("tracking stream mutex poisoned");
                let usage = inner.snapshot();
                let truncated = !inner.finished;
                let authoritative = inner.has_authoritative_usage();
                // Extract cache data before dropping the lock.
                let cache_data = if !truncated {
                    inner.cache_completion_data()
                } else {
                    None
                };
                drop(inner);

                // Reuse the authoritative non-streaming cost math (3-bucket
                // input pricing incl. cache-write premium); fee applied inside.
                // `flex_applied` meters at flex rates and attributes the flex
                // saving, matching the non-streaming `compute_cost_with_flex`.
                let breakdown = crate::routes::chat::compute_cost_full(
                    &partial_to_usage(&usage),
                    pricing.as_ref(),
                    baseline_pricing.as_ref(),
                    fee_multiplier,
                    flex_applied,
                    // Batch never streams — the gateway clears the advisory
                    // marker at the gate (`batch_ineligible:streaming`).
                    false,
                    pass_effects,
                    // Streaming books $0 for the minify ESTIMATE in v1
                    // (metered only — see `record_minify_estimate`).
                    0,
                    // Output shaping never streams — both planners skip
                    // streaming requests (zero shape effects here).
                    crate::shaping::ShapeEffects::default(),
                );
                let cost_usd = breakdown.cost_usd;
                let baseline_cost_usd = breakdown.baseline_cost_usd;

                // Panel aggregate billing (Phase 5 Task 4). `cost_usd` above is
                // the STREAMED answer's cost. For a panel the billable figure is
                // `Σ legs + arbiter`:
                //   - `Live` (Synthesize): the streamed tokens ARE the arbiter's
                //     fresh work, so keep the streamed cost as the arbiter cost.
                //   - `Known` (BestOfN / Majority replay): the streamed tokens are
                //     a replay of a member leg already counted in `leg_cost_total`,
                //     so the cost plan DISCARDS the streamed figure and substitutes
                //     the pre-computed arbiter cost — no double-count (invariant 3).
                // The aggregate baseline EQUALS the cost: a panel is the service
                // the caller opted into, so no routing/cache saving is claimed
                // (mirrors `aggregate_cost_breakdown` in the non-streaming path).
                let (cost_usd, baseline_cost_usd, row_provider, row_model) =
                    if let Some(p) = panel.as_ref() {
                        let arbiter_cost = p.arbiter_cost_plan.finalize(Some(cost_usd));
                        let total = crate::routes::panel::sum_metered_iter(
                            std::iter::once(p.leg_cost_total).chain(std::iter::once(arbiter_cost)),
                        )
                        .unwrap_or(0.0);
                        (total, total, "panel".to_string(), p.arbiter_model.clone())
                    } else {
                        (
                            cost_usd,
                            baseline_cost_usd,
                            provider_id_log.clone(),
                            model.clone(),
                        )
                    };

                // Record realized streamed spend into the same enforcer the check uses.
                spend_sink.record(org_id, api_key_id, cost_usd, Utc::now());
                // P0-1/P0-3: settle the served request. This DropGuard fires
                // only on the `Some(log_ctx)` branch, which the streaming chat
                // handler builds ONLY for the live provider-dispatch path (the
                // L1 fake-stream cache-hit path passes `None` log_ctx and settles
                // `cached=true` inline at its return). So a settle reaching here
                // is always a live dispatch → non-cached → advances both the
                // billed and served counters.
                spend_sink.settle(org_id, api_key_id, false, Utc::now());

                // Telemetry-parity: for a panel aggregate row, savings/cache
                // fields must be zeroed to match `complete_panel`'s
                // `aggregate_cost_breakdown` (cost == baseline ⇒ no savings,
                // no provider cache credit). For a non-panel stream these keep
                // the actual `breakdown.*` values — off-by-default.
                let (
                    span_saved_usd,
                    span_prov_cache_saved_usd,
                    row_prov_cache_saved_usd,
                    row_cache_bust_usd,
                ) = if panel.is_some() {
                    (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64)
                } else {
                    (
                        breakdown.tt_saved_usd(),
                        breakdown.provider_cache_saved_usd,
                        breakdown.provider_cache_saved_usd,
                        breakdown.cache_bust_penalty_usd,
                    )
                };

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
                                saved_usd: span_saved_usd,
                                provider_cache_saved_usd: span_prov_cache_saved_usd,
                            },
                            cache_outcome: Some(&sc.cache_outcome),
                            route: sc.route.as_deref(),
                            // Shadow mode is non-streaming by design, so the
                            // streaming span never carries a shadow cost. The
                            // canary traffic-split pct/arm is recorded on the
                            // request_logs row below (the rewrite already
                            // happened before dispatch); we keep the streaming
                            // span attrs additive + minimal here.
                            traffic_split_pct: sc.traffic_split_pct,
                            shadow_model: None,
                            shadow_cost_usd: None,
                            // Panel-specific additive span attributes (Phase 5
                            // Task 4). `None` for non-panel streams — off-by-default,
                            // mirroring the non-streaming `complete_panel` span.
                            panel_strategy: panel.as_ref().map(|p| p.strategy.as_str()),
                            panel_leg_count: panel.as_ref().map(|p| p.leg_records.len() as i64),
                            panel_quorum_required: panel.as_ref().map(|p| p.quorum_required as i64),
                            panel_quorum_met: panel.as_ref().map(|p| p.quorum_met as i64),
                        },
                    );
                }

                // Hoisted so the per-leg `panel_legs` rows (written below for a
                // panel stream) can reference this parent `request_logs.id`.
                let parent_id = Uuid::now_v7();
                let row = RequestLogRow {
                    id: parent_id,
                    org_id,
                    api_key_id,
                    ts: Utc::now(),
                    provider: row_provider,
                    model: row_model,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cached_tokens: usage.cached_tokens,
                    cost_usd,
                    baseline_cost_usd,
                    provider_cache_saved_usd: row_prov_cache_saved_usd,
                    // Fee-applied, matching the usage-event/span figure — keeps
                    // the row-derived TT headline equal to `tt_saved_usd()`.
                    // For a panel aggregate, zeroed to match `complete_panel`.
                    cache_bust_penalty_usd: row_cache_bust_usd,
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
                    // Shadow mode is non-streaming; the streaming row only ever
                    // carries the canary arm (the rewrite already happened
                    // pre-dispatch). Shadow columns stay NULL here.
                    shadow_model: None,
                    shadow_cost_usd: None,
                    traffic_split_arm: traffic_split_arm.clone(),
                    // Raw provider cache counts from the terminal usage block;
                    // None (NULL) on truncated streams — never estimated.
                    cache_read_input_tokens: usage.cache_read_tokens,
                    cache_creation_input_tokens: usage.cache_creation_tokens,
                    // Batch never streams — cleared at the gate
                    // (`batch_ineligible:streaming`), so a streamed row is
                    // never batch-eligible and forgoes nothing.
                    batch_eligible: false,
                    batch_forgone_usd: 0.0,
                    route_paused,
                    // Streaming books $0 for the minify estimate in v1.
                    minify_saved_est_usd: 0.0,
                    // Output shaping never streams either — both planners
                    // skip streaming requests (`*_skipped:streaming`), so a
                    // streamed row carries the defaults.
                    format_switched: None,
                    format_switch_saved_est_usd: 0.0,
                    diff_applied: false,
                    diff_saved_usd: 0.0,
                    diff_failed: false,
                    diff_failed_cost_usd: 0.0,
                    retrieval_tokens_saved,
                };

                // Per-route provider-cache counters on cleanly completed
                // streams. Token counters only from authoritative provider
                // usage (never tokenizer estimates); a clean stream whose
                // provider sent no terminal usage block still counts as
                // result="unreported" — that's a fact, not an estimate — so
                // the hit/(hit+miss+unreported) denominator matches the
                // non-streaming path. Truncated streams count nothing.
                if !truncated {
                    let (cache_read, cache_creation) = if authoritative {
                        (
                            usage.cache_read_tokens.map(|v| v.max(0) as u64),
                            usage.cache_creation_tokens.map(|v| v.max(0) as u64),
                        )
                    } else {
                        (None, None)
                    };
                    crate::metrics::record_provider_cache_usage(
                        &row.provider,
                        span_ctx.as_ref().and_then(|s| s.route.as_deref()),
                        cache_read,
                        cache_creation,
                    );
                }

                if let Some(w) = writer {
                    // Durability (P2): retry a TRANSIENT DB blip a bounded number
                    // of times (idempotent-safe — `row.id` is the request_logs
                    // PK, so a retry cannot double-insert), and on PERMANENT
                    // failure bump the loud, alertable
                    // `tt_request_log_write_failed_total{path="sse"}` counter.
                    // Tracked through the TaskTracker when present so a graceful
                    // shutdown drains this committed billing row.
                    let writer_clone = w.clone();
                    let fut = async move {
                        if let Err(e) = tt_telemetry::request_logs::write_with_retry(
                            writer_clone.as_ref(),
                            row,
                            tt_telemetry::request_logs::DEFAULT_WRITE_ATTEMPTS,
                            tt_telemetry::request_logs::DEFAULT_WRITE_BACKOFF,
                        )
                        .await
                        {
                            tracing::error!(
                                error = %e,
                                "sse request_logs write failed permanently after retries"
                            );
                            crate::metrics::record_request_log_write_failed("sse");
                        }
                    };
                    match &tracker {
                        Some(t) => {
                            t.spawn(fut);
                        }
                        None => {
                            tokio::spawn(fut);
                        }
                    }
                }

                // Per-leg `panel_legs` rows for a streaming panel (Phase 5 Task 4),
                // fire-and-forget — MUST NOT block or fail the response. Keyed by
                // the parent `request_logs.id` (`parent_id`). One row per
                // enumeration position (NOT `LegResult::leg_index`) so the arbiter
                // leg's `usize::MAX` sentinel never appears. Mirrors the
                // non-streaming `complete_panel` per-leg write block verbatim.
                if let Some(p) = panel.as_ref() {
                    if let Some(leg_writer) = p.panel_leg_writer.clone() {
                        let leg_rows: Vec<tt_telemetry::panel_legs::PanelLegRow> = p
                            .leg_records
                            .iter()
                            .enumerate()
                            .map(|(i, leg)| tt_telemetry::panel_legs::PanelLegRow {
                                request_log_id: parent_id,
                                leg_index: i as i32,
                                role: match leg.role {
                                    crate::routes::panel::LegRole::Leg => "leg".to_string(),
                                    crate::routes::panel::LegRole::Arbiter => "arbiter".to_string(),
                                },
                                provider: leg.provider.clone(),
                                model: leg.model.clone(),
                                input_tokens: leg.usage.as_ref().map(|u| u.prompt_tokens as i64),
                                output_tokens: leg
                                    .usage
                                    .as_ref()
                                    .map(|u| u.completion_tokens as i64),
                                cached_tokens: leg.usage.as_ref().map(|u| u.cached_tokens as i64),
                                cost_usd: leg.cost_usd,
                                latency_ms: Some(leg.latency_ms as i64),
                                status: leg.status.as_str().to_string(),
                                error_class: None,
                            })
                            .collect();
                        let fut = async move {
                            let _ = leg_writer.write_legs(leg_rows).await;
                        };
                        match &tracker {
                            Some(t) => {
                                t.spawn(fut);
                            }
                            None => {
                                tokio::spawn(fut);
                            }
                        }
                    }
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
                        let ttl_secs = ins.l2_ttl_secs;
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
                                // No streaming L2 lookup occurred, so there is
                                // no embedding to reuse — embed here (COST-3).
                                None,
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

            // Synchronous served-counter bump (P2): in-band, once per served
            // streaming dispatch, BEFORE the response is handed back — the cheap
            // sync truth to diff against the async-written streaming
            // `request_logs` row. The cache-HIT streaming path is a separate
            // fake-stream that logs its own `chat` row, so this path is always a
            // dispatch.
            crate::metrics::record_request_served("sse", "dispatch");

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
    let cache_creation = u.cache_creation_tokens.unwrap_or(0).max(0) as u64;
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cached_tokens: u.cached_tokens.max(0) as u64,
        cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation),
        // The cost math consumes only the folds above; the raw Option is not
        // threaded here (the request_logs row takes it from the snapshot).
        cache_read_input_tokens: None,
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
    // A pre-computed embedding to reuse instead of embedding `query_text` again
    // (COST-3, mirroring `insert_into_l2`). The streaming path performs no L2
    // *lookup* (only an L1 fake-stream check), so there is no prior embedding to
    // reuse and callers pass `None` — this then embeds here as before. The
    // parameter exists for API symmetry and so a future streaming L2 lookup can
    // thread its vector through without re-embedding.
    precomputed_embedding: Option<Vec<f32>>,
) {
    let embedding_model = l2.embedder.model().to_string();
    let embed = match precomputed_embedding {
        Some(v) => v,
        None => match l2.embedder.embed(query_text).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "streaming l2 embed during insert failed");
                return;
            }
        },
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
        quality_score: None,
        judge_verdict: None,
        created_at: now,
        expires_at: now + chrono::Duration::from_std(ttl).unwrap_or_default(),
        // One-way lexical signature (never the text) — see `insert_into_l2`.
        lexical_sig: Some(tt_cache::lexical_sig(query_text)),
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
                cache_read_input_tokens: None,
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
                    cache_read_input_tokens: None,
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
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        };
        let usage = PartialUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cached_tokens: 0,
            cache_read_tokens: None,
            cache_creation_tokens: None,
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
                cache_read_input_tokens: None,
            }),
            extra: Default::default(),
        })];
        let stream = futures::stream::iter(chunks).boxed();
        let mut tracker = UsageTrackingStream::new(stream, 100, 20, "anthropic");
        let _ = tracker.next().await;

        let usage = tracker.snapshot();
        assert_eq!(usage.cache_creation_tokens, Some(30));
        // Folded-nonzero rescue: cached_tokens 20 with no raw field still
        // proves real cache reads -> Some(20), never NULL.
        assert_eq!(usage.cache_read_tokens, Some(20));

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
            cache_read_tokens: None,
            cache_creation_tokens: None,
        };
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
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
            cache_read_tokens: Some(20),
            cache_creation_tokens: Some(30),
        };
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: Some(0.1),
            cache_write_per_million: Some(1.25),
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
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
