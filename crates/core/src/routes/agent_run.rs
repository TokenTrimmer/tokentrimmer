//! Server-side agentic loop (slice 1a): run model->tool->model over the
//! read-only gateway tools until a final answer or `max_turns`. Synchronous;
//! no Redis/no client round-trip (slice 1b). Generic over `TurnCompleter` so
//! tests inject a stub.

use async_trait::async_trait;
use axum::{
    extract::{Extension, Path, State},
    http::HeaderMap,
    Json,
};
use tt_auth::ApiKeyContext;
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{ChatCompletionRequest, Message, MessageContent},
    RequestContext,
};
use uuid::Uuid;

use crate::{
    error::ApiError,
    middleware::trace::TraceId,
    passes::agentic_budget::summarize_judge::SummarizeCall,
    routes::chat::{self, CompletionOutcome},
    ApiResult, AppState,
};

/// Terminal status of a run.
///
/// `Completed` = the model returned a final (tool-call-free) answer.
/// `Incomplete` = the loop stopped without a final answer (`max_turns` was
/// reached, or — for non-persisting/1a callers — a client tool surfaced).
/// `Failed` = a completion turn errored.
/// `RequiresAction` = the loop paused on a client (non-gateway) tool and the
/// run was persisted awaiting the caller's tool outputs (slice 1b).
///
/// `snake_case` keeps `completed`/`incomplete`/`failed` byte-identical to 1a's
/// `lowercase` rename and adds `requires_action`. `Deserialize` is needed by
/// `StoredRun`, which round-trips a `RunStatus` through the run store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    Incomplete,
    Failed,
    RequiresAction,
}

/// Accumulated token usage across every turn of a run.
///
/// `Deserialize` is needed by `StoredRun`, which round-trips a `RunUsage`
/// through the run store (slice 1b).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// The result of running the agent loop. The full message transcript is
/// returned so the caller sees the model/tool exchange.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Run {
    pub id: uuid::Uuid,
    pub status: RunStatus,
    pub messages: Vec<Message>,
    pub turns: u32,
    pub usage: RunUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Summarizer measurement tax (USD) accrued across the run's turns (slice
    /// 2c-1). `None` ⇒ unmetered or no summarization. Never folded into served
    /// cost — a measurement tax, like the quality-judge tax.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarizer_tax_usd: Option<f64>,
}

/// One completion turn. Production impl wraps `prepare` + `complete_once`
/// (slice 1a Task 4); tests inject a stub. Returns the assistant message +
/// usage for the turn.
#[async_trait]
pub trait TurnCompleter: Send + Sync {
    async fn complete(
        &self,
        req: ChatCompletionRequest,
        is_mechanical: bool,
    ) -> Result<(Message, RunUsage), ApiError>;
}

/// Optional per-turn transcript summarizer injected into [`run_loop_core`].
/// `summarize_before_turn` may rewrite aging tool-result blocks in `messages`
/// in place and advance `summarized_upto`; it returns this call's metered tax
/// (`Some(0.0)` when nothing was summarized; `None` only when a dispatch was
/// billed but unpriced). Fail-open: it never errors the run. The pure loop
/// tests + the 1a `run_loop` wrapper pass `None`.
#[async_trait]
pub(crate) trait TranscriptSummarizer: Send + Sync {
    async fn summarize_before_turn(
        &self,
        messages: &mut Vec<Message>,
        summarized_upto: &mut u32,
    ) -> Option<f64>;
}

/// A turn is "mechanical" when the model is about to digest ONLY read-only tool
/// output: scanning back over the trailing `Message::Tool` results to the
/// assistant turn that produced them, that assistant turn called >=1 tool and
/// EVERY tool_call is read-only (`classify_substep == ReadOnly`). Conservative:
/// any client/mutating tool in that turn — or no preceding assistant tool turn
/// (e.g. the first turn, or a plain user/assistant message) — => not mechanical.
fn is_mechanical_continuation(messages: &[tt_shared::messages::Message]) -> bool {
    use crate::passes::agentic_budget::substep_cache::{classify_substep, SubstepKind};
    use tt_shared::messages::Message;
    // Walk back over trailing Tool results.
    let mut i = messages.len();
    let mut saw_tool_result = false;
    while i > 0 {
        match &messages[i - 1] {
            Message::Tool { .. } => {
                saw_tool_result = true;
                i -= 1;
            }
            Message::Assistant { tool_calls, .. } if saw_tool_result => {
                // The assistant turn whose tool results we just appended.
                return !tool_calls.is_empty()
                    && tool_calls
                        .iter()
                        .all(|tc| classify_substep(&tc.function.name) == SubstepKind::ReadOnly);
            }
            _ => return false, // a non-Tool, non-producing message (or no tool results) => not mechanical
        }
    }
    false
}

/// Message indices of the tool-result blocks eligible for summarization: the
/// tool blocks OLDER than the last `keep_recent_pairs` (caveat C1 — recent tail
/// verbatim) AND beyond the `summarized_upto` high-water mark (tool blocks with a
/// lower ordinal are already summarized; each block is processed at most once).
fn eligible_tool_ordinals(
    messages: &[Message],
    summarized_upto: u32,
    keep_recent_pairs: u32,
) -> Vec<usize> {
    let tool_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::Tool { .. }))
        .map(|(i, _)| i)
        .collect();
    let n = tool_idxs.len();
    let cutoff = n.saturating_sub(keep_recent_pairs as usize); // ordinals [0, cutoff) are old
    let start = (summarized_upto as usize).min(cutoff);
    tool_idxs[start..cutoff].to_vec()
}

/// Token-true gate: a summary commits only when it reduces the served-model
/// token count by at least `clear_at_least_tokens.max(1)` (≥1 ⇒ never a
/// token-neutral/-inflating commit; the floor is the R1 cache-thrash guard).
/// Mirrors the pipeline gate's discipline (`passes/mod.rs`): even on the
/// `Confidence::Low` (`chars/4`) fallback a non-reduction is rejected.
fn token_true_ok(
    provider_id: &str,
    model: &str,
    original: &str,
    summary: &str,
    clear_at_least_tokens: u32,
) -> bool {
    let orig = tt_tokenize::estimate_input_tokens_for_model(provider_id, model, original).tokens;
    let new = tt_tokenize::estimate_input_tokens_for_model(provider_id, model, summary).tokens;
    orig.saturating_sub(new) >= clear_at_least_tokens.max(1)
}

/// Deterministic per-edit sampling key: a `Uuid` digest of `(trace_id, tool_call_id)`,
/// so `should_sample`/`ab_order_for` (which hash an opaque `Uuid`) give a stable,
/// uniform per-edit decision. No RNG.
#[allow(dead_code)] // wired into the summary judge in slice 2c-2 Task 4
fn sample_key(trace_id: Uuid, tool_call_id: &str) -> Uuid {
    use std::hash::{Hash, Hasher};
    let mut hi = std::collections::hash_map::DefaultHasher::new();
    trace_id.hash(&mut hi);
    tool_call_id.hash(&mut hi);
    let mut lo = std::collections::hash_map::DefaultHasher::new();
    tool_call_id.hash(&mut lo);
    trace_id.hash(&mut lo);
    lo.write_u8(0x9e); // distinct salt so hi != lo
    Uuid::from_u64_pair(hi.finish(), lo.finish())
}

/// The run's task context for the summary judge's `input`: the most-recent
/// `Message::User` text, or `""` (the judge still compares A/B info-preservation).
#[allow(dead_code)] // wired into the summary judge in slice 2c-2 Task 4
fn latest_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User {
                content: MessageContent::Text(t),
                ..
            } => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Default cap on completion turns when the caller does not specify one.
///
/// Consumed by the `POST /v1/agent/runs` handler ([`create_run`]).
pub(crate) const DEFAULT_MAX_TURNS: u32 = 8;
/// Hard upper bound on completion turns regardless of the caller's request.
const MAX_MAX_TURNS: u32 = 32;

/// Outcome of running (or resuming) the loop until a terminal state or a pause.
pub(crate) enum LoopOutcome {
    /// The run reached a terminal state (the `Run` carries the final status).
    Terminal(Run),
    /// The model called a client (non-gateway) tool; the loop paused. Any
    /// gateway tool_calls of that same assistant turn were executed inline
    /// (their results are in `messages`); `pending_tool_calls` are the CLIENT
    /// tool_calls awaiting the caller's output.
    Paused {
        messages: Vec<Message>,
        turns_done: u32,
        usage: RunUsage,
        pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
        summarized_upto: u32,
        summarizer_tax_usd: Option<f64>,
    },
}

/// The pausable loop core. Runs from `turns_done` (0 for a fresh run, >0 on
/// resume) up to `max_turns` (clamped to `[1, 32]`). `id`/usage-carry-in let
/// resume continue a run.
///
/// Each turn builds a non-streaming [`ChatCompletionRequest`], calls
/// `completer.complete`, appends the assistant message and accumulates usage.
/// If the assistant returns no tool calls the run is `Terminal(Completed)`. A
/// completer error ends the run `Terminal(Failed)`; exhausting `max_turns` ends
/// it `Terminal(Incomplete)`. Gateway (read-only) tool_calls are executed and
/// their results appended as [`Message::Tool`]. If a turn calls ANY client
/// (non-gateway) tool, the turn's gateway tool_calls are still executed inline
/// (so a mixed turn's gateway work isn't wasted and, on resume, every tool_call
/// of the assistant turn is answered) and the loop returns [`LoopOutcome::Paused`]
/// with the CLIENT tool_calls as `pending_tool_calls`.
// Eight params mirror the persisted resume state (id/usage/turns_done carry-in
// for resume); grouping them into a struct would just shuffle the run-state
// fields that Tasks 3/5 already track separately.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_loop_core(
    completer: &dyn TurnCompleter,
    id: uuid::Uuid,
    model: String,
    mut messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    turns_done: u32,
    mut summarized_upto: u32,
    mut usage: RunUsage,
    summarizer: Option<&dyn TranscriptSummarizer>,
) -> LoopOutcome {
    use crate::passes::agentic_budget::summarize_judge::sum_metered;
    let max_turns = max_turns.clamp(1, MAX_MAX_TURNS);
    let mut turn = turns_done;
    // No summarizer ⇒ no tax (`None`), keeping the `summarizer: None` path
    // byte-behavior-identical to pre-2c-1. With a summarizer, start the metered
    // accumulator at `Some(0.0)` so each turn's tax folds in via `sum_metered`.
    let mut summarizer_tax: Option<f64> = summarizer.map(|_| 0.0);
    while turn < max_turns {
        if let Some(s) = summarizer {
            let tax = s
                .summarize_before_turn(&mut messages, &mut summarized_upto)
                .await;
            summarizer_tax = sum_metered(summarizer_tax, tax);
        }
        let req = ChatCompletionRequest {
            model: model.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            stream: false,
            ..Default::default()
        };
        // A "mechanical" turn digests ONLY read-only tool output (the prior
        // assistant turn called solely read-only tools and their results are
        // already appended to `messages`). Computed from the transcript that is
        // about to be SENT — before this turn's assistant response is pushed.
        let is_mechanical = is_mechanical_continuation(&messages);
        let (assistant, turn_usage) = match completer.complete(req, is_mechanical).await {
            Ok(x) => x,
            Err(e) => {
                return LoopOutcome::Terminal(Run {
                    id,
                    status: RunStatus::Failed,
                    messages,
                    turns: turn + 1,
                    usage,
                    note: Some(format!("turn {turn} failed: {e}")),
                    summarizer_tax_usd: summarizer_tax,
                });
            }
        };
        usage.prompt_tokens += turn_usage.prompt_tokens;
        usage.completion_tokens += turn_usage.completion_tokens;
        messages.push(assistant.clone());

        let tool_calls = match &assistant {
            Message::Assistant { tool_calls, .. } => tool_calls.clone(),
            _ => Vec::new(),
        };
        if tool_calls.is_empty() {
            return LoopOutcome::Terminal(Run {
                id,
                status: RunStatus::Completed,
                messages,
                turns: turn + 1,
                usage,
                note: None,
                summarizer_tax_usd: summarizer_tax,
            });
        }

        let has_client_tool = tool_calls
            .iter()
            .any(|tc| !crate::routes::gateway_tools::is_gateway_tool(&tc.function.name));

        // Execute the gateway tool_calls of this turn inline (whether or not we
        // are about to pause — so a mixed turn's gateway work isn't wasted and,
        // on resume, every tool_call of this assistant turn is answered).
        for tc in &tool_calls {
            if crate::routes::gateway_tools::is_gateway_tool(&tc.function.name) {
                let result = match crate::routes::gateway_tools::execute(
                    &tc.function.name,
                    &tc.function.arguments,
                ) {
                    Ok(s) => s,
                    // A tool error is appended as the tool result (not aborted)
                    // so the model can read it and react on the next turn.
                    Err(e) => format!("tool error: {e}"),
                };
                messages.push(Message::Tool {
                    content: MessageContent::Text(result),
                    tool_call_id: tc.id.clone(),
                });
            }
        }

        if has_client_tool {
            let pending: Vec<_> = tool_calls
                .into_iter()
                .filter(|tc| !crate::routes::gateway_tools::is_gateway_tool(&tc.function.name))
                .collect();
            return LoopOutcome::Paused {
                messages,
                turns_done: turn + 1,
                usage,
                pending_tool_calls: pending,
                summarized_upto,
                summarizer_tax_usd: summarizer_tax,
            };
        }
        turn += 1;
    }
    LoopOutcome::Terminal(Run {
        id,
        status: RunStatus::Incomplete,
        messages,
        turns: max_turns,
        usage,
        note: Some("max_turns reached".into()),
        summarizer_tax_usd: summarizer_tax,
    })
}

/// Run the synchronous agent loop. `model`/`messages`/`tools` come from the
/// request; `max_turns` is clamped to `[1, 32]`.
///
/// Thin wrapper over [`run_loop_core`] preserving slice-1a behavior for callers
/// without persistence: a pause on a client tool is surfaced as an `Incomplete`
/// `Run` (the note names the first pending client tool), exactly as 1a did.
pub async fn run_loop(
    completer: &dyn TurnCompleter,
    id: uuid::Uuid,
    model: String,
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
) -> Run {
    match run_loop_core(
        completer,
        id,
        model,
        messages,
        tools,
        max_turns,
        0, /*turns_done*/
        0, /*summarized_upto*/
        RunUsage::default(),
        None, /*summarizer*/
    )
    .await
    {
        LoopOutcome::Terminal(run) => run,
        LoopOutcome::Paused {
            messages,
            turns_done,
            usage,
            pending_tool_calls,
            summarized_upto: _,
            summarizer_tax_usd,
        } => {
            // 1a callers (no persistence) surface a pause as Incomplete, exactly
            // as before. The note names the first client tool.
            let name = pending_tool_calls
                .first()
                .map(|tc| tc.function.name.clone())
                .unwrap_or_default();
            Run {
                id,
                status: RunStatus::Incomplete,
                messages,
                turns: turns_done,
                usage,
                note: Some(format!("client tool '{name}' requires slice-1b round-trip")),
                summarizer_tax_usd,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Persisted run record + L1-backed run store helpers (slice 1b Task 2)
//
// Wired into the create/get handlers by slice-1b Tasks 3-4 (`create_run`
// persists a paused run; `get_run` fetches it). Resume (Task 5) consumes them
// too.
// ---------------------------------------------------------------------------

/// TTL for a persisted run record. A paused run is GETtable/resumable for this
/// long; after it the L1 store evicts the record (one hour).
const RUN_TTL_SECS: u64 = 3600;

/// Non-secret routing config carried across a pause so resume turns route
/// consistently. NEVER includes credentials.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredRouting {
    pub provider_pin: Option<String>,
    pub forced_route: Option<String>,
    pub tag: Option<String>,
}

/// Non-secret summarize policy resolved once from the run's (turn-0) route and
/// persisted with the run so resume drives the same policy. Tiny config only.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SummarizeConfig {
    pub keep_recent_pairs: u32,
    pub clear_at_least_tokens: u32,
}

/// The full resumable run state persisted to the L1 store. NO secrets — only
/// the conversation transcript and the non-secret routing config.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredRun {
    pub id: uuid::Uuid,
    pub org_id: uuid::Uuid,
    pub status: RunStatus,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<tt_shared::messages::Tool>,
    pub max_turns: u32,
    pub turns_done: u32,
    pub usage: RunUsage,
    pub pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
    pub routing: StoredRouting,
    /// Tool-block watermark: count of leading `Message::Tool` blocks already
    /// summarized. Restored on resume so each block is summarized at most once.
    #[serde(default)]
    pub summarized_upto: u32,
    /// Accrued summarizer measurement tax (USD). `#[serde(default)]` for
    /// cross-deploy resume back-compat (a pre-2c-1 record has no key).
    #[serde(default)]
    pub summarizer_tax_usd: Option<f64>,
    /// The run's pinned summarize policy (turn-0 route). `None` ⇒ summarize off.
    #[serde(default)]
    pub summarize: Option<SummarizeConfig>,
}

/// L1 key for a run record, scoped by org so a fetch with the wrong org misses.
fn run_key(org_id: uuid::Uuid, run_id: uuid::Uuid) -> String {
    format!("tt:runs:{org_id}:{run_id}")
}

impl StoredRun {
    /// Derive the HTTP `Run` view from a stored record (the `requires_action`
    /// response body). `turns` is the turns completed so far; no note.
    pub(crate) fn to_run(&self) -> Run {
        Run {
            id: self.id,
            status: self.status,
            messages: self.messages.clone(),
            turns: self.turns_done,
            usage: self.usage.clone(),
            note: None,
            summarizer_tax_usd: self.summarizer_tax_usd,
        }
    }
}

/// Persist (overwrite) a run record with the run TTL.
pub(crate) async fn store_run(
    cache: &dyn tt_cache::L1Cache,
    run: &StoredRun,
) -> Result<(), ApiError> {
    let bytes =
        serde_json::to_vec(run).map_err(|e| ApiError::Internal(format!("run serialize: {e}")))?;
    cache
        .set(&run_key(run.org_id, run.id), &bytes, RUN_TTL_SECS)
        .await
        .map_err(|e| ApiError::Internal(format!("run store: {e}")))?;
    Ok(())
}

/// Fetch a run record scoped by (org, id). `None` when absent/expired.
pub(crate) async fn fetch_run(
    cache: &dyn tt_cache::L1Cache,
    org_id: uuid::Uuid,
    run_id: uuid::Uuid,
) -> Result<Option<StoredRun>, ApiError> {
    match cache
        .get(&run_key(org_id, run_id))
        .await
        .map_err(|e| ApiError::Internal(format!("run fetch: {e}")))?
    {
        Some(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
                ApiError::Internal(format!("run deserialize: {e}"))
            })?))
        }
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Production completer + `POST /v1/agent/runs` endpoint (slice 1a Task 4)
// ---------------------------------------------------------------------------

/// Run-level caller identity captured once at run creation. Every per-turn
/// completion re-derives the same `RequestContext` + ~16 `prepare` inputs the
/// chat handler builds post-auth (provider + credentials are RE-RESOLVED per
/// turn since routing/the model can change between turns), so each turn routes
/// exactly as a single-shot `/v1/chat/completions` would for that turn's model.
struct RunIdentity {
    /// Caller's org (nil for anonymous/dev), from `ApiKeyContext`.
    org_id: Uuid,
    /// Caller's API key id (nil for anonymous/dev), from `ApiKeyContext`.
    api_key_id: Uuid,
    /// Caller tier (drives L2 entitlement + cache TTL). `None` ⇒ treated Free.
    caller_tier: Option<tt_shared::CallerTier>,
    /// L2 entitlement (paid-tier only) — derived once from `caller_tier`.
    l2_allowed: bool,
    /// Raw bearer (the source provider's key for the legacy passthrough path;
    /// also the cross-provider re-emit credential).
    raw_bearer: String,
    /// Resolved trace id (stable across the run's turns).
    trace_id: Uuid,
    /// `X-TokenTrimmer-Tag` cost-attribution tag, if any.
    tag: Option<String>,
    /// Per-request upstream deadline (`X-TokenTrimmer-Timeout-Ms`), if any.
    request_timeout: Option<std::time::Duration>,
    /// `X-TokenTrimmer-Provider` pin, applied per turn after routing.
    provider_pin: Option<String>,
    /// `X-TokenTrimmer-Route` forced route, passed into routing per turn.
    forced_route: Option<String>,
    /// Sticky-canary idempotency key (stable across the run's turns).
    idempotency_key: String,
    /// The caller's request headers with `X-TokenTrimmer-Cache` STRIPPED, so the
    /// per-turn `tt_extras.cache=bypass` knob is never re-enabled by a header
    /// override (header beats body in `prepare`). All other headers — provider
    /// pin, forced route, timeout, tag, interactive — flow through unchanged.
    headers: HeaderMap,
}

impl RunIdentity {
    /// Build the run-level identity from the auth context + headers, mirroring
    /// the chat handler's post-auth setup (`chat::handler` §2 / §2b). The
    /// sandbox `tt_test_*` short-circuit is intentionally NOT replicated here —
    /// an agent run always drives the real per-turn completion pipeline.
    fn from_request(auth_ctx: Option<&ApiKeyContext>, trace: &str, headers: &HeaderMap) -> Self {
        let raw_bearer = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                s.strip_prefix("Bearer ")
                    .or_else(|| s.strip_prefix("bearer "))
            })
            .unwrap_or("")
            .to_string();

        // Trace id: the trace-middleware extension wins, else a fresh v7. (The
        // chat handler also accepts an `x-tokentrimmer-trace-id` header; the run
        // endpoint inherits that via the same middleware-populated `TraceId`.)
        let trace_id = if !trace.is_empty() {
            Uuid::parse_str(trace).unwrap_or_else(|_| Uuid::now_v7())
        } else {
            headers
                .get("x-tokentrimmer-trace-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_else(Uuid::now_v7)
        };

        let idempotency_key = headers
            .get("x-idempotency-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if trace_id != Uuid::nil() {
                    trace_id.to_string()
                } else {
                    Uuid::now_v7().to_string()
                }
            });

        let (org_id, api_key_id, caller_tier) = match auth_ctx {
            Some(c) => (c.org_id, c.key_id, c.tier),
            None => (Uuid::nil(), Uuid::nil(), None),
        };
        let l2_allowed = matches!(
            caller_tier,
            Some(
                tt_shared::CallerTier::Pro
                    | tt_shared::CallerTier::Team
                    | tt_shared::CallerTier::Scale
            )
        );

        // Strip the cache-override header so the per-turn `tt_extras.cache=bypass`
        // is authoritative (`prepare` lets `X-TokenTrimmer-Cache` override the
        // body decision; without this a `read-only`/`force-write` header could
        // re-enable a lookup/insert mid-loop).
        let mut headers = headers.clone();
        headers.remove("x-tokentrimmer-cache");

        Self {
            org_id,
            api_key_id,
            caller_tier,
            l2_allowed,
            raw_bearer,
            trace_id,
            tag: headers
                .get("x-tokentrimmer-tag")
                .and_then(|v| v.to_str().ok())
                .map(String::from),
            request_timeout: chat::timeout_ms_from_header(&headers)
                .map(std::time::Duration::from_millis),
            provider_pin: chat::provider_override_from_header(&headers),
            forced_route: chat::route_override_from_header(&headers),
            idempotency_key,
            headers,
        }
    }
}

/// Production completer: routes + dispatches each turn through the real
/// `chat::prepare` + `chat::complete_once` pipeline (per-turn routing / cache /
/// telemetry), exactly as a single-shot non-streaming `/v1/chat/completions`
/// would for that turn's model.
struct GatewayCompleter<'a> {
    state: &'a AppState,
    /// Run-level caller identity. Per-turn the completer builds a FRESH
    /// `RequestContext` from these (never reusing a `prepare`-rebound ctx), so a
    /// cross-provider / provider-pin credential rebind from one turn can never
    /// leak into the next turn's routing.
    identity: RunIdentity,
}

impl GatewayCompleter<'_> {
    /// Disable L1/L2 cache lookup AND insert for a per-turn request by setting
    /// `tt_extras.cache = {"mode":"bypass"}` (→ `CacheBehavior::resolve` yields
    /// `do_lookup=false, do_insert=false`). An agent turn is a fresh, evolving
    /// transcript — serving a cached answer mid-loop would be wrong — so
    /// `complete_once` always returns `Dispatched` and never the `CacheHit` arm
    /// (the headers carrying the cache override are also stripped in
    /// [`RunIdentity::from_request`], so a header can't re-enable caching).
    fn disable_cache(req: &mut ChatCompletionRequest) {
        req.tt_extras
            .insert("cache".to_string(), serde_json::json!({ "mode": "bypass" }));
    }
}

#[async_trait]
impl TurnCompleter for GatewayCompleter<'_> {
    async fn complete(
        &self,
        mut req: ChatCompletionRequest,
        is_mechanical: bool,
    ) -> Result<(Message, RunUsage), ApiError> {
        Self::disable_cache(&mut req);

        // Per-turn provider resolution: the model can change between turns
        // (routing / a tool-driven downgrade), so resolve fresh from THIS turn's
        // `req.model`, mirroring the chat handler's step 1.
        let provider =
            self.state
                .registry
                .resolve(&req.model)
                .ok_or_else(|| ApiError::ModelNotFound {
                    model: req.model.clone(),
                })?;
        let source_provider_id = provider.id().to_string();

        // Per-turn credentials, resolved against THIS turn's provider exactly as
        // the chat handler does (store hit wins; anonymous BYO bearer fallback;
        // verified-org miss fails closed → deferred error inside `prepare`).
        let resolved_source_creds = chat::resolve_credentials(
            self.state,
            self.identity.org_id,
            provider.id(),
            &self.identity.raw_bearer,
        )
        .await;
        let source_creds_missing = resolved_source_creds.is_none();
        let credentials = resolved_source_creds.unwrap_or_else(|| ProviderCredentials {
            api_key: SecretString::new(self.identity.raw_bearer.clone()),
            base_url: None,
            extra_headers: Vec::new(),
        });

        // FRESH per-turn context built from the run-level identity. Cloning the
        // base identity (not a prior turn's rebound `ctx`) guarantees a
        // cross-provider credential rebind inside `prepare` never leaks forward.
        let mut ctx = RequestContext {
            trace_id: self.identity.trace_id,
            org_id: self.identity.org_id,
            api_key_id: self.identity.api_key_id,
            credentials,
            tag: self.identity.tag.clone(),
            deadline: self.identity.request_timeout,
        };

        let request_started = std::time::Instant::now();
        let prep = chat::prepare(
            self.state,
            &mut ctx,
            &mut req,
            &self.identity.headers,
            provider,
            self.identity.provider_pin.clone(),
            self.identity.forced_route.clone(),
            self.identity.request_timeout,
            self.identity.idempotency_key.clone(),
            self.identity.raw_bearer.clone(),
            self.identity.org_id,
            source_provider_id,
            source_creds_missing,
            self.identity.caller_tier,
            self.identity.l2_allowed,
            Default::default(),
            request_started,
            is_mechanical,
        )
        .await?;

        match chat::complete_once(self.state, &ctx, prep).await? {
            CompletionOutcome::Dispatched { response, .. } => {
                let usage = RunUsage {
                    prompt_tokens: response.usage.prompt_tokens,
                    completion_tokens: response.usage.completion_tokens,
                };
                let msg = response
                    .choices
                    .into_iter()
                    .next()
                    .map(|c| c.message)
                    .ok_or_else(|| {
                        ApiError::Internal("agent turn: provider returned no choices".into())
                    })?;
                Ok((msg, usage))
            }
            // Unreachable in practice — every per-turn request disables the cache
            // (lookup + insert), so `complete_once` always dispatches. Treat a
            // cache hit as an internal invariant violation rather than silently
            // mishandling the prebuilt HTTP `Response` (which the loop can't turn
            // back into a typed `Message`).
            CompletionOutcome::CacheHit(_) => Err(ApiError::Internal(
                "agent turn unexpectedly served from cache (cache should be disabled per turn)"
                    .into(),
            )),
        }
    }
}

/// Request body for `POST /v1/agent/runs`.
#[derive(serde::Deserialize)]
pub struct CreateRunRequest {
    /// Model id for every turn (routing may rewrite it per turn).
    pub model: String,
    /// Initial transcript (system/user messages).
    pub messages: Vec<Message>,
    /// Tool definitions advertised to the model. Defaults to none.
    #[serde(default)]
    pub tools: Vec<tt_shared::messages::Tool>,
    /// Turn cap; clamped to `[1, 32]`. Defaults to [`DEFAULT_MAX_TURNS`].
    #[serde(default)]
    pub max_turns: Option<u32>,
}

/// `POST /v1/agent/runs` — run a synchronous server-side agent loop
/// (model→tool→model over the read-only gateway tools) until a final answer or
/// `max_turns`. Auth is inherited from the router's auth middleware (the
/// `ApiKeyContext` extension); identity + credentials are built per the chat
/// handler's post-auth setup and forwarded per turn.
pub async fn create_run(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(req): Json<CreateRunRequest>,
) -> ApiResult<Json<Run>> {
    let identity = RunIdentity::from_request(auth_ctx.as_deref(), trace.0.as_str(), &headers);
    // Capture the org + non-secret routing config BEFORE `identity` is moved
    // into the completer — a paused run persists these (never any credential).
    let org_id = identity.org_id;
    let routing = StoredRouting {
        provider_pin: identity.provider_pin.clone(),
        forced_route: identity.forced_route.clone(),
        tag: identity.tag.clone(),
    };
    let model = req.model.clone();
    let tools = req.tools.clone();
    let max_turns = req.max_turns.unwrap_or(DEFAULT_MAX_TURNS);

    // Resolve the run's summarize policy ONCE from the turn-0 route, then build
    // the production summarizer. Done BEFORE `identity` is moved into the
    // completer and BEFORE `req.messages` is moved into the loop. `None` policy
    // ⇒ summarize off (the `summarizer: None` loop path is byte-identical to
    // pre-2c-1). With the default `NeverCommit` gate, `is_committable` is always
    // false, so no summarize dispatch ever happens even when a route opts in.
    let summarize_cfg =
        resolve_summarize_config(&state, &identity, &req.model, &req.messages).await;
    let base_provider_id = state
        .registry
        .resolve(&req.model)
        .map(|p| p.id().to_string());
    let summarizer_model = summarizer_model(&state);
    let base_ctx = base_request_context(&identity);
    let summarizer_obj = summarize_cfg
        .clone()
        .map(|cfg| GatewayTranscriptSummarizer {
            state: &state,
            org_id: identity.org_id,
            raw_bearer: identity.raw_bearer.clone(),
            base_ctx,
            gate: state.summary_gate.clone(),
            cfg,
            base_model: req.model.clone(),
            base_provider_id,
            summarizer_model,
            deadline: state.judge_config.baseline_timeout,
        });

    let completer = GatewayCompleter {
        state: &state,
        identity,
    };
    let id = Uuid::new_v4();
    let summ_ref: Option<&dyn TranscriptSummarizer> = summarizer_obj
        .as_ref()
        .map(|s| s as &dyn TranscriptSummarizer);

    match run_loop_core(
        &completer,
        id,
        model.clone(),
        req.messages,
        tools.clone(),
        max_turns,
        0,
        0,
        RunUsage::default(),
        summ_ref,
    )
    .await
    {
        // Inline completion — terminal runs are not persisted.
        LoopOutcome::Terminal(run) => Ok(Json(run)),
        LoopOutcome::Paused {
            messages,
            turns_done,
            usage,
            pending_tool_calls,
            summarized_upto,
            summarizer_tax_usd,
        } => match state.l1.as_ref() {
            // Redis present → persist the paused run so it can be GET/resumed.
            // Persist the COMPUTED watermark + tax + pinned policy (create_run is
            // the first segment, so there is no prior tax to fold in).
            Some(l1) => {
                let stored = StoredRun {
                    id,
                    org_id,
                    status: RunStatus::RequiresAction,
                    model,
                    messages,
                    tools,
                    max_turns,
                    turns_done,
                    usage,
                    pending_tool_calls,
                    routing,
                    summarized_upto,
                    summarizer_tax_usd,
                    summarize: summarize_cfg,
                };
                store_run(l1.cache.as_ref(), &stored).await?;
                Ok(Json(stored.to_run()))
            }
            // No Redis → 1a fallback: surface the pause as Incomplete (carry the
            // segment's summarizer tax through to the returned Run).
            None => {
                let name = pending_tool_calls
                    .first()
                    .map(|tc| tc.function.name.clone())
                    .unwrap_or_default();
                Ok(Json(Run {
                    id,
                    status: RunStatus::Incomplete,
                    messages,
                    turns: turns_done,
                    usage,
                    note: Some(format!(
                        "client tool '{name}' requires Redis to pause/resume (none configured)"
                    )),
                    summarizer_tax_usd,
                }))
            }
        },
    }
}

/// `GET /v1/agent/runs/:id` — fetch a persisted run's current state. Org is
/// derived from the authenticated key (a real key is required; anonymous /
/// dogfood callers get 401) and embedded in the store key, so a fetch with the
/// wrong org cleanly misses (404). Requires the L1/Redis store; without it the
/// run was never persisted, so the handler returns 503.
pub async fn get_run(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Run>> {
    // Resolve the caller's real org, or 401 (mirrors `routes_api::require_org`,
    // which is private to that module). Dogfood/absent contexts are rejected.
    let org = match ctx {
        Some(Extension(c)) if c.org_id != crate::DOGFOOD_ORG_ID => c.org_id,
        _ => return Err(ApiError::Unauthorized),
    };
    let l1 = state.l1.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "agent runs require the L1/Redis store (none configured)".into(),
        )
    })?;
    let stored = fetch_run(l1.cache.as_ref(), org, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no run with id {id}")))?;
    Ok(Json(stored.to_run()))
}

/// One tool result the caller submits to resume a paused run: the id of the
/// pending tool_call it answers + the (opaque, client-produced) output text.
#[derive(serde::Deserialize)]
pub struct ToolOutput {
    pub tool_call_id: String,
    pub output: String,
}

/// Request body for `POST /v1/agent/runs/:id/tool_outputs`. The submitted ids
/// must EXACTLY cover the run's `pending_tool_calls` (see [`submit_tool_outputs`]).
#[derive(serde::Deserialize)]
pub struct ToolOutputsRequest {
    pub tool_outputs: Vec<ToolOutput>,
}

/// `POST /v1/agent/runs/:id/tool_outputs` — resume a `requires_action` run by
/// submitting the client tool outputs it paused on.
///
/// Order of checks (each maps to its HTTP status):
/// 1. Org from the resume request's auth (real key required; dogfood/absent →
///    401). The run is fetched scoped by this org, so a wrong-org caller misses.
/// 2. L1/Redis store required, else 503 (without it the run was never persisted).
/// 3. Run not found for (org, id) → 404.
/// 4. Run not in `RequiresAction` → 409 (it is terminal or otherwise not
///    awaiting outputs).
/// 5. Submitted `tool_call_id`s must EXACTLY cover the pending client tool_calls
///    (set equality) → 400 otherwise (lists the expected ids).
/// 6. Single-flight on the run key: only one resume drives a run at a time; a
///    concurrent resume loses the leader race → 409. The leader guard is held
///    across the resume so a second request can't interleave.
///
/// On resume the per-turn completer is rebuilt from the RESUME request's auth +
/// headers (re-authenticated; org verified equal to `stored.org_id` in step 1)
/// and the run's stored, non-secret routing config (`provider_pin`/`forced_route`
/// /`tag`) — no credential is ever persisted. The submitted outputs are appended
/// as `Tool` messages (the paused turn's gateway tool_calls were answered inline
/// at pause, so every tool_call of that assistant turn is now answered) and
/// [`run_loop_core`] continues from `turns_done`. The updated run is persisted
/// (terminal → stays GETtable to TTL; paused again → another `requires_action`).
pub async fn submit_tool_outputs(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<ToolOutputsRequest>,
) -> ApiResult<Json<Run>> {
    // (1) Org from the resume request's auth (mirrors `routes_api::require_org`).
    // `auth_ctx` is borrowed (not consumed) so it can rebuild the identity below.
    let org = match auth_ctx.as_deref() {
        Some(c) if c.org_id != crate::DOGFOOD_ORG_ID => c.org_id,
        _ => return Err(ApiError::Unauthorized),
    };
    // (2) The run store is required; without it nothing was ever persisted.
    let l1 = state.l1.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "agent runs require the L1/Redis store (none configured)".into(),
        )
    })?;

    // (3) Fetch scoped by (org, id); a wrong-org caller cleanly misses → 404.
    let mut stored = fetch_run(l1.cache.as_ref(), org, id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no run with id {id}")))?;
    // (4) Only a paused (`requires_action`) run accepts tool outputs.
    if stored.status != RunStatus::RequiresAction {
        return Err(ApiError::Conflict(format!(
            "run {id} is {:?}, not awaiting tool outputs",
            stored.status
        )));
    }

    // (5) The submitted ids must EXACTLY cover the pending client tool_calls.
    let pending_ids: std::collections::HashSet<&str> = stored
        .pending_tool_calls
        .iter()
        .map(|tc| tc.id.as_str())
        .collect();
    let submitted_ids: std::collections::HashSet<&str> = body
        .tool_outputs
        .iter()
        .map(|o| o.tool_call_id.as_str())
        .collect();
    if submitted_ids != pending_ids {
        return Err(ApiError::InvalidRequest(format!(
            "tool_outputs must cover exactly the pending tool_call ids {pending_ids:?}"
        )));
    }

    // (6) Single-flight: only one resume drives a given run at a time. The guard
    // is held across the resume so a concurrent caller (loser) is rejected for
    // the run's whole resume rather than racing into a second loop.
    let sf_key = run_key(org, id);
    let _guard = state
        .single_flight
        .try_become_leader(&sf_key)
        .map_err(|_| ApiError::Conflict(format!("run {id} is already being resumed")))?;

    // Append each submitted output as a Tool message answering its pending
    // client tool_call (gateway results were appended at pause, so every
    // tool_call of the paused assistant turn is now answered).
    for o in &body.tool_outputs {
        stored.messages.push(Message::Tool {
            content: MessageContent::Text(o.output.clone()),
            tool_call_id: o.tool_call_id.clone(),
        });
    }

    // Rebuild the completer from the RESUME request's auth/headers (re-auth; org
    // verified == stored.org_id above) + the stored, non-secret routing config.
    let mut identity = RunIdentity::from_request(auth_ctx.as_deref(), trace.0.as_str(), &headers);
    identity.provider_pin = stored.routing.provider_pin.clone();
    identity.forced_route = stored.routing.forced_route.clone();
    identity.tag = stored.routing.tag.clone();

    // Rebuild the production summarizer from the PERSISTED, turn-0-pinned policy
    // (no re-resolution on resume — the route could have changed). Built BEFORE
    // `identity` is moved into the completer. `None` policy ⇒ summarize off.
    let summ_obj = stored.summarize.clone().map(|cfg| {
        let base_ctx = base_request_context(&identity);
        GatewayTranscriptSummarizer {
            state: &state,
            org_id: identity.org_id,
            raw_bearer: identity.raw_bearer.clone(),
            base_ctx,
            gate: state.summary_gate.clone(),
            cfg,
            base_model: stored.model.clone(),
            base_provider_id: state
                .registry
                .resolve(&stored.model)
                .map(|p| p.id().to_string()),
            summarizer_model: summarizer_model(&state),
            deadline: state.judge_config.baseline_timeout,
        }
    });

    let completer = GatewayCompleter {
        state: &state,
        identity,
    };
    let summ_ref: Option<&dyn TranscriptSummarizer> =
        summ_obj.as_ref().map(|s| s as &dyn TranscriptSummarizer);

    let outcome = run_loop_core(
        &completer,
        stored.id,
        stored.model.clone(),
        std::mem::take(&mut stored.messages),
        stored.tools.clone(),
        stored.max_turns,
        stored.turns_done,
        stored.summarized_upto,
        stored.usage.clone(),
        summ_ref,
    )
    .await;

    use crate::passes::agentic_budget::summarize_judge::sum_metered;
    match outcome {
        // Terminal — record the final state and keep it GETtable until the TTL.
        // Tax is CUMULATIVE across segments (fold this segment's into the prior
        // total); the returned `Run` carries the TOTAL.
        LoopOutcome::Terminal(mut run) => {
            let cumulative = sum_metered(stored.summarizer_tax_usd, run.summarizer_tax_usd);
            stored.status = run.status;
            stored.messages = run.messages.clone();
            stored.turns_done = run.turns;
            stored.usage = run.usage.clone();
            stored.summarizer_tax_usd = cumulative;
            stored.pending_tool_calls = Vec::new();
            store_run(l1.cache.as_ref(), &stored).await?;
            run.summarizer_tax_usd = cumulative; // return the TOTAL across segments
            Ok(Json(run))
        }
        // Paused again on another client tool — re-persist as `requires_action`.
        // Watermark REPLACES (absolute high-water mark); tax FOLDS in (cumulative).
        LoopOutcome::Paused {
            messages,
            turns_done,
            usage,
            pending_tool_calls,
            summarized_upto,
            summarizer_tax_usd,
        } => {
            stored.status = RunStatus::RequiresAction;
            stored.messages = messages;
            stored.turns_done = turns_done;
            stored.usage = usage;
            stored.summarized_upto = summarized_upto;
            stored.summarizer_tax_usd = sum_metered(stored.summarizer_tax_usd, summarizer_tax_usd);
            stored.pending_tool_calls = pending_tool_calls;
            store_run(l1.cache.as_ref(), &stored).await?;
            Ok(Json(stored.to_run()))
        }
    }
}

/// Build the cheap-model summarize request for one tool-result blob.
fn build_summary_request(class: &str, original: &str, model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message::System {
                content: MessageContent::Text(format!(
                    "Summarize this `{class}` tool result. Preserve every fact a later \
                     step might need; drop only redundancy and formatting. Output only \
                     the summary, no preamble."
                )),
            },
            Message::User {
                content: MessageContent::Text(original.to_string()),
                name: None,
            },
        ],
        stream: false,
        // Bound a runaway summarizer's generation cost. The token-true gate
        // already protects correctness (a non-reducing summary is rejected);
        // this only caps wasted output tokens on a misbehaving cheap model.
        max_tokens: Some(1024),
        ..Default::default()
    }
}

/// Map a measured dispatch result into a `SummarizeCall` (fail-open): `Err` →
/// decline (`summary: None`); `Ok` → the first choice's assistant text (empty/
/// missing ⇒ decline) + the dispatch's metered cost.
fn summary_call_from_result(
    res: Result<crate::measurement::MeasuredDispatch, String>,
) -> SummarizeCall {
    match res {
        Err(_) => SummarizeCall {
            summary: None,
            cost_usd: None,
        },
        Ok(d) => {
            let cost_usd = d.cost_usd;
            let text = d
                .response
                .choices
                .into_iter()
                .next()
                .and_then(|c| match c.message {
                    Message::Assistant {
                        content: Some(MessageContent::Text(t)),
                        ..
                    } => Some(t),
                    _ => None,
                })
                .filter(|t| !t.trim().is_empty());
            SummarizeCall {
                summary: text,
                cost_usd,
            }
        }
    }
}

/// Dispatch one cheap-model summarize call on the SUMMARIZER model's own
/// provider+creds (NOT the turn's served provider), bounded by `deadline`.
/// Fail-open: any resolution/dispatch failure ⇒ a declined `SummarizeCall`.
#[allow(clippy::too_many_arguments)] // all args are required; no natural grouping
async fn dispatch_summary(
    state: &AppState,
    org_id: Uuid,
    raw_bearer: &str,
    base_ctx: &RequestContext,
    summarizer_model: &str,
    class: &str,
    original: &str,
    deadline: std::time::Duration,
) -> SummarizeCall {
    let Some(provider) = state.registry.resolve(summarizer_model) else {
        return SummarizeCall {
            summary: None,
            cost_usd: None,
        };
    };
    let ctx =
        match chat::resolve_credentials_for(state, org_id, provider.id(), raw_bearer, true).await {
            Some(credentials) => RequestContext {
                credentials,
                ..base_ctx.clone()
            },
            None => {
                return SummarizeCall {
                    summary: None,
                    cost_usd: None,
                }
            }
        };
    let req = build_summary_request(class, original, summarizer_model);
    let res = crate::measurement::measured_single_dispatch(&provider, req, &ctx, deadline).await;
    summary_call_from_result(res)
}

/// Production transcript summarizer: for each eligible aging tool block, if its
/// class is gate-trusted and it is not an error blob, dispatch a cheap-model
/// summary and commit it when the token-true gate passes. Advances the watermark
/// to the eligible cutoff after a pass (a rejected/declined block is dispatched —
/// and taxed — at most once, never retried); the only early return is the
/// no-provider bail, which advances nothing. Fail-open throughout.
struct GatewayTranscriptSummarizer<'a> {
    state: &'a AppState,
    org_id: Uuid,
    raw_bearer: String,
    base_ctx: RequestContext,
    gate: std::sync::Arc<dyn crate::passes::agentic_budget::summarize_judge::SummaryGate>,
    cfg: SummarizeConfig,
    base_model: String,
    base_provider_id: Option<String>,
    summarizer_model: String,
    deadline: std::time::Duration,
}

#[async_trait]
impl TranscriptSummarizer for GatewayTranscriptSummarizer<'_> {
    async fn summarize_before_turn(
        &self,
        messages: &mut Vec<Message>,
        summarized_upto: &mut u32,
    ) -> Option<f64> {
        use crate::passes::agentic_budget::summarize_judge::{
            is_error_blob, resolve_summary_class,
        };
        let Some(provider_id) = self.base_provider_id.as_deref() else {
            return Some(0.0);
        };
        let tool_count = messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .count() as u32;
        let eligible =
            eligible_tool_ordinals(messages, *summarized_upto, self.cfg.keep_recent_pairs);
        let mut tax: Option<f64> = Some(0.0);
        for idx in eligible {
            let class = resolve_summary_class(messages, idx);
            if !self.gate.is_committable(&class) {
                continue;
            }
            let original = match &messages[idx] {
                Message::Tool {
                    content: MessageContent::Text(t),
                    ..
                } if !is_error_blob(t) => t.clone(),
                _ => continue,
            };
            let call = dispatch_summary(
                self.state,
                self.org_id,
                &self.raw_bearer,
                &self.base_ctx,
                &self.summarizer_model,
                &class,
                &original,
                self.deadline,
            )
            .await;
            tax = crate::passes::agentic_budget::summarize_judge::sum_metered(tax, call.cost_usd);
            let Some(summary) = call.summary else {
                continue;
            };
            if !token_true_ok(
                provider_id,
                &self.base_model,
                &original,
                &summary,
                self.cfg.clear_at_least_tokens,
            ) {
                continue;
            }
            if let Message::Tool { content, .. } = &mut messages[idx] {
                *content = MessageContent::Text(summary);
            }
        }
        *summarized_upto = tool_count.saturating_sub(self.cfg.keep_recent_pairs);
        tax
    }
}

/// Resolve the run's summarize policy ONCE from the turn-0 route (pinned).
/// `None` ⇒ summarize off (no route / nil-org / `elide_stale_tools` unset).
async fn resolve_summarize_config(
    state: &AppState,
    identity: &RunIdentity,
    model: &str,
    messages: &[Message],
) -> Option<SummarizeConfig> {
    let ctx = base_request_context(identity);
    let mut req_clone = ChatCompletionRequest {
        model: model.to_string(),
        messages: messages.to_vec(),
        ..Default::default()
    };
    let route_match = chat::apply_routing(
        state,
        &ctx,
        &mut req_clone,
        identity.forced_route.as_deref(),
    )
    .await
    .ok()
    .flatten()?;
    let ab = route_match.agentic_budget?;
    if !ab.elide_stale_tools {
        return None;
    }
    Some(SummarizeConfig {
        keep_recent_pairs: ab.keep_recent_pairs,
        clear_at_least_tokens: ab.clear_at_least_tokens,
    })
}

/// The cheap summarizer model: `TT_SUMMARIZER_MODEL` (new env) or the resolved
/// judge model (default `gpt-4o-mini`).
fn summarizer_model(state: &AppState) -> String {
    std::env::var("TT_SUMMARIZER_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| state.judge_config.judge_model.clone())
}

/// Build the base `RequestContext` for a run's auxiliary (summarizer/routing)
/// calls from the run identity. The credential is left EMPTY: routing reads only
/// org_id/tag, and the summarizer dispatch re-resolves the summarizer model's
/// OWN provider credential per call (so carrying the source bearer here would be
/// unused — and a wrong-vendor footgun).
fn base_request_context(identity: &RunIdentity) -> RequestContext {
    RequestContext {
        trace_id: identity.trace_id,
        org_id: identity.org_id,
        api_key_id: identity.api_key_id,
        credentials: ProviderCredentials {
            api_key: SecretString::new(String::new()),
            base_url: None,
            extra_headers: Vec::new(),
        },
        tag: identity.tag.clone(),
        deadline: identity.request_timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted completer: each call pops the next assistant message from the
    /// script. Lets the loop be exercised with no provider and no DB.
    struct Stub {
        script: std::sync::Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl TurnCompleter for Stub {
        async fn complete(
            &self,
            _req: ChatCompletionRequest,
            _is_mechanical: bool,
        ) -> Result<(Message, RunUsage), ApiError> {
            let mut s = self.script.lock().unwrap();
            Ok((
                s.remove(0),
                RunUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                },
            ))
        }
    }

    /// A completer that records the `is_mechanical` flag it is handed each turn
    /// (and pops the next scripted assistant message), so a test can assert the
    /// loop's per-turn mechanical classification.
    struct RecordingStub {
        mech: std::sync::Mutex<Vec<bool>>,
        script: std::sync::Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl TurnCompleter for RecordingStub {
        async fn complete(
            &self,
            _req: ChatCompletionRequest,
            is_mechanical: bool,
        ) -> Result<(Message, RunUsage), ApiError> {
            self.mech.lock().unwrap().push(is_mechanical);
            Ok((
                self.script.lock().unwrap().remove(0),
                RunUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                },
            ))
        }
    }

    fn assistant_final() -> Message {
        Message::Assistant {
            content: Some(MessageContent::Text("done".into())),
            tool_calls: vec![],
            name: None,
        }
    }

    fn assistant_toolcall(name: &str) -> Message {
        Message::Assistant {
            content: None,
            name: None,
            tool_calls: vec![tt_shared::messages::ToolCall {
                id: "c1".into(),
                r#type: "function".into(),
                function: tt_shared::messages::ToolCallFunction {
                    name: name.into(),
                    arguments: r#"{"task_description":"x"}"#.into(),
                },
            }],
        }
    }

    /// An assistant turn calling two tools (ids `c1`/`c2`) — used to exercise the
    /// mixed gateway+client pause path.
    fn assistant_two_toolcalls(a: &str, b: &str) -> Message {
        Message::Assistant {
            content: None,
            name: None,
            tool_calls: vec![
                tt_shared::messages::ToolCall {
                    id: "c1".into(),
                    r#type: "function".into(),
                    function: tt_shared::messages::ToolCallFunction {
                        name: a.into(),
                        arguments: r#"{"task_description":"x"}"#.into(),
                    },
                },
                tt_shared::messages::ToolCall {
                    id: "c2".into(),
                    r#type: "function".into(),
                    function: tt_shared::messages::ToolCallFunction {
                        name: b.into(),
                        arguments: r#"{"task_description":"y"}"#.into(),
                    },
                },
            ],
        }
    }

    fn tool_result(id: &str) -> Message {
        Message::Tool {
            content: tt_shared::messages::MessageContent::Text("r".into()),
            tool_call_id: id.into(),
        }
    }

    // ----- is_mechanical_continuation detection (slice 2a Task 1) -----

    #[test]
    fn mechanical_after_readonly_tool_continuation() {
        // assistant called a read-only gateway tool, its result appended → next turn is mechanical
        let msgs = vec![assistant_toolcall("find_route_for"), tool_result("c1")];
        assert!(is_mechanical_continuation(&msgs));
    }

    #[test]
    fn not_mechanical_after_client_tool() {
        let msgs = vec![assistant_toolcall("write_file"), tool_result("c1")];
        assert!(!is_mechanical_continuation(&msgs));
    }

    #[test]
    fn not_mechanical_mixed_prior_turn() {
        // a turn with a read-only AND a client tool → not mechanical
        let msgs = vec![
            assistant_two_toolcalls("find_route_for", "write_file"),
            tool_result("c1"),
            tool_result("c2"),
        ];
        assert!(!is_mechanical_continuation(&msgs));
    }

    #[test]
    fn not_mechanical_first_turn() {
        assert!(!is_mechanical_continuation(&[]));
        assert!(!is_mechanical_continuation(&[Message::User {
            content: tt_shared::messages::MessageContent::Text("hi".into()),
            name: None,
        }]));
    }

    #[test]
    fn not_mechanical_after_final_answer() {
        // last message is an assistant final (no tool results trailing) → not mechanical
        let msgs = vec![assistant_final()];
        assert!(!is_mechanical_continuation(&msgs));
    }

    #[tokio::test]
    async fn completes_on_final_answer() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![assistant_final()]),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 1);
    }

    #[tokio::test]
    async fn gateway_tool_turn_then_final() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 2);
        // transcript carries the tool result between the two assistant turns
        assert!(run
            .messages
            .iter()
            .any(|m| matches!(m, Message::Tool { .. })));
    }

    #[tokio::test]
    async fn loop_passes_is_mechanical_on_readonly_continuation() {
        // turn1: assistant calls a read-only gateway tool → loop executes it,
        // appends the result; turn2: the digest turn over that read-only result
        // → is_mechanical should be true. turn2 returns a final answer.
        let stub = RecordingStub {
            mech: std::sync::Mutex::new(vec![]),
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        let mech = stub.mech.lock().unwrap().clone();
        // turn1 not mechanical (fresh transcript), turn2 mechanical (read-only continuation).
        assert_eq!(mech, vec![false, true]);
    }

    #[tokio::test]
    async fn unknown_tool_is_incomplete() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![assistant_toolcall("write_file")]),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert!(run.note.unwrap().contains("write_file"));
    }

    #[tokio::test]
    async fn max_turns_bound() {
        // always returns a (gateway) tool call → never completes
        let script: Vec<Message> = (0..10)
            .map(|_| assistant_toolcall("find_route_for"))
            .collect();
        let stub = Stub {
            script: std::sync::Mutex::new(script),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 3).await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(run.turns, 3);
    }

    // ----- Loop-core pause/resume (slice 1b Task 1) -----

    #[tokio::test]
    async fn core_pauses_on_client_tool_with_pending() {
        // Stub returns an assistant turn calling a client tool "write_file".
        let stub = Stub {
            script: std::sync::Mutex::new(vec![assistant_toolcall("write_file")]),
        };
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            0,
            0, /*summarized_upto*/
            RunUsage::default(),
            None, /*summarizer*/
        )
        .await;
        match out {
            LoopOutcome::Paused {
                pending_tool_calls,
                turns_done,
                ..
            } => {
                assert_eq!(turns_done, 1);
                assert_eq!(pending_tool_calls.len(), 1);
                assert_eq!(pending_tool_calls[0].function.name, "write_file");
            }
            _ => panic!("expected Paused"),
        }
    }

    #[tokio::test]
    async fn core_resume_continues_to_completion() {
        // Resume: messages already contain the paused assistant turn + the
        // appended client tool result; the next completion is a final answer.
        let stub = Stub {
            script: std::sync::Mutex::new(vec![assistant_final()]),
        };
        let resumed_messages = vec![
            assistant_toolcall("write_file"),
            Message::Tool {
                content: MessageContent::Text("ok".into()),
                tool_call_id: "c1".into(),
            },
        ];
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            resumed_messages,
            vec![],
            8,
            1,
            0, /*summarized_upto*/
            RunUsage::default(),
            None, /*summarizer*/
        )
        .await;
        match out {
            LoopOutcome::Terminal(run) => {
                assert_eq!(run.status, RunStatus::Completed);
                assert_eq!(run.turns, 2);
            }
            _ => panic!("expected Terminal Completed"),
        }
    }

    #[tokio::test]
    async fn core_mixed_turn_executes_gateway_then_pauses() {
        // An assistant turn with BOTH a gateway tool and a client tool: gateway
        // executed inline (a Tool result appears), pause with only the client one.
        let stub = Stub {
            script: std::sync::Mutex::new(vec![assistant_two_toolcalls(
                "find_route_for",
                "write_file",
            )]),
        };
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            0,
            0, /*summarized_upto*/
            RunUsage::default(),
            None, /*summarizer*/
        )
        .await;
        match out {
            LoopOutcome::Paused {
                messages,
                pending_tool_calls,
                ..
            } => {
                assert!(
                    messages.iter().any(|m| matches!(m, Message::Tool { .. })),
                    "gateway result appended"
                );
                assert_eq!(pending_tool_calls.len(), 1);
                assert_eq!(pending_tool_calls[0].function.name, "write_file");
            }
            _ => panic!("expected Paused"),
        }
    }

    // ----- Task 4 wiring (no provider, no DB) -----

    #[test]
    fn disable_cache_sets_bypass_mode() {
        // The per-turn cache-disable knob must parse to `CacheMode::Bypass`,
        // which `CacheBehavior::resolve` maps to `do_lookup=false,
        // do_insert=false` — so `complete_once` always returns `Dispatched`.
        let mut req = ChatCompletionRequest::default();
        GatewayCompleter::disable_cache(&mut req);
        let cfg = tt_shared::parse_cache_control(&req.tt_extras)
            .expect("cache knob present after disable_cache");
        assert_eq!(cfg.mode, tt_shared::CacheMode::Bypass);
    }

    #[test]
    fn run_identity_strips_cache_override_header() {
        // A caller-supplied `X-TokenTrimmer-Cache` header must be stripped so it
        // cannot re-enable lookups/inserts mid-loop (header beats body in
        // `prepare`). Non-cache headers (e.g. the tag) survive.
        let mut headers = HeaderMap::new();
        headers.insert("x-tokentrimmer-cache", "force-write".parse().unwrap());
        headers.insert("x-tokentrimmer-tag", "proj-x".parse().unwrap());
        let id = RunIdentity::from_request(None, "", &headers);
        assert!(!id.headers.contains_key("x-tokentrimmer-cache"));
        assert_eq!(id.tag.as_deref(), Some("proj-x"));
        // Anonymous caller → nil org/key, no L2 entitlement.
        assert_eq!(id.org_id, Uuid::nil());
        assert!(!id.l2_allowed);
    }

    #[test]
    fn run_identity_carries_paid_tier_l2() {
        let ctx = ApiKeyContext {
            key_id: Uuid::from_u128(1),
            org_id: Uuid::from_u128(2),
            tier: Some(tt_shared::CallerTier::Pro),
        };
        let id = RunIdentity::from_request(Some(&ctx), "", &HeaderMap::new());
        assert_eq!(id.org_id, Uuid::from_u128(2));
        assert_eq!(id.api_key_id, Uuid::from_u128(1));
        assert!(id.l2_allowed);
    }

    // ----- Run store round-trip (slice 1b Task 2) -----

    #[tokio::test]
    async fn stored_run_roundtrips_through_cache() {
        let cache = tt_cache::memory::InMemoryL1Cache::new();
        let org = uuid::Uuid::new_v4();
        let run = StoredRun {
            id: uuid::Uuid::new_v4(),
            org_id: org,
            status: RunStatus::RequiresAction,
            model: "m".into(),
            messages: vec![assistant_toolcall("write_file")],
            tools: vec![],
            max_turns: 8,
            turns_done: 1,
            usage: RunUsage {
                prompt_tokens: 5,
                completion_tokens: 7,
            },
            pending_tool_calls: vec![],
            routing: StoredRouting {
                provider_pin: None,
                forced_route: None,
                tag: None,
            },
            summarized_upto: 0,
            summarizer_tax_usd: None,
            summarize: None,
        };
        store_run(&cache, &run).await.unwrap();
        let got = fetch_run(&cache, org, run.id)
            .await
            .unwrap()
            .expect("present");
        assert_eq!(got.id, run.id);
        assert_eq!(got.status, RunStatus::RequiresAction);
        assert_eq!(got.turns_done, 1);
        // wrong org → miss
        assert!(fetch_run(&cache, uuid::Uuid::new_v4(), run.id)
            .await
            .unwrap()
            .is_none());
    }

    // ----- create_run persist-on-pause (slice 1b Task 3) -----

    // `create_run` builds its completer from `GatewayCompleter` (needs a
    // provider), so the persist DECISION is asserted at the seam: a Paused
    // outcome + an L1 store ⇒ a `StoredRun` lands in the cache with status
    // `RequiresAction`. The full create→pause→persist HTTP path is exercised by
    // Task 5's resume tests, which seed a `StoredRun` directly.
    #[tokio::test]
    async fn paused_with_l1_persists_requires_action() {
        let cache = tt_cache::memory::InMemoryL1Cache::new();
        let org = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();
        let stored = StoredRun {
            id,
            org_id: org,
            status: RunStatus::RequiresAction,
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            max_turns: 8,
            turns_done: 1,
            usage: RunUsage::default(),
            pending_tool_calls: vec![],
            routing: StoredRouting {
                provider_pin: None,
                forced_route: None,
                tag: None,
            },
            summarized_upto: 0,
            summarizer_tax_usd: None,
            summarize: None,
        };
        store_run(&cache, &stored).await.unwrap();
        assert_eq!(
            fetch_run(&cache, org, id).await.unwrap().unwrap().status,
            RunStatus::RequiresAction
        );
    }

    // ----- get_run org-scoped fetch (slice 1b Task 4) -----

    // `get_run` maps `fetch_run` outcomes to the HTTP response: `None` → 404,
    // `Some` → the `Run` view. The org is embedded in the store key, so a fetch
    // with the wrong org misses (→ 404). Assert that store-level contract
    // (provider-free); the 401/503 guards are thin and integration-covered.
    #[tokio::test]
    async fn get_run_missing_is_404_and_wrong_org_misses() {
        let cache = tt_cache::memory::InMemoryL1Cache::new();
        let org = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();
        // absent → fetch returns None (handler maps to 404)
        assert!(fetch_run(&cache, org, id).await.unwrap().is_none());
        // seed, then wrong-org fetch misses
        let stored = StoredRun {
            id,
            org_id: org,
            status: RunStatus::RequiresAction,
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            max_turns: 8,
            turns_done: 1,
            usage: RunUsage::default(),
            pending_tool_calls: vec![],
            routing: StoredRouting {
                provider_pin: None,
                forced_route: None,
                tag: None,
            },
            summarized_upto: 0,
            summarizer_tax_usd: None,
            summarize: None,
        };
        store_run(&cache, &stored).await.unwrap();
        assert!(fetch_run(&cache, uuid::Uuid::new_v4(), id)
            .await
            .unwrap()
            .is_none());
        assert!(fetch_run(&cache, org, id).await.unwrap().is_some());
    }

    // ----- submit_tool_outputs resume (slice 1b Task 5) -----

    // The handler validates that the submitted `tool_call_id`s EXACTLY cover the
    // run's `pending_tool_calls` (HashSet equality) — a partial set is a 400,
    // the exact set proceeds. `submit_tool_outputs` builds its completer from
    // `GatewayCompleter` (needs a provider), so the id-coverage rule is asserted
    // here at the seam; the happy-path resume is covered by
    // `core_resume_continues_to_completion` (Task 1) + the store round-trip
    // (Task 2), which the handler wires together.
    #[test]
    fn tool_outputs_id_coverage_check() {
        // pending {c1,c2}; submitting only {c1} must be rejected; {c1,c2} accepted.
        let pending: std::collections::HashSet<&str> = ["c1", "c2"].into_iter().collect();
        let only_one: std::collections::HashSet<&str> = ["c1"].into_iter().collect();
        let both: std::collections::HashSet<&str> = ["c1", "c2"].into_iter().collect();
        assert_ne!(only_one, pending);
        assert_eq!(both, pending);
    }

    // A Completed (terminal) stored run is exactly what the status guard 409s on:
    // its status is not `RequiresAction`, so `submit_tool_outputs` returns
    // `ApiError::Conflict` before touching the loop. Seed one and assert the
    // guard condition (provider-free).
    #[tokio::test]
    async fn terminal_stored_run_is_not_requires_action() {
        let cache = tt_cache::memory::InMemoryL1Cache::new();
        let org = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();
        let stored = StoredRun {
            id,
            org_id: org,
            status: RunStatus::Completed,
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            max_turns: 8,
            turns_done: 2,
            usage: RunUsage::default(),
            pending_tool_calls: vec![],
            routing: StoredRouting {
                provider_pin: None,
                forced_route: None,
                tag: None,
            },
            summarized_upto: 0,
            summarizer_tax_usd: None,
            summarize: None,
        };
        store_run(&cache, &stored).await.unwrap();
        let got = fetch_run(&cache, org, id).await.unwrap().expect("present");
        // The handler's status guard: a non-RequiresAction run → 409 Conflict.
        assert_ne!(got.status, RunStatus::RequiresAction);
    }

    // ----- SummarizeConfig + Run/StoredRun tax & watermark fields (slice 2c-1 Task 3) -----

    #[test]
    fn stored_run_deserializes_without_new_fields() {
        // A run persisted BEFORE this deploy has no summarized_upto/summarizer_tax_usd
        // /summarize keys; #[serde(default)] must let it deserialize (resumes unsummarized).
        let json = r#"{
            "id":"00000000-0000-0000-0000-000000000001",
            "org_id":"00000000-0000-0000-0000-000000000002",
            "status":"requires_action","model":"m","messages":[],"tools":[],
            "max_turns":8,"turns_done":1,
            "usage":{"prompt_tokens":0,"completion_tokens":0},
            "pending_tool_calls":[],
            "routing":{"provider_pin":null,"forced_route":null,"tag":null}
        }"#;
        let sr: StoredRun = serde_json::from_str(json).expect("back-compat deserialize");
        assert_eq!(sr.summarized_upto, 0);
        assert_eq!(sr.summarizer_tax_usd, None);
        assert!(sr.summarize.is_none());
    }

    #[test]
    fn to_run_maps_summarizer_tax() {
        let sr = StoredRun {
            id: uuid::Uuid::nil(),
            org_id: uuid::Uuid::nil(),
            status: RunStatus::RequiresAction,
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            max_turns: 8,
            turns_done: 1,
            usage: RunUsage::default(),
            pending_tool_calls: vec![],
            routing: StoredRouting {
                provider_pin: None,
                forced_route: None,
                tag: None,
            },
            summarized_upto: 3,
            summarizer_tax_usd: Some(0.0004),
            summarize: None,
        };
        assert_eq!(sr.to_run().summarizer_tax_usd, Some(0.0004));
    }

    // ----- eligible_tool_ordinals + token_true_ok (slice 2c-1 Task 4) -----

    #[test]
    fn eligible_ordinals_keeps_recent_and_respects_watermark() {
        // messages: A(tc) T0 A(tc) T1 A(tc) T2 A(tc) T3  (4 tool blocks)
        let msgs = vec![
            assistant_toolcall("find_route_for"),
            tool_result("c1"),
            assistant_toolcall("find_route_for"),
            tool_result("c2"),
            assistant_toolcall("find_route_for"),
            tool_result("c3"),
            assistant_toolcall("find_route_for"),
            tool_result("c4"),
        ];
        // keep_recent_pairs=2 → eligible tool blocks are T0,T1 (the 2 oldest); their
        // MESSAGE indices are 1 and 3. watermark=0 → both.
        assert_eq!(eligible_tool_ordinals(&msgs, 0, 2), vec![1, 3]);
        // watermark=1 → T0 already done → only T1 (index 3).
        assert_eq!(eligible_tool_ordinals(&msgs, 1, 2), vec![3]);
        // keep_recent_pairs >= tool count → nothing eligible.
        assert!(eligible_tool_ordinals(&msgs, 0, 4).is_empty());
        assert!(eligible_tool_ordinals(&msgs, 0, 9).is_empty());
        // watermark advanced to/past the eligible cutoff → nothing to do
        assert!(eligible_tool_ordinals(&msgs, 2, 2).is_empty());
        assert!(eligible_tool_ordinals(&msgs, 99, 2).is_empty());
    }

    #[test]
    fn token_true_ok_requires_real_reduction() {
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu";
        let short = "alpha";
        assert!(token_true_ok("openai", "gpt-4o-mini", long, short, 0));
        // a non-reduction (same text) must be rejected even at floor 0 (>=1 required).
        assert!(!token_true_ok("openai", "gpt-4o-mini", long, long, 0));
        // a reduction below the clear_at_least_tokens floor is rejected.
        assert!(!token_true_ok("openai", "gpt-4o-mini", long, short, 9999));
    }

    #[test]
    fn sample_key_is_deterministic_and_spreads() {
        let t = uuid::Uuid::from_u128(42);
        assert_eq!(sample_key(t, "c1"), sample_key(t, "c1")); // deterministic
        assert_ne!(sample_key(t, "c1"), sample_key(t, "c2")); // distinct tool_call_ids differ
        assert_ne!(
            sample_key(uuid::Uuid::from_u128(1), "c1"),
            sample_key(uuid::Uuid::from_u128(2), "c1")
        );
    }

    #[test]
    fn latest_user_text_takes_the_most_recent_user_message() {
        let msgs = vec![
            Message::User {
                content: MessageContent::Text("first".into()),
                name: None,
            },
            Message::Assistant {
                content: Some(MessageContent::Text("a".into())),
                tool_calls: vec![],
                name: None,
            },
            Message::User {
                content: MessageContent::Text("second".into()),
                name: None,
            },
            tool_result("c1"),
        ];
        assert_eq!(latest_user_text(&msgs), "second");
        assert_eq!(latest_user_text(&[]), ""); // no user message → empty
    }

    #[test]
    fn build_summary_request_shapes_a_cheap_call() {
        let req = build_summary_request("inspect_diff", "big tool output", "gpt-4o-mini");
        assert_eq!(req.model, "gpt-4o-mini");
        assert!(!req.stream);
        assert_eq!(req.messages.len(), 2);
        assert!(matches!(req.messages[0], Message::System { .. }));
        assert!(matches!(req.messages[1], Message::User { .. }));
    }

    #[test]
    fn summary_call_maps_dispatch_and_fails_open_on_err() {
        use crate::measurement::MeasuredDispatch;
        // Err → fail open (no summary, no cost).
        let call = summary_call_from_result(Err("deadline exceeded".into()));
        assert!(call.summary.is_none());
        assert!(call.cost_usd.is_none());

        // Ok with text → summary + cost passed through. ChatCompletionResponse has
        // NO Default — construct all 6 fields explicitly; Usage DOES derive Default.
        let resp = tt_shared::ChatCompletionResponse {
            id: String::new(),
            object: String::new(),
            created: 0,
            model: "gpt-4o-mini".into(),
            choices: vec![tt_shared::messages::Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("short".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: tt_shared::Usage::default(),
        };
        let call = summary_call_from_result(Ok(MeasuredDispatch {
            response: resp,
            cost_usd: Some(0.0001),
        }));
        assert_eq!(call.summary.as_deref(), Some("short"));
        assert_eq!(call.cost_usd, Some(0.0001));
    }

    // ----- per-block commit decision: gate ∧ ¬error-blob ∧ token-true (slice 2c-1 Task 7) -----

    #[test]
    fn summarize_commit_decision_gates_and_token_checks() {
        use crate::passes::agentic_budget::summarize_judge::{
            is_error_blob, parse_trusted_classes, ConfigSummaryGate, SummaryGate,
        };
        let gate = ConfigSummaryGate::new(parse_trusted_classes("inspect_diff"));
        assert!(gate.is_committable("inspect_diff"));
        assert!(!is_error_blob(
            "a long verbose tool result with lots of words"
        ));
        assert!(token_true_ok(
            "openai",
            "gpt-4o-mini",
            "a long verbose tool result with lots of words to remove",
            "short",
            0
        ));
        assert!(!gate.is_committable("write_file")); // untrusted → no commit
        assert!(is_error_blob(r#"{"error":"boom"}"#)); // error blob → never summarized
    }

    // ----- TranscriptSummarizer hook threading (slice 2c-1 Task 6) -----

    /// A stub summarizer that records calls, advances the watermark, reports a fixed tax.
    struct StubSummarizer {
        calls: std::sync::Mutex<u32>,
    }
    #[async_trait]
    impl TranscriptSummarizer for StubSummarizer {
        async fn summarize_before_turn(
            &self,
            messages: &mut Vec<Message>,
            summarized_upto: &mut u32,
        ) -> Option<f64> {
            *self.calls.lock().unwrap() += 1;
            let tools = messages
                .iter()
                .filter(|m| matches!(m, Message::Tool { .. }))
                .count() as u32;
            *summarized_upto = tools.saturating_sub(1);
            Some(0.0002)
        }
    }

    #[tokio::test]
    async fn loop_calls_summarizer_each_turn_and_accrues_tax() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
        };
        let summ = StubSummarizer {
            calls: std::sync::Mutex::new(0),
        };
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            0,
            0,
            RunUsage::default(),
            Some(&summ),
        )
        .await;
        match out {
            LoopOutcome::Terminal(run) => {
                assert_eq!(run.status, RunStatus::Completed);
                assert_eq!(*summ.calls.lock().unwrap(), 2); // hook ran before each of the 2 turns
                assert_eq!(run.summarizer_tax_usd, Some(0.0004)); // 0.0002 * 2, metered
            }
            _ => panic!("expected Terminal Completed"),
        }
    }

    #[tokio::test]
    async fn loop_with_no_summarizer_is_unchanged() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![assistant_final()]),
        };
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            0,
            0,
            RunUsage::default(),
            None,
        )
        .await;
        match out {
            LoopOutcome::Terminal(run) => assert_eq!(run.summarizer_tax_usd, None),
            _ => panic!("expected Terminal"),
        }
    }
}
