//! Pure agent turn loop, stop policy, and wire events.

use super::*;
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
    pub(super) fn event_name(&self) -> &'static str {
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
    pub(super) fn to_sse(&self) -> axum::response::sse::Event {
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
pub(super) fn is_mechanical_continuation(messages: &[tt_shared::messages::Message]) -> bool {
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
pub(super) fn eligible_tool_ordinals(
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
pub(super) fn token_true_ok(
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
pub(super) fn sample_key(trace_id: Uuid, tool_call_id: &str) -> Uuid {
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
pub(super) fn latest_user_text(messages: &[Message]) -> String {
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
pub(super) async fn run_loop_core_with_output_cap(
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
                let budget_exhausted = matches!(
                    &e,
                    ApiError::Provider(tt_shared::ProviderError::BudgetExceeded { .. })
                );
                return LoopOutcome::Terminal(Run {
                    id,
                    status: if budget_exhausted {
                        RunStatus::Incomplete
                    } else {
                        RunStatus::Failed
                    },
                    messages,
                    turns: if budget_exhausted { turn } else { turn + 1 },
                    usage,
                    note: Some(format!("turn {turn} failed: {e}")),
                    summarizer_tax_usd: summarizer_tax,
                    stop_reason: budget_exhausted.then_some(StopReason::BudgetExhausted),
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

        // A started turn has now settled its actual served cost. The directional
        // pre-dispatch estimate only admitted the turn on the way in
        // (`accrued + est <= cap`); reality can settle above that. Once the
        // accumulated cost crosses the cap, terminate as a RECORDED breach at
        // the moment of settlement (fail closed) — never continue into another
        // turn and never return `Completed` on a breaching final turn — and
        // never silently clamp `usage.cost_usd` to the cap. The run carries
        // `StopReason::BudgetBreach` + the real settled cost so the breach is
        // persisted (`budget_breach`) and reconcileable. Settling exactly AT
        // the cap is not a breach (matches `would_exceed`'s equal-is-admissible
        // boundary).
        if max_cost_usd.is_some_and(|cap| usage.cost_usd > cap) {
            let cap = max_cost_usd.unwrap_or_default();
            let settled_cost_usd = usage.cost_usd;
            return LoopOutcome::Terminal(Run {
                id,
                status: RunStatus::Incomplete,
                messages,
                turns: turn + 1,
                usage,
                note: Some(format!(
                    "run cost cap ${cap:.4} breached: a started provider call settled ${settled_cost_usd:.4}, above the directional estimate"
                )),
                summarizer_tax_usd: summarizer_tax,
                stop_reason: Some(StopReason::BudgetBreach),
            });
        }

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
