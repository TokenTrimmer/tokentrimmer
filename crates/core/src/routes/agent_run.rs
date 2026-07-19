//! Server-side agentic loop (slice 1a): run model->tool->model over the
//! read-only gateway tools until a final answer or `max_turns`. Synchronous;
//! no Redis/no client round-trip (slice 1b). Generic over `TurnCompleter` so
//! tests inject a stub.

use async_trait::async_trait;
use axum::{
    extract::{Extension, Path, State},
    http::HeaderMap,
    response::{
        sse::{Event, KeepAlive},
        IntoResponse, Response, Sse,
    },
    Json,
};
use futures::StreamExt; // for .chain
use tokio::sync::mpsc;
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
    quality_sample::{self, GatewayLlmJudge},
    routes::{
        agent_run_budget::{
            estimate_next_turn_cost, step_signature, would_exceed, NoProgressTracker, StopReason,
            RUNAWAY_REPEAT_THRESHOLD,
        },
        chat::{self, CompletionOutcome},
    },
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
    /// Accumulated SERVED cost (USD) across the run's turns — the sum of each
    /// turn's `x-tokentrimmer-cost-usd` (`CompletionHeaders.cost_breakdown.cost_usd`).
    /// Unsigned, like the per-request cost header. Distinct from `Run`/`StoredRun`'s
    /// `summarizer_tax_usd` (the 2c-2 measurement tax).
    #[serde(default)]
    pub cost_usd: f64,
    /// Accumulated BASELINE cost (USD) across the run's turns — what each turn
    /// would have cost unoptimized (originally-requested model, full input price,
    /// no cache discount). Sourced from `CostBreakdown.baseline_cost_usd` per turn.
    /// `#[serde(default)]` for back-compat with stored runs written before W2a Task 2.
    #[serde(default)]
    pub baseline_cost_usd: f64,
    /// Served cost (USD) of each completed turn, in order. Lets a caller see
    /// where the spend went without per-turn headers (the run is body-returned).
    #[serde(default)]
    pub per_turn_cost_usd: Vec<f64>,
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
    /// Why a non-`Completed`/`Failed` run terminated (machine-readable). `None`
    /// for completed/failed runs and for legacy stored runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

/// One server-sent event from a streaming agent run (slice 3b). TT-native,
/// turn-level (per-turn completion is non-streaming, so no token deltas).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum RunEvent {
    #[serde(rename = "run.turn")]
    Turn { turn: u32 },
    #[serde(rename = "run.turn_cost")]
    TurnCost {
        turn: u32,
        turn_cost_usd: f64,
        run_cost_usd: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_remaining_usd: Option<f64>,
    },
    #[serde(rename = "run.message")]
    Message {
        message: tt_shared::messages::Message,
    },
    #[serde(rename = "run.tool_result")]
    ToolResult {
        tool_call_id: String,
        content: String,
    },
    #[serde(rename = "run.requires_action")]
    RequiresAction {
        run: Run,
        pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
    },
    #[serde(rename = "run.completed")]
    Completed { run: Run },
    #[serde(rename = "run.failed")]
    Failed { run: Run },
    #[serde(rename = "run.incomplete")]
    Incomplete { run: Run },
}

impl RunEvent {
    fn event_name(&self) -> &'static str {
        match self {
            RunEvent::Turn { .. } => "run.turn",
            RunEvent::TurnCost { .. } => "run.turn_cost",
            RunEvent::Message { .. } => "run.message",
            RunEvent::ToolResult { .. } => "run.tool_result",
            RunEvent::RequiresAction { .. } => "run.requires_action",
            RunEvent::Completed { .. } => "run.completed",
            RunEvent::Failed { .. } => "run.failed",
            RunEvent::Incomplete { .. } => "run.incomplete",
        }
    }
    /// Render as an axum SSE event (named, JSON data).
    fn to_sse(&self) -> axum::response::sse::Event {
        axum::response::sse::Event::default()
            .event(self.event_name())
            .data(serde_json::to_string(self).unwrap_or_default())
    }
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
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    max_cost_usd: Option<f64>,
    turns_done: u32,
    summarized_upto: u32,
    usage: RunUsage,
    summarizer: Option<&dyn TranscriptSummarizer>,
    events: Option<&tokio::sync::mpsc::UnboundedSender<RunEvent>>,
) -> LoopOutcome {
    run_loop_core_with_output_cap(
        completer,
        id,
        model,
        messages,
        tools,
        max_turns,
        None,
        max_cost_usd,
        turns_done,
        summarized_upto,
        usage,
        summarizer,
        events,
    )
    .await
}

/// Internal variant used by bounded workflow nodes. Public agent runs continue
/// through [`run_loop_core`] with `None`, preserving their provider-default
/// completion behavior and persisted resume wire format.
#[allow(clippy::too_many_arguments)]
async fn run_loop_core_with_output_cap(
    completer: &dyn TurnCompleter,
    id: uuid::Uuid,
    model: String,
    mut messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    max_output_tokens: Option<u32>,
    max_cost_usd: Option<f64>,
    turns_done: u32,
    mut summarized_upto: u32,
    mut usage: RunUsage,
    summarizer: Option<&dyn TranscriptSummarizer>,
    events: Option<&tokio::sync::mpsc::UnboundedSender<RunEvent>>,
) -> LoopOutcome {
    use crate::passes::agentic_budget::summarize_judge::sum_metered;
    let emit = |ev: RunEvent| {
        if let Some(tx) = events {
            let _ = tx.send(ev); // unbounded, sync; receiver-dropped ⇒ ignored
        }
    };
    let max_turns = max_turns.clamp(1, MAX_MAX_TURNS);
    let mut turn = turns_done;
    // No summarizer ⇒ no tax (`None`), keeping the `summarizer: None` path
    // byte-behavior-identical to pre-2c-1. With a summarizer, start the metered
    // accumulator at `Some(0.0)` so each turn's tax folds in via `sum_metered`.
    let mut summarizer_tax: Option<f64> = summarizer.map(|_| 0.0);
    // Runaway / no-progress guard: terminate a gateway-only loop that re-issues
    // the same tool call and gets the same result turn after turn (the fastest
    // way an autonomous loop burns money — it trips before the static cost cap).
    // Per-`run_loop`-invocation: a client-tool pause returns `Paused` and the
    // detector resets on resume, which is correct (a paused loop cannot burn
    // server-side between turns).
    let mut no_progress = NoProgressTracker::new(RUNAWAY_REPEAT_THRESHOLD);
    while turn < max_turns {
        let est_next = if max_cost_usd.is_some() {
            estimate_next_turn_cost(&model, &messages, max_output_tokens)
        } else {
            None
        };
        if would_exceed(usage.cost_usd, est_next, max_cost_usd) {
            let accrued = usage.cost_usd;
            let note = match est_next {
                Some(e) => format!(
                    "run cost cap ${:.4} would be exceeded (accrued ${:.4} + est ${:.4})",
                    max_cost_usd.unwrap_or_default(),
                    accrued,
                    e
                ),
                None => format!(
                    "run cost cap ${:.4} reached (accrued ${:.4})",
                    max_cost_usd.unwrap_or_default(),
                    accrued
                ),
            };
            return LoopOutcome::Terminal(Run {
                id,
                status: RunStatus::Incomplete,
                messages,
                turns: turn,
                usage,
                note: Some(note),
                summarizer_tax_usd: summarizer_tax,
                stop_reason: Some(StopReason::BudgetExhausted),
            });
        }
        emit(RunEvent::Turn { turn: turn + 1 }); // 1-indexed
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
            max_tokens: max_output_tokens,
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
                    stop_reason: None,
                });
            }
        };
        usage.prompt_tokens += turn_usage.prompt_tokens;
        usage.completion_tokens += turn_usage.completion_tokens;
        usage.cost_usd += turn_usage.cost_usd; // served cost across turns (and resume, via the carried usage)
        usage.baseline_cost_usd += turn_usage.baseline_cost_usd; // baseline (unoptimized) cost across turns (W2a)
        usage.per_turn_cost_usd.push(turn_usage.cost_usd);
        emit(RunEvent::TurnCost {
            turn: turn + 1,
            turn_cost_usd: turn_usage.cost_usd,
            run_cost_usd: usage.cost_usd,
            budget_remaining_usd: max_cost_usd.map(|c| (c - usage.cost_usd).max(0.0)),
        });
        messages.push(assistant.clone());
        emit(RunEvent::Message {
            message: assistant.clone(),
        });

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
                stop_reason: None,
            });
        }

        let has_client_tool = tool_calls
            .iter()
            .any(|tc| !crate::routes::gateway_tools::is_gateway_tool(&tc.function.name));

        // Execute the gateway tool_calls of this turn inline (whether or not we
        // are about to pause — so a mixed turn's gateway work isn't wasted and,
        // on resume, every tool_call of this assistant turn is answered). The
        // results are collected (in call order) to fingerprint this step for the
        // no-progress guard below.
        let mut gateway_results: Vec<String> = Vec::new();
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
                gateway_results.push(result.clone());
                emit(RunEvent::ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: result.clone(),
                });
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

        // Gateway-only continuation: fingerprint this step (tool calls + their
        // results) and terminate if the loop has repeated the same step
        // `RUNAWAY_REPEAT_THRESHOLD` times — it is making no progress and will
        // only keep spending. Ends `Incomplete` (partial transcript preserved),
        // disambiguated by `StopReason::Runaway`.
        if no_progress.record(step_signature(&tool_calls, &gateway_results)) {
            return LoopOutcome::Terminal(Run {
                id,
                status: RunStatus::Incomplete,
                messages,
                turns: turn + 1,
                usage,
                note: Some(format!(
                    "no progress: {RUNAWAY_REPEAT_THRESHOLD} consecutive identical tool-call/result steps (runaway loop) — terminated"
                )),
                summarizer_tax_usd: summarizer_tax,
                stop_reason: Some(StopReason::Runaway),
            });
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
        stop_reason: Some(StopReason::MaxTurns),
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
        None, /*max_cost_usd*/
        0,    /*turns_done*/
        0,    /*summarized_upto*/
        RunUsage::default(),
        None, /*summarizer*/
        None, /*events*/
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
                stop_reason: None,
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
    /// Cost-admission guard (USD) carried across pause so resume applies the
    /// same cap. It is not a provider settlement guarantee.
    /// `#[serde(default)]` preserves cross-deploy back-compat.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Terminal stop reason; `None` for paused (requires_action) runs.
    /// `#[serde(default)]` for cross-deploy back-compat.
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
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
            stop_reason: self.stop_reason,
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
// Durable-record view types + `GET /v1/agent/runs` list (W0b Task 6)
// ---------------------------------------------------------------------------

/// Lightweight durable summary of a run — only the fields persisted in
/// Postgres. The transcript (`messages`) is NOT included; it only lives in
/// Redis while the run is in-flight. Callers that need the transcript should
/// `GET /v1/agent/runs/:id` while the run is still within its Redis TTL.
#[derive(Debug, serde::Serialize)]
pub struct DurableRunView {
    pub id: uuid::Uuid,
    /// Snake-case status string from the DB: `running`, `completed`,
    /// `incomplete`, `failed`, or `requires_action`.
    pub status: String,
    pub model: String,
    pub turns: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<i32>,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// Response body for `GET /v1/agent/runs` — an OpenAI-style `list` envelope.
#[derive(Debug, serde::Serialize)]
pub struct ListRunsResponse {
    pub object: &'static str,
    pub data: Vec<DurableRunView>,
}

/// Map a durable [`AgentRunRecord`] (read from Postgres) to a full [`Run`]
/// view for `GET /v1/agent/runs/:id` Postgres fallback.
///
/// The transcript (`messages`) is NOT durable; only available while the run
/// is in Redis. This returns `messages: vec![]` and a `note` explaining the
/// absence. All identity + terminal-state fields are carried over.
fn durable_record_to_run(rec: &crate::routes::agent_run_store::AgentRunRecord) -> Run {
    let status = match rec.status.as_str() {
        "completed" => RunStatus::Completed,
        "incomplete" => RunStatus::Incomplete,
        "failed" => RunStatus::Failed,
        "requires_action" => RunStatus::RequiresAction,
        // "running" or unknown: the run never reached a terminal state (e.g.
        // it crashed without a `finish_agent_run` call). Map to Incomplete so
        // the caller sees a coherent non-running status.
        _ => RunStatus::Incomplete,
    };
    let stop_reason = rec.stop_reason.as_deref().and_then(|s| match s {
        "max_turns" => Some(crate::routes::agent_run_budget::StopReason::MaxTurns),
        "budget_exhausted" => Some(crate::routes::agent_run_budget::StopReason::BudgetExhausted),
        "runaway" => Some(crate::routes::agent_run_budget::StopReason::Runaway),
        _ => None,
    });
    Run {
        id: rec.id,
        status,
        messages: vec![],
        turns: rec.turns as u32,
        usage: RunUsage {
            cost_usd: rec.cost_usd,
            ..Default::default()
        },
        note: Some(
            "durable view: transcript not available (Redis TTL expired or run in-flight)".into(),
        ),
        summarizer_tax_usd: None,
        stop_reason,
    }
}

/// `GET /v1/agent/runs` — list up to 50 of the caller's agent runs, newest
/// first. Returns durable identity + terminal state only (no transcript —
/// that only lives in Redis). Requires Postgres (503 if absent) and a real
/// authenticated org (401 for anonymous / dogfood callers).
pub async fn list_runs(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Json<ListRunsResponse>> {
    let org = match ctx {
        Some(Extension(c)) if c.org_id != crate::DOGFOOD_ORG_ID => c.org_id,
        _ => return Err(ApiError::Unauthorized),
    };
    let pool = state.db_pool.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "agent run list requires a Postgres pool (none configured)".into(),
        )
    })?;
    const LIST_LIMIT: i64 = 50;
    let records = crate::routes::agent_run_store::list_agent_runs(pool, org, LIST_LIMIT).await;
    let data = records
        .into_iter()
        .map(|r| DurableRunView {
            id: r.id,
            status: r.status,
            model: r.model,
            turns: r.turns,
            max_turns: r.max_turns,
            cost_usd: r.cost_usd,
            stop_reason: r.stop_reason,
            tag: r.tag,
        })
        .collect();
    Ok(Json(ListRunsResponse {
        object: "list",
        data,
    }))
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
    /// CO-4: at-breach PauseShadow shadow-skip flag (from `ApiKeyContext`);
    /// inherited by every agent-loop turn so a breach doesn't double-spend
    /// mid-loop either.
    skip_shadow: bool,
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
    /// The run's own id — set by the handler after `Uuid::new_v4()` (create) or
    /// from `stored.id` (resume). Initialized to `Uuid::nil()` in `from_request`
    /// so construction sites don't need a run id up front.
    run_id: Uuid,
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

        let (org_id, api_key_id, caller_tier, skip_shadow) = match auth_ctx {
            Some(c) => (c.org_id, c.key_id, c.tier, c.skip_shadow),
            None => (Uuid::nil(), Uuid::nil(), None, false),
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
            skip_shadow,
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
            // Set to nil here; the handler overwrites it with the real run id
            // after `Uuid::new_v4()` (create) or `stored.id` (resume).
            run_id: Uuid::nil(),
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
            // Stamp the run id so complete_once writes it on the request_logs row.
            run_id: Some(self.identity.run_id),
            node_id: None,
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
            // CO-4: agent-loop turns inherit the original request's
            // skip_shadow flag (an at-breach PauseShadow org's loop turns
            // also skip the pane/shadow dispatch — no doubled spend mid-loop).
            self.identity.skip_shadow,
        )
        .await?;

        // P0-1/P0-3 budget accounting (B2): each agent-loop turn is its OWN
        // billed request. `complete_once` writes one `request_logs` row per
        // provider call (`cached: false`, status 200) AND settles `cached=false`
        // into the spend sink inline — so per-turn settle is CONSISTENT with
        // per-row billing (one row + one billed/served advance per turn). Cache
        // is disabled per turn (`disable_cache` → `do_lookup=false`), so every
        // turn dispatches and the `CacheHit` arm below is an invariant
        // violation, never a free turn.
        //
        // The auth pre-flight `check()` runs ONCE per run (at the `/v1/agent/runs`
        // boundary), not per turn, so the free monthly cap is best-effort across
        // a multi-turn run and may overshoot WITHIN one run; it is re-enforced on
        // the next request once the per-turn settles have advanced the counter.
        match chat::complete_once(self.state, &ctx, prep).await? {
            CompletionOutcome::Dispatched { response, headers } => {
                let usage = RunUsage {
                    prompt_tokens: response.usage.prompt_tokens,
                    completion_tokens: response.usage.completion_tokens,
                    cost_usd: headers.cost_breakdown.cost_usd, // served cost (x-tokentrimmer-cost-usd)
                    baseline_cost_usd: headers.cost_breakdown.baseline_cost_usd, // unoptimized baseline (W2a)
                    ..Default::default()
                };
                let msg = response
                    .choices
                    .into_iter()
                    .next()
                    .map(|c| c.message)
                    .ok_or_else(|| {
                        ApiError::Internal("agent turn: provider returned no choices".into())
                    })?;
                crate::metrics::record_request_served("agent_run", "dispatch");
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
    /// Admission guard on the run's accumulated served cost (USD). When set,
    /// the loop stops before a new turn when accrued cost plus an available
    /// best-effort estimate reaches the cap; a started turn can settle above it.
    /// None => no cost cap.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// When true, `POST /v1/agent/runs` streams run events as SSE (slice 3b)
    /// instead of returning a single JSON `Run`. Default false.
    #[serde(default)]
    pub stream: bool,
}

/// Build the production summarizer + completer (borrowing `state` + `identity`)
/// and run the loop with an optional event sink. Shared by the JSON and SSE paths.
#[allow(clippy::too_many_arguments)]
async fn drive_run_loop(
    state: &AppState,
    identity: RunIdentity,
    id: Uuid,
    model: String,
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    max_cost_usd: Option<f64>,
    summarize_cfg: Option<SummarizeConfig>,
    events: Option<&tokio::sync::mpsc::UnboundedSender<RunEvent>>,
) -> LoopOutcome {
    drive_run_loop_with_output_cap(
        state,
        identity,
        id,
        model,
        messages,
        tools,
        max_turns,
        None,
        max_cost_usd,
        summarize_cfg,
        events,
    )
    .await
}

/// Variant used by workflow nodes with an explicit per-turn completion cap.
/// The cap is forwarded unchanged to both preview admission and the provider
/// request; it remains a request parameter rather than a settlement guarantee.
#[allow(clippy::too_many_arguments)]
async fn drive_run_loop_with_output_cap(
    state: &AppState,
    identity: RunIdentity,
    id: Uuid,
    model: String,
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    max_output_tokens: Option<u32>,
    max_cost_usd: Option<f64>,
    summarize_cfg: Option<SummarizeConfig>,
    events: Option<&tokio::sync::mpsc::UnboundedSender<RunEvent>>,
) -> LoopOutcome {
    let base_provider_id = state.registry.resolve(&model).map(|p| p.id().to_string());
    let summarizer_model = summarizer_model(state);
    let base_ctx = base_request_context(&identity);
    let summarizer_obj = summarize_cfg.map(|cfg| GatewayTranscriptSummarizer {
        state,
        org_id: identity.org_id,
        raw_bearer: identity.raw_bearer.clone(),
        base_ctx,
        gate: state.summary_gate.clone(),
        cfg,
        base_model: model.clone(),
        base_provider_id,
        summarizer_model,
        deadline: state.judge_config.baseline_timeout,
    });
    let summ_ref: Option<&dyn TranscriptSummarizer> = summarizer_obj
        .as_ref()
        .map(|s| s as &dyn TranscriptSummarizer);
    let completer = GatewayCompleter { state, identity };
    run_loop_core_with_output_cap(
        &completer,
        id,
        model,
        messages,
        tools,
        max_turns,
        max_output_tokens,
        max_cost_usd,
        0,
        0,
        RunUsage::default(),
        summ_ref,
        events,
    )
    .await
}

/// Drive ONE workflow node through the gateway agent path.
///
/// Model nodes should pass `max_turns = 1`; Agent nodes pass their configured
/// cap.  Builds a per-node [`RunIdentity`] — the workflow's `run_id` is stamped
/// on every request_log row for billing attribution; `trace_id` and
/// `idempotency_key` are fresh per invocation.
///
/// # node_id
/// `node_id` is accepted for future attribution: in W1a it is NOT threaded into
/// `request_logs` (the `RunIdentity` has no `node_id` field; the ctx seam that
/// carries it exists but is always `None` today).  The `run_id` attribution is
/// the load-bearing part for billing.  Node-level logging is a W2 follow-up.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive_workflow_node(
    state: &AppState,
    org_id: Uuid,
    api_key_id: Uuid,
    caller_tier: Option<tt_shared::CallerTier>,
    l2_allowed: bool,
    raw_bearer: String,
    run_id: Uuid,
    node_id: String,
    model: String,
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    max_output_tokens: Option<u32>,
    max_cost_usd: Option<f64>,
    route_ref: Option<String>,
    tag: Option<String>,
) -> LoopOutcome {
    // node_id: not yet threaded to request_logs in W1a (see doc comment).
    let _ = node_id;

    let identity = RunIdentity {
        org_id,
        api_key_id,
        caller_tier,
        l2_allowed,
        skip_shadow: false,
        raw_bearer,
        // Fresh trace + idempotency per node invocation — a workflow run may
        // invoke many nodes concurrently in later tasks; per-node traces keep
        // them distinguishable in the request_logs table.
        trace_id: Uuid::new_v4(),
        tag,
        // No per-node request timeout — the workflow engine owns deadline mgmt.
        request_timeout: None,
        // No provider pin — let routing / forced_route drive provider selection.
        provider_pin: None,
        // route_ref → forced_route: the node's ModelSelection::Route variant
        // sets this so the per-turn routing step picks the right named route.
        forced_route: route_ref,
        idempotency_key: Uuid::new_v4().to_string(),
        // No incoming HTTP headers for a workflow node; the per-turn completer
        // resolves everything from the identity fields above.
        headers: HeaderMap::new(),
        run_id,
    };
    drive_run_loop_with_output_cap(
        state,
        identity,
        run_id,
        model,
        messages,
        tools,
        max_turns,
        max_output_tokens,
        max_cost_usd,
        None, // no summarizer in W1a workflow nodes
        None, // no SSE event sink
    )
    .await
}

/// Handle a paused run: Redis present ⇒ persist a `RequiresAction` `StoredRun`
/// (returns its `Run` view); no Redis ⇒ the 1a `Incomplete` fallback `Run`. The
/// returned `Run.status` discriminates the two for the caller.
#[allow(clippy::too_many_arguments)]
async fn persist_paused(
    state: &AppState,
    id: Uuid,
    org_id: Uuid,
    routing: StoredRouting,
    model: String,
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    max_cost_usd: Option<f64>,
    turns_done: u32,
    usage: RunUsage,
    pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
    summarized_upto: u32,
    summarizer_tax_usd: Option<f64>,
    summarize_cfg: Option<SummarizeConfig>,
) -> ApiResult<Run> {
    match state.l1.as_ref() {
        Some(l1) => {
            let stored = StoredRun {
                id,
                org_id,
                status: RunStatus::RequiresAction,
                model,
                messages,
                tools,
                max_turns,
                max_cost_usd,
                turns_done,
                usage,
                pending_tool_calls,
                routing,
                summarized_upto,
                summarizer_tax_usd,
                summarize: summarize_cfg,
                stop_reason: None,
            };
            store_run(l1.cache.as_ref(), &stored).await?;
            Ok(stored.to_run())
        }
        None => {
            let name = pending_tool_calls
                .first()
                .map(|tc| tc.function.name.clone())
                .unwrap_or_default();
            Ok(Run {
                id,
                status: RunStatus::Incomplete,
                messages,
                turns: turns_done,
                usage,
                note: Some(format!(
                    "client tool '{name}' requires Redis to pause/resume (none configured)"
                )),
                summarizer_tax_usd,
                stop_reason: None,
            })
        }
    }
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
) -> ApiResult<Response> {
    let mut identity = RunIdentity::from_request(auth_ctx.as_deref(), trace.0.as_str(), &headers);
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
    let max_cost_usd = req.max_cost_usd;

    // Resolve the run's summarize policy ONCE from the turn-0 route, then build
    // the production summarizer. Done BEFORE `identity` is moved into the
    // completer and BEFORE `req.messages` is moved into the loop. `None` policy
    // ⇒ summarize off (the `summarizer: None` loop path is byte-identical to
    // pre-2c-1). With the default `NeverCommit` gate, `is_committable` is always
    // false, so no summarize dispatch ever happens even when a route opts in.
    let summarize_cfg =
        resolve_summarize_config(&state, &identity, &req.model, &req.messages).await;

    let id = Uuid::new_v4();
    // Stamp the run id onto the identity so each turn's RequestContext carries it.
    identity.run_id = id;

    // W0b Task 5: insert a "running" row in agent_runs (best-effort; pool
    // absence ⇒ skip, mirroring the l1/Redis optional guard pattern).
    if let Some(pool) = state.db_pool.as_ref() {
        let rec = crate::routes::agent_run_store::AgentRunRecord::new_running(
            id,
            org_id,
            model.clone(),
            Some(max_turns),
            max_cost_usd,
            identity.tag.clone(),
        );
        crate::routes::agent_run_store::insert_agent_run(pool, &rec).await;
    }

    if req.stream {
        let owned_state = state.clone(); // cheap Arcs; 'static for the spawned task
        let messages = req.messages;
        let (tx, rx) = mpsc::unbounded_channel::<RunEvent>();
        tokio::spawn(async move {
            let outcome = drive_run_loop(
                &owned_state,
                identity,
                id,
                model.clone(),
                messages,
                tools.clone(),
                max_turns,
                max_cost_usd,
                summarize_cfg.clone(),
                Some(&tx),
            )
            .await;
            match outcome {
                LoopOutcome::Terminal(run) => {
                    // W0b Task 5: finalize agent_runs row (best-effort).
                    // Copy fields before `run` is moved into the SSE event below
                    // (all of id/status/turns/cost_usd/stop_reason are Copy).
                    if let Some(pool) = owned_state.db_pool.as_ref() {
                        crate::routes::agent_run_store::finish_agent_run(
                            pool,
                            run.id,
                            org_id,
                            run.status,
                            run.turns,
                            run.usage.cost_usd,
                            run.stop_reason,
                        )
                        .await;
                    }
                    let _ = tx.send(match run.status {
                        RunStatus::Completed => RunEvent::Completed { run },
                        RunStatus::Failed => RunEvent::Failed { run },
                        _ => RunEvent::Incomplete { run }, // max_turns or budget_exhausted (both Incomplete; stop_reason disambiguates)
                    });
                }
                LoopOutcome::Paused {
                    messages,
                    turns_done,
                    usage,
                    pending_tool_calls,
                    summarized_upto,
                    summarizer_tax_usd,
                } => {
                    let pending = pending_tool_calls.clone();
                    match persist_paused(
                        &owned_state,
                        id,
                        org_id,
                        routing,
                        model,
                        messages,
                        tools,
                        max_turns,
                        max_cost_usd,
                        turns_done,
                        usage,
                        pending_tool_calls,
                        summarized_upto,
                        summarizer_tax_usd,
                        summarize_cfg,
                    )
                    .await
                    {
                        Ok(run) => {
                            // W0b Task 6 paused-status fix (streaming path):
                            // same best-effort mark as the non-streaming path.
                            if run.status == RunStatus::RequiresAction {
                                if let Some(pool) = owned_state.db_pool.as_ref() {
                                    crate::routes::agent_run_store::mark_run_requires_action(
                                        pool, id, org_id,
                                    )
                                    .await;
                                }
                            }
                            let _ = tx.send(match run.status {
                                RunStatus::RequiresAction => RunEvent::RequiresAction {
                                    run,
                                    pending_tool_calls: pending,
                                },
                                _ => RunEvent::Incomplete { run }, // no-Redis fallback
                            });
                        }
                        Err(e) => {
                            tracing::warn!(run_id = %id, error = %e, "agent run persist failed");
                            let _ = tx.send(RunEvent::Failed {
                                run: Run {
                                    id,
                                    status: RunStatus::Failed,
                                    messages: vec![],
                                    turns: turns_done,
                                    usage: RunUsage::default(),
                                    note: Some(format!("persist failed: {e}")),
                                    summarizer_tax_usd: None,
                                    stop_reason: None,
                                },
                            });
                        }
                    }
                }
            }
            // tx dropped here ⇒ the stream ends after [DONE]
        });
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|ev| (Ok::<_, std::convert::Infallible>(ev.to_sse()), rx))
        })
        .chain(futures::stream::once(async {
            Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"))
        }));
        return Ok(Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response());
    }

    let outcome = drive_run_loop(
        &state,
        identity,
        id,
        model.clone(),
        req.messages,
        tools.clone(),
        max_turns,
        max_cost_usd,
        summarize_cfg.clone(),
        None,
    )
    .await;
    match outcome {
        LoopOutcome::Terminal(run) => {
            // W0b Task 5: finalize agent_runs row (best-effort; pool absence ⇒ skip).
            if let Some(pool) = state.db_pool.as_ref() {
                crate::routes::agent_run_store::finish_agent_run(
                    pool,
                    run.id,
                    org_id,
                    run.status,
                    run.turns,
                    run.usage.cost_usd,
                    run.stop_reason,
                )
                .await;
            }
            Ok(Json(run).into_response())
        }
        LoopOutcome::Paused {
            messages,
            turns_done,
            usage,
            pending_tool_calls,
            summarized_upto,
            summarizer_tax_usd,
        } => {
            let run = persist_paused(
                &state,
                id,
                org_id,
                routing,
                model,
                messages,
                tools,
                max_turns,
                max_cost_usd,
                turns_done,
                usage,
                pending_tool_calls,
                summarized_upto,
                summarizer_tax_usd,
                summarize_cfg,
            )
            .await?;
            // W0b Task 6 paused-status fix: mark the durable row as
            // `requires_action` so a stale paused run (Redis TTL expired)
            // reads correctly rather than orphaned as `running`. Best-effort.
            if run.status == RunStatus::RequiresAction {
                if let Some(pool) = state.db_pool.as_ref() {
                    crate::routes::agent_run_store::mark_run_requires_action(pool, id, org_id)
                        .await;
                }
            }
            Ok(Json(run).into_response())
        }
    }
}

/// `GET /v1/agent/runs/:id` — fetch a persisted run's current state.
///
/// Org is derived from the authenticated key (a real key is required;
/// anonymous / dogfood callers get 401). Look-up order:
///
/// 1. **Redis (L1)** — if configured. A hit returns the FULL run including the
///    live transcript (messages).
/// 2. **Postgres fallback** (W0b Task 6) — on a Redis miss (key absent/expired)
///    or when no Redis is configured, the durable `agent_runs` row is returned.
///    The transcript is NOT persisted to Postgres; the response has
///    `messages: []` and a note explaining the absence.
/// 3. **404** — if neither store holds the run for the caller's org.
///
/// Org-scoping is preserved on both paths: Redis keys include the org id; the
/// Postgres query has `WHERE id=$1 AND org_id=$2`.
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

    // (1) Try Redis first — richer view (includes the live transcript).
    if let Some(l1) = state.l1.as_ref() {
        match fetch_run(l1.cache.as_ref(), org, id).await {
            Ok(Some(stored)) => return Ok(Json(stored.to_run())),
            Ok(None) => {} // key absent → fall through to Postgres
            Err(e) => {
                tracing::warn!(run_id = %id, error = %e, "agent run Redis fetch failed; falling back to Postgres");
                // fall through to Postgres
            }
        }
    }

    // (2) Postgres fallback — durable identity + terminal state, no transcript.
    if let Some(pool) = state.db_pool.as_ref() {
        if let Some(rec) = crate::routes::agent_run_store::get_agent_run(pool, id, org).await {
            return Ok(Json(durable_record_to_run(&rec)));
        }
    }

    // (3) Neither store holds a run for this (org, id).
    Err(ApiError::NotFound(format!("no run with id {id}")))
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
    // Restore the run id from the persisted record so resumed turns carry the
    // same run_id on their request_logs rows as the initial turns did.
    identity.run_id = stored.id;

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
        stored.max_cost_usd,
        stored.turns_done,
        stored.summarized_upto,
        stored.usage.clone(),
        summ_ref,
        None, /*events*/
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
            // W0b Task 5: finalize agent_runs row (best-effort; pool absence ⇒ skip).
            // run.id/status/turns/cost_usd/stop_reason are all Copy so they survive
            // the borrow used for the Redis store above.
            if let Some(pool) = state.db_pool.as_ref() {
                crate::routes::agent_run_store::finish_agent_run(
                    pool,
                    run.id,
                    stored.org_id,
                    run.status,
                    run.turns,
                    run.usage.cost_usd,
                    run.stop_reason,
                )
                .await;
            }
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
            // W0b Task 6 paused-status fix (submit_tool_outputs re-pause):
            // mark durable row as requires_action so a stale re-paused run
            // reads correctly after its Redis TTL expires. Best-effort.
            if let Some(pool) = state.db_pool.as_ref() {
                crate::routes::agent_run_store::mark_run_requires_action(
                    pool,
                    stored.id,
                    stored.org_id,
                )
                .await;
            }
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
            // Capture the judge inputs BEFORE the in-place overwrite of messages[idx].
            let tool_call_id = match &messages[idx] {
                Message::Tool { tool_call_id, .. } => tool_call_id.clone(),
                _ => String::new(),
            };
            let input = latest_user_text(messages);
            if let Message::Tool { content, .. } = &mut messages[idx] {
                *content = MessageContent::Text(summary.clone());
            }
            // Sample + detached-judge the committed summary (feeds the ratchet
            // for FUTURE turns/runs). No-op unless this commit is sampled.
            self.maybe_spawn_summary_judge(&tool_call_id, &input, &class, original, summary);
        }
        *summarized_upto = tool_count.saturating_sub(self.cfg.keep_recent_pairs);
        tax
    }
}

impl GatewayTranscriptSummarizer<'_> {
    /// Sample (~`judge_config.sample_rate`) a freshly-committed summary and, if
    /// sampled, spawn a DETACHED blind judge (zero added latency) comparing the
    /// original vs the summary (recall-of-baseline). Its verdict feeds the gate's
    /// ratchet for FUTURE turns/runs. Fail-open: any resolve/dispatch failure ⇒
    /// no verdict recorded (a flaky judge must never shut a class). Owned clones
    /// only — never captures `&self`/`&self.state` (the spawn is `'static + Send`).
    fn maybe_spawn_summary_judge(
        &self,
        tool_call_id: &str,
        input: &str,
        class: &str,
        original: String,
        summary: String,
    ) {
        let key = sample_key(self.base_ctx.trace_id, tool_call_id);
        if !quality_sample::should_sample(key, self.state.judge_config.sample_rate) {
            return;
        }
        // Owned captures only — the spawned future is `'static + Send`.
        let state = self.state.clone(); // AppState: Clone — owned, not the borrow
        let gate = self.gate.clone();
        let org_id = self.org_id;
        let raw_bearer = self.raw_bearer.clone();
        let base_ctx = self.base_ctx.clone();
        let class = class.to_string();
        let input = input.to_string();
        tokio::spawn(async move {
            let Some(provider) = state.registry.resolve(&state.judge_config.judge_model) else {
                return;
            };
            let Some(creds) =
                chat::resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, true)
                    .await
            else {
                return;
            };
            let judge_ctx = RequestContext {
                credentials: creds,
                ..base_ctx
            };
            let judge =
                GatewayLlmJudge::new(provider, state.judge_config.judge_model.clone(), judge_ctx)
                    .with_call_timeout(state.judge_config.baseline_timeout);
            // `summary` is the OPTIMIZED arg + the matching `order`, so the returned
            // verdict reads as the SUMMARY's recall-of-baseline verdict.
            match quality_sample::judge_paired(
                &judge,
                &input,
                &original,
                &summary,
                quality_sample::ab_order_for(key),
                false,
            )
            .await
            {
                Ok(outcome) => {
                    tracing::info!(
                        target: "tokentrimmer.summary_judge",
                        class = %class,
                        verdict = quality_sample::verdict_str(outcome.verdict),
                        cost_usd = ?outcome.judge_cost_usd,
                        "summary judge verdict"
                    );
                    gate.record_summary_verdict(&class, outcome.verdict);
                }
                Err(_failure) => {
                    tracing::debug!(
                        target: "tokentrimmer.summary_judge",
                        class = %class,
                        "summary judge failed; no verdict recorded (fail-open)"
                    );
                }
            }
        });
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
        run_id: None,
        node_id: None,
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
                    cost_usd: 0.0,
                    ..Default::default()
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
                    cost_usd: 0.0,
                    ..Default::default()
                },
            ))
        }
    }

    /// Records the provider-facing completion cap so workflow propagation can
    /// be checked without a provider, run record, or network dispatch.
    struct OutputCapRecordingStub {
        caps: std::sync::Mutex<Vec<Option<u32>>>,
    }

    #[async_trait]
    impl TurnCompleter for OutputCapRecordingStub {
        async fn complete(
            &self,
            req: ChatCompletionRequest,
            _is_mechanical: bool,
        ) -> Result<(Message, RunUsage), ApiError> {
            self.caps.lock().unwrap().push(req.max_tokens);
            Ok((assistant_final(), RunUsage::default()))
        }
    }

    fn assistant_final() -> Message {
        Message::Assistant {
            content: Some(MessageContent::Text("done".into())),
            tool_calls: vec![],
            name: None,
        }
    }

    #[tokio::test]
    async fn bounded_workflow_turn_forwards_its_explicit_output_cap() {
        let stub = OutputCapRecordingStub {
            caps: std::sync::Mutex::new(vec![]),
        };
        let outcome = run_loop_core_with_output_cap(
            &stub,
            Uuid::nil(),
            "gpt-4o-mini".into(),
            vec![],
            vec![],
            1,
            Some(37),
            None,
            0,
            0,
            RunUsage::default(),
            None,
            None,
        )
        .await;

        assert!(matches!(outcome, LoopOutcome::Terminal(_)));
        assert_eq!(*stub.caps.lock().unwrap(), vec![Some(37)]);
    }

    #[tokio::test]
    async fn uncapped_turn_keeps_the_provider_default_output_setting() {
        let stub = OutputCapRecordingStub {
            caps: std::sync::Mutex::new(vec![]),
        };
        let outcome = run_loop_core(
            &stub,
            Uuid::nil(),
            "gpt-4o-mini".into(),
            vec![],
            vec![],
            1,
            None,
            0,
            0,
            RunUsage::default(),
            None,
            None,
        )
        .await;

        assert!(matches!(outcome, LoopOutcome::Terminal(_)));
        assert_eq!(*stub.caps.lock().unwrap(), vec![None]);
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

    // ----- Task 6: durable_record_to_run mapping (written first, TDD) -----

    #[test]
    fn durable_record_to_run_maps_status_and_stop_reason() {
        use crate::routes::agent_run_store::AgentRunRecord;
        let mut rec = AgentRunRecord {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            status: "completed".into(),
            model: "gpt-4o".into(),
            turns: 3,
            max_turns: Some(8),
            max_cost_usd: Some(1.0),
            cost_usd: 0.42,
            stop_reason: None,
            tag: None,
        };
        let run = durable_record_to_run(&rec);
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 3);
        assert!((run.usage.cost_usd - 0.42).abs() < 1e-9, "cost preserved");
        assert!(run.messages.is_empty(), "no transcript in durable view");
        assert!(run.note.is_some(), "note explains absent transcript");
        assert!(run.stop_reason.is_none());

        // incomplete + budget_exhausted
        rec.status = "incomplete".into();
        rec.stop_reason = Some("budget_exhausted".into());
        let run = durable_record_to_run(&rec);
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(
            run.stop_reason,
            Some(crate::routes::agent_run_budget::StopReason::BudgetExhausted)
        );

        // max_turns stop reason
        rec.stop_reason = Some("max_turns".into());
        let run = durable_record_to_run(&rec);
        assert_eq!(
            run.stop_reason,
            Some(crate::routes::agent_run_budget::StopReason::MaxTurns)
        );

        // orphaned "running" row → Incomplete fallback
        rec.status = "running".into();
        rec.stop_reason = None;
        let run = durable_record_to_run(&rec);
        assert_eq!(run.status, RunStatus::Incomplete);

        // requires_action maps correctly
        rec.status = "requires_action".into();
        let run = durable_record_to_run(&rec);
        assert_eq!(run.status, RunStatus::RequiresAction);

        // failed maps correctly
        rec.status = "failed".into();
        let run = durable_record_to_run(&rec);
        assert_eq!(run.status, RunStatus::Failed);
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
            None, /*max_cost_usd*/
            0,
            0, /*summarized_upto*/
            RunUsage::default(),
            None, /*summarizer*/
            None, /*events*/
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
            None, /*max_cost_usd*/
            1,
            0, /*summarized_upto*/
            RunUsage::default(),
            None, /*summarizer*/
            None, /*events*/
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
            None, /*max_cost_usd*/
            0,
            0, /*summarized_upto*/
            RunUsage::default(),
            None, /*summarizer*/
            None, /*events*/
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
            skip_shadow: false,
        };
        let id = RunIdentity::from_request(Some(&ctx), "", &HeaderMap::new());
        assert_eq!(id.org_id, Uuid::from_u128(2));
        assert_eq!(id.api_key_id, Uuid::from_u128(1));
        assert!(id.l2_allowed);
    }

    /// W0b Task 4: `run_id` is initialized to `Uuid::nil()` by `from_request`
    /// and then overwritten by the handler with the real run id. Verify the nil
    /// default exists (the handler assignment is integration-tested at the
    /// complete_once/request_logs level).
    #[test]
    fn run_identity_run_id_initialized_to_nil() {
        let id = RunIdentity::from_request(None, "", &HeaderMap::new());
        assert_eq!(
            id.run_id,
            Uuid::nil(),
            "run_id must be nil from from_request; handler sets it to the real run id"
        );
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
                cost_usd: 0.0,
                ..Default::default()
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
            max_cost_usd: None,
            stop_reason: None,
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
            max_cost_usd: None,
            stop_reason: None,
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
            max_cost_usd: None,
            stop_reason: None,
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
            max_cost_usd: None,
            stop_reason: None,
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
            max_cost_usd: None,
            stop_reason: None,
        };
        assert_eq!(sr.to_run().summarizer_tax_usd, Some(0.0004));
    }

    // ----- eligible_tool_ordinals + token_true_ok (slice 2c-1 Task 4) -----

    #[test]
    fn eligible_ordinals_preserves_recent_blocks_and_respects_watermark() {
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
        // The structured control only permits keep>=3: with 3 blocks, only
        // the oldest is eligible; with 4 (or more), every block is preserved.
        assert_eq!(eligible_tool_ordinals(&msgs, 0, 3), vec![1]);
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

    // ----- run_loop_core optional event sink (slice 3b Task 2) -----

    #[tokio::test]
    async fn loop_emits_run_events_to_sink() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunEvent>();
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            None, /*max_cost_usd*/
            0,
            0,
            RunUsage::default(),
            None,
            Some(&tx),
        )
        .await;
        assert!(matches!(out, LoopOutcome::Terminal(_)));
        drop(tx);
        let mut evs = vec![];
        while let Some(ev) = rx.recv().await {
            evs.push(ev);
        }
        let names: Vec<&str> = evs.iter().map(|e| e.event_name()).collect();
        assert_eq!(
            names,
            vec![
                "run.turn",
                "run.turn_cost",
                "run.message",
                "run.tool_result",
                "run.turn",
                "run.turn_cost",
                "run.message"
            ]
        );
        assert!(matches!(evs[0], RunEvent::Turn { turn: 1 })); // 1-indexed
    }

    #[tokio::test]
    async fn loop_emits_turn_cost_events() {
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
            cost_per_turn: 0.25,
            baseline_per_turn: 0.0,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunEvent>();
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            Some(1.0),
            0,
            0,
            RunUsage::default(),
            None,
            Some(&tx),
        )
        .await;
        assert!(matches!(out, LoopOutcome::Terminal(_)));
        drop(tx);
        let mut costs = vec![];
        while let Some(ev) = rx.recv().await {
            if let RunEvent::TurnCost {
                turn,
                turn_cost_usd,
                run_cost_usd,
                budget_remaining_usd,
            } = ev
            {
                costs.push((turn, turn_cost_usd, run_cost_usd, budget_remaining_usd));
            }
        }
        assert_eq!(
            costs,
            vec![(1, 0.25, 0.25, Some(0.75)), (2, 0.25, 0.50, Some(0.50))]
        );
    }

    #[tokio::test]
    async fn loop_with_no_event_sink_emits_nothing() {
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
            None, /*max_cost_usd*/
            0,
            0,
            RunUsage::default(),
            None,
            None,
        )
        .await;
        assert!(matches!(out, LoopOutcome::Terminal(_)));
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
            None, /*max_cost_usd*/
            0,
            0,
            RunUsage::default(),
            Some(&summ),
            None, /*events*/
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
            None, /*max_cost_usd*/
            0,
            0,
            RunUsage::default(),
            None,
            None, /*events*/
        )
        .await;
        match out {
            LoopOutcome::Terminal(run) => assert_eq!(run.summarizer_tax_usd, None),
            _ => panic!("expected Terminal"),
        }
    }

    #[test]
    fn run_usage_deserializes_without_cost_usd() {
        // A RunUsage persisted before this deploy has no cost_usd key; #[serde(default)]
        // must default it to 0.0 (so old StoredRun.usage round-trips).
        let ru: RunUsage = serde_json::from_str(r#"{"prompt_tokens":5,"completion_tokens":7}"#)
            .expect("back-compat deserialize");
        assert_eq!(ru.prompt_tokens, 5);
        assert_eq!(ru.completion_tokens, 7);
        assert_eq!(ru.cost_usd, 0.0);
    }

    /// A completer that returns a fixed per-turn served cost and baseline cost (+
    /// a scripted message), so a test can assert the loop accumulates
    /// `usage.cost_usd` and `usage.baseline_cost_usd` across turns.
    struct CostStub {
        script: std::sync::Mutex<Vec<Message>>,
        cost_per_turn: f64,
        baseline_per_turn: f64,
    }
    #[async_trait]
    impl TurnCompleter for CostStub {
        async fn complete(
            &self,
            _req: ChatCompletionRequest,
            _is_mechanical: bool,
        ) -> Result<(Message, RunUsage), ApiError> {
            Ok((
                self.script.lock().unwrap().remove(0),
                RunUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    cost_usd: self.cost_per_turn,
                    baseline_cost_usd: self.baseline_per_turn,
                    ..Default::default()
                },
            ))
        }
    }

    #[tokio::test]
    async fn loop_accumulates_served_cost_across_turns() {
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
            cost_per_turn: 0.25,
            baseline_per_turn: 0.0,
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 2);
        assert_eq!(run.usage.cost_usd, 0.5); // 2 * 0.25, exact in f64
    }

    #[tokio::test]
    async fn loop_cost_continues_from_restored_usage_on_resume() {
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
            cost_per_turn: 0.5,
            baseline_per_turn: 0.0,
        };
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            None, // max_cost_usd
            0,    // turns_done
            0,    // summarized_upto
            RunUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                cost_usd: 1.0,
                ..Default::default()
            }, // restored carry-in
            None, // summarizer
            None, // events
        )
        .await;
        match out {
            LoopOutcome::Terminal(run) => assert_eq!(run.usage.cost_usd, 2.0),
            _ => panic!("expected Terminal"),
        }
    }

    // ----- RunEvent enum (slice 3b Task 1) -----

    // ----- Budget cap + stop_reason (W0a Task 2) -----

    /// Test helper: drive the loop from scratch with an explicit cost cap.
    async fn run_loop_capped(
        completer: &dyn TurnCompleter,
        id: uuid::Uuid,
        model: String,
        messages: Vec<Message>,
        tools: Vec<tt_shared::messages::Tool>,
        max_turns: u32,
        max_cost_usd: Option<f64>,
    ) -> Run {
        match run_loop_core(
            completer,
            id,
            model,
            messages,
            tools,
            max_turns,
            max_cost_usd,
            0,
            0,
            RunUsage::default(),
            None,
            None,
        )
        .await
        {
            LoopOutcome::Terminal(run) => run,
            LoopOutcome::Paused { .. } => panic!("unexpected pause in capped test"),
        }
    }

    #[tokio::test]
    async fn loop_stops_when_accrued_cost_reaches_cap() {
        // Each turn costs 0.25; cap is 0.40. After turn 1 (0.25) the loop runs
        // turn 2 (0.50 >= 0.40), then refuses turn 3 -> Incomplete/BudgetExhausted.
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_toolcall("find_route_for"),
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
            cost_per_turn: 0.25,
            baseline_per_turn: 0.0,
        };
        let run = run_loop_capped(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            Some(0.40),
        )
        .await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(
            run.stop_reason,
            Some(crate::routes::agent_run_budget::StopReason::BudgetExhausted)
        );
        assert_eq!(run.turns, 2);
        assert_eq!(run.usage.cost_usd, 0.50);
    }

    #[tokio::test]
    async fn resume_with_carried_cost_stops_immediately_when_over_cap() {
        // A resumed run that already accrued 0.45 under a 0.40 cap must stop on the
        // first loop iteration with BudgetExhausted, running ZERO new turns.
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![assistant_final()]),
            cost_per_turn: 0.25,
            baseline_per_turn: 0.0,
        };
        let carried = RunUsage {
            cost_usd: 0.45,
            ..Default::default()
        };
        let out = run_loop_core(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            Some(0.40),
            /* turns_done */ 2,
            /* summarized_upto */ 0,
            carried,
            None,
            None,
        )
        .await;
        match out {
            LoopOutcome::Terminal(run) => {
                assert_eq!(run.status, RunStatus::Incomplete);
                assert_eq!(
                    run.stop_reason,
                    Some(crate::routes::agent_run_budget::StopReason::BudgetExhausted)
                );
                assert_eq!(run.turns, 2); // unchanged — no new turn ran
                assert_eq!(run.usage.cost_usd, 0.45); // unchanged
            }
            LoopOutcome::Paused { .. } => panic!("unexpected pause"),
        }
    }

    #[tokio::test]
    async fn loop_stops_before_a_turn_that_would_exceed_cap() {
        // A real catalog model so estimate_next_turn_cost returns Some(>0).
        // cap is tiny (0.0001) so accrued(0) + est >= cap on the FIRST turn:
        // the loop must stop with 0 turns run.
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![assistant_final()]),
            cost_per_turn: 0.25,
            baseline_per_turn: 0.0,
        };
        let msgs = vec![Message::User {
            content: MessageContent::Text("estimate me".into()),
            name: None,
        }];
        let run = run_loop_capped(
            &stub,
            uuid::Uuid::nil(),
            "gpt-4o-mini".into(),
            msgs,
            vec![],
            8,
            Some(0.0001),
        )
        .await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(
            run.stop_reason,
            Some(crate::routes::agent_run_budget::StopReason::BudgetExhausted)
        );
        assert_eq!(run.turns, 0);
        assert_eq!(run.usage.cost_usd, 0.0);
    }

    #[tokio::test]
    async fn loop_without_cap_is_unchanged() {
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
            cost_per_turn: 0.25,
            baseline_per_turn: 0.0,
        };
        let run = run_loop_capped(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            None,
        )
        .await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.stop_reason, None);
        assert_eq!(run.turns, 2);
        assert_eq!(run.usage.cost_usd, 0.50);
    }

    #[tokio::test]
    async fn loop_max_turns_sets_stop_reason() {
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_toolcall("find_route_for"),
            ]),
            cost_per_turn: 0.0,
            baseline_per_turn: 0.0,
        };
        let run = run_loop_capped(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            2,
            None,
        )
        .await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(
            run.stop_reason,
            Some(crate::routes::agent_run_budget::StopReason::MaxTurns)
        );
        assert_eq!(run.turns, 2);
    }

    /// A gateway-only loop that re-issues the SAME tool call (and gets the same
    /// result) every turn is terminated as a runaway at the threshold — well
    /// before the generous `max_turns` would stop it — preserving the partial
    /// transcript (Incomplete, not Failed).
    #[tokio::test]
    async fn loop_runaway_terminates_with_stop_reason() {
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_toolcall("find_route_for"),
                assistant_toolcall("find_route_for"),
            ]),
            cost_per_turn: 0.10,
            baseline_per_turn: 0.0,
        };
        let run = run_loop_capped(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8, // generous turn cap — the runaway guard must fire first
            None,
        )
        .await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(
            run.stop_reason,
            Some(crate::routes::agent_run_budget::StopReason::Runaway),
            "3 identical tool-call/result steps must trip the runaway guard"
        );
        // Tripped at the 3rd identical step, not at max_turns (8).
        assert_eq!(run.turns, 3);
        assert!(
            run.note
                .as_deref()
                .is_some_and(|n| n.contains("no progress")),
            "note explains the runaway termination: {:?}",
            run.note
        );
    }

    /// A loop that alternates between two DIFFERENT tool calls never repeats a
    /// step signature, so the runaway guard stays silent and the run completes
    /// normally on the final (tool-call-free) answer.
    #[tokio::test]
    async fn loop_alternating_tools_does_not_trip_runaway() {
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_toolcall("preview_cost"),
                assistant_toolcall("find_route_for"),
                assistant_toolcall("preview_cost"),
                assistant_final(),
            ]),
            cost_per_turn: 0.01,
            baseline_per_turn: 0.0,
        };
        let run = run_loop_capped(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            None,
        )
        .await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.stop_reason, None);
        assert_eq!(run.turns, 5);
    }

    #[test]
    fn stored_run_deserializes_without_max_cost_usd() {
        // A StoredRun JSON written by a prior deploy (no max_cost_usd/stop_reason)
        // must still deserialize (fields default to None).
        let json = r#"{"id":"00000000-0000-0000-0000-000000000000","org_id":"00000000-0000-0000-0000-000000000000","status":"requires_action","model":"m","messages":[],"tools":[],"max_turns":8,"turns_done":1,"usage":{"prompt_tokens":1,"completion_tokens":1},"pending_tool_calls":[],"routing":{"provider_pin":null,"forced_route":null,"tag":null}}"#;
        let sr: StoredRun = serde_json::from_str(json).expect("legacy StoredRun must deserialize");
        assert_eq!(sr.max_cost_usd, None);
        assert_eq!(sr.stop_reason, None);
    }

    #[tokio::test]
    async fn run_records_per_turn_cost() {
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
            cost_per_turn: 0.25,
            baseline_per_turn: 0.0,
        };
        let run = run_loop_capped(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            None,
        )
        .await;
        assert_eq!(run.usage.per_turn_cost_usd, vec![0.25, 0.25]);
        assert_eq!(run.usage.cost_usd, 0.50);
    }

    #[test]
    fn run_usage_deserializes_without_per_turn_cost_usd() {
        let json = r#"{"prompt_tokens":1,"completion_tokens":1,"cost_usd":0.5}"#;
        let u: RunUsage = serde_json::from_str(json).expect("legacy RunUsage must deserialize");
        assert!(u.per_turn_cost_usd.is_empty());
    }

    #[test]
    fn run_event_serializes_with_type_tag_and_event_name() {
        let ev = RunEvent::Turn { turn: 1 };
        assert_eq!(ev.event_name(), "run.turn");
        assert_eq!(serde_json::to_value(&ev).unwrap()["type"], "run.turn");

        let m = RunEvent::Message {
            message: assistant_final(),
        };
        assert_eq!(m.event_name(), "run.message");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["type"], "run.message");
        assert!(v.get("message").is_some());

        let tr = RunEvent::ToolResult {
            tool_call_id: "c1".into(),
            content: "r".into(),
        };
        assert_eq!(tr.event_name(), "run.tool_result");
    }

    // ----- RunUsage.baseline_cost_usd accrual (W2a Task 2) -----

    #[tokio::test]
    async fn loop_accumulates_baseline_cost_across_turns() {
        // Two turns: tool-call then final. Each costs 0.25 served / 0.60 baseline.
        let stub = CostStub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
            cost_per_turn: 0.25,
            baseline_per_turn: 0.60,
        };
        let run = run_loop_capped(
            &stub,
            uuid::Uuid::nil(),
            "m".into(),
            vec![],
            vec![],
            8,
            None,
        )
        .await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 2);
        assert_eq!(run.usage.cost_usd, 0.50); // 2 * 0.25, exact in f64
        assert_eq!(run.usage.baseline_cost_usd, 1.20); // 2 * 0.60, exact in f64
    }

    #[test]
    fn run_usage_deserializes_without_baseline_cost_usd() {
        // A RunUsage JSON written before W2a Task 2 (no baseline_cost_usd key)
        // must still deserialize — the field defaults to 0.0 via #[serde(default)].
        let json = r#"{"prompt_tokens":1,"completion_tokens":1,"cost_usd":0.5}"#;
        let u: RunUsage = serde_json::from_str(json).expect("legacy RunUsage must deserialize");
        assert_eq!(u.cost_usd, 0.5);
        assert_eq!(u.baseline_cost_usd, 0.0);
    }
}
