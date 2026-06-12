//! Cache-aware compaction for `tt chat` (research Phase 4): fold older turns
//! into a FROZEN running summary that sits at the STABLE-PREFIX position
//! (right after the system prompt), every K successful turns — not per turn.
//!
//! The invariants this module enforces (caching-tension rule, honesty guard,
//! stable→volatile order, fallback trimmer) are documented centrally in
//! [`chat::budget`](super::budget)'s module docs; in short:
//!
//! - Between compactions the summary block is BYTE-FROZEN: [`FrozenSummary`]
//!   has no mutator — its only constructor is
//!   [`FrozenSummary::replace_at_compaction`], called from exactly one place
//!   (the success arm of [`compact_now`]). Nothing CAN regenerate or reorder
//!   the cached prefix between compactions.
//! - A compaction busts the provider-cached prefix once per K by design; the
//!   bust is booked as the negative cache-credit `B` in the pre-call
//!   net-positive predicate, and the transaction proceeds only when
//!   `(D − S) × K > C_in + S + B` (all estimated tokens). Otherwise it skips
//!   with an honest warning and makes ZERO gateway calls.
//! - The summary call is REAL SPEND, metered via [`Ledger::add_compaction`]
//!   into the headline `cost_usd`; the future re-send reduction is an
//!   ESTIMATE, labeled `est./unbooked`, never merged into `saved_usd`.
//! - A failed/empty/oversized summary call is a strict no-op on history: the
//!   conversation is not touched until the new block has been validated.
//!
//! Compaction is OPT-IN (`--compact` / `/compact on`) because the summary is
//! a paid model call. The 95% [`trim_to_budget`](super::budget::trim_to_budget)
//! safety valve stays active either way and structurally cannot touch the
//! summary (it drains `conv.messages` only).

use serde::{Deserialize, Serialize};

use tt_shared::messages::{Message, MessageContent};

use super::budget::{self, ContextState, ESTIMATE_PROVIDER};
use super::command::CompactArg;
use super::{Conversation, Ledger};
use crate::ui;

/// Default compaction cadence: compact every K = 8 successful turns.
pub const DEFAULT_COMPACT_EVERY: u32 = 8;
/// Minimum cadence. K = 1 would re-summarize (and bust the provider-cached
/// prefix) EVERY turn — the naive per-turn re-selection pattern that measured
/// −145% at 40 turns in the research. Rejected at the flag and `/compact every`
/// level by [`CompactionState::set_every`].
pub const MIN_COMPACT_EVERY: u32 = 2;
/// Most recent whole turns always kept verbatim (the volatile tail). The last
/// turn therefore can never fold — preserving the trimmer's "preserve the last
/// turn" spirit.
pub const KEEP_RECENT_TURNS: usize = 4;
/// Hard cap on the summary call's output tokens (the effective cap is
/// `min(D/4, this)` where D = estimated folded tokens).
pub const SUMMARY_MAX_TOKENS: u32 = 1024;
/// Default (cheap) model for the summary call.
pub const DEFAULT_COMPACT_MODEL: &str = "gpt-4o-mini";

/// First line of every frozen summary block, composed into the stored bytes.
const SUMMARY_HEADER: &str = "[Conversation summary — earlier turns were compacted by tt chat]";

/// Versioned summarizer instruction (v1 — bump the tag when the wording
/// changes: the block it produces is byte-frozen into sessions, so prompt
/// drift must stay observable).
const SUMMARIZER_PROMPT: &str = "You are compacting the history of an ongoing chat session \
(tt-chat-compact v1). Write a dense, factual summary of the conversation turns you are given. \
Preserve every fact, decision, constraint, number, file or model name, open question, and tool \
result the assistant may need to continue the conversation. If a previous summary is provided, \
fold it in — your output replaces it entirely. Output plain text only, with no preamble and no \
commentary.";

/// The frozen summary block carried at the stable-prefix position.
///
/// CACHING-TENSION RULE, expressed in the type system: `block` is private,
/// read-only via [`wire_block`](Self::wire_block), and the ONLY constructor is
/// [`replace_at_compaction`](Self::replace_at_compaction) — called from exactly
/// one place. Between compactions no code path can regenerate or reorder the
/// block, so the provider cache over `[system + summary + older-kept-verbatim]`
/// stays warm. All fields are serde-defaulted for forward/backward session
/// compatibility (old files load as `None` at the `Conversation` level; old
/// binaries ignore the field).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrozenSummary {
    /// Fully composed wire text (header line + summary), stored verbatim.
    #[serde(default)]
    block: String,
    /// Cumulative whole turns folded into this block across compactions.
    #[serde(default)]
    folded_turns: u32,
    /// Model that produced the latest block.
    #[serde(default)]
    model: String,
}

impl FrozenSummary {
    /// The single construction site for a summary block — only the compaction
    /// transaction ([`compact_now`]) may call this, and only after the new
    /// block has been validated. There is deliberately NO setter.
    pub(crate) fn replace_at_compaction(block: String, folded_turns: u32, model: String) -> Self {
        Self {
            block,
            folded_turns,
            model,
        }
    }
    /// The exact bytes sent on the wire (and counted by the estimator).
    #[must_use]
    pub fn wire_block(&self) -> &str {
        &self.block
    }
    /// Cumulative whole turns folded into this block.
    #[must_use]
    pub fn folded_turns(&self) -> u32 {
        self.folded_turns
    }
}

/// Runtime compaction config + cadence counter. The counter is RUNTIME-ONLY
/// (not persisted): resume/clear resets it, so the first compaction fires K
/// turns after a resume.
pub struct CompactionState {
    /// Compaction on/off (`--compact` / `/compact on|off`). OFF by default —
    /// the summary is a paid model call.
    pub enabled: bool,
    /// Model for the summary call (`--compact-model` / `/compact model`).
    pub model: String,
    /// K: compact every K successful turns. Private — writes go through
    /// [`set_every`](Self::set_every), which rejects K < [`MIN_COMPACT_EVERY`].
    every: u32,
    /// Successful turns since the last compaction (or skip).
    turns_since: u32,
}

impl CompactionState {
    #[must_use]
    pub fn new(enabled: bool, model: Option<String>) -> Self {
        Self {
            enabled,
            model: model.unwrap_or_else(|| DEFAULT_COMPACT_MODEL.to_string()),
            every: DEFAULT_COMPACT_EVERY,
            turns_since: 0,
        }
    }

    /// The configured cadence K. Also the REMAINING-TURNS HEURISTIC of the
    /// net-positive predicate: assume the session lasts at least one more
    /// compaction window (K more turns), so dropped tokens are saved ~K times.
    #[must_use]
    pub fn every(&self) -> u32 {
        self.every
    }

    /// Set the cadence. K < 2 is REJECTED — the error quotes the
    /// caching-tension rule (per-turn prefix busting is the −145% pattern).
    pub fn set_every(&mut self, k: u32) -> Result<(), String> {
        if k < MIN_COMPACT_EVERY {
            return Err(format!(
                "compact every {k} rejected: K < {MIN_COMPACT_EVERY} would re-summarize (and bust \
                 the provider-cached prefix) every turn — naive per-turn re-selection measured \
                 −145% at 40 turns; keeping every {}",
                self.every
            ));
        }
        self.every = k;
        Ok(())
    }

    /// Count one SUCCESSFUL dispatched turn (`/retry` does not count).
    pub fn note_turn(&mut self) {
        self.turns_since = self.turns_since.saturating_add(1);
    }

    /// Reset the cadence counter (after a compaction, skip, or failure — so a
    /// net-negative skip is not retried every turn).
    pub fn reset_cadence(&mut self) {
        self.turns_since = 0;
    }

    /// Compaction is due ⇔ enabled, K turns have elapsed, and there is more
    /// history than the verbatim-kept tail.
    #[must_use]
    pub fn due(&self, conv: &Conversation) -> bool {
        self.enabled
            && self.turns_since >= self.every
            && budget::turn_starts(&conv.messages).len() > KEEP_RECENT_TURNS
    }
}

/// Outcome of one compaction attempt (the event/skip lines are printed by
/// [`compact_now`] itself; callers and tests branch on this).
#[derive(Debug, PartialEq, Eq)]
pub enum CompactOutcome {
    /// History folded into a new frozen block.
    Compacted,
    /// Pre-call predicate said net-negative — zero gateway calls were made.
    SkippedNetNegative,
    /// Nothing beyond the verbatim-kept tail to fold.
    NothingToFold,
    /// The summary call failed (transport / non-2xx) — history unchanged.
    Failed,
    /// The summary came back empty or no smaller than the folded turns —
    /// history unchanged (spend still metered).
    Discarded,
}

/// The pre-call plan: what would fold, and the token-denominated predicate
/// terms (research Phase 4): D = folded verbatim tokens, S = summary cap,
/// `spend = C_in + S + B`, `save = (D − S) × K`. `B` books the one-off
/// cache bust as a full-rate re-write of the new stable prefix (conservative).
struct CompactionPlan {
    /// Messages (not turns) to drain from the front of `conv.messages`.
    fold_len: usize,
    /// Whole turns folded by this compaction.
    fold_turns: u32,
    /// D: estimated tokens of the folded verbatim transcript.
    dropped_tokens: u32,
    /// S: output-token cap for the summary call, `min(D/4, SUMMARY_MAX_TOKENS)`.
    summary_cap: u32,
    /// `C_in + S + B` — what the compaction is estimated to cost, in tokens.
    spend_tokens: u64,
    /// `(D − S) × K` — what it is estimated to save over the heuristic window.
    save_tokens: u64,
    /// The folded turns rendered as a transcript for the summarizer.
    transcript: String,
}

impl CompactionPlan {
    fn net_positive(&self) -> bool {
        self.save_tokens > self.spend_tokens
    }
}

/// Render messages as a plain transcript for the summarizer / estimator.
fn transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            Message::System {
                content: MessageContent::Text(t),
            } => {
                out.push_str("system: ");
                out.push_str(t);
                out.push('\n');
            }
            Message::User {
                content: MessageContent::Text(t),
                ..
            } => {
                out.push_str("user: ");
                out.push_str(t);
                out.push('\n');
            }
            Message::Tool {
                content: MessageContent::Text(t),
                ..
            } => {
                out.push_str("tool: ");
                out.push_str(t);
                out.push('\n');
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if let Some(MessageContent::Text(t)) = content {
                    out.push_str("assistant: ");
                    out.push_str(t);
                    out.push('\n');
                }
                for tc in tool_calls {
                    out.push_str("assistant→tool ");
                    out.push_str(&tc.function.name);
                    out.push('(');
                    out.push_str(&tc.function.arguments);
                    out.push_str(")\n");
                }
            }
            _ => {}
        }
    }
    out
}

/// Build the compaction plan for `conv`, or `None` when there is nothing
/// beyond the verbatim-kept tail. Folding happens on the SAME whole-turn
/// boundaries as the trimmer ([`budget::turn_starts`]), so the compactor can
/// never orphan an `Assistant{tool_calls}` + `Tool` pair either.
fn build_plan(conv: &Conversation, every: u32) -> Option<CompactionPlan> {
    let starts = budget::turn_starts(&conv.messages);
    if starts.len() <= KEEP_RECENT_TURNS {
        return None;
    }
    let fold_len = starts[starts.len() - KEEP_RECENT_TURNS];
    let fold_turns = (starts.len() - KEEP_RECENT_TURNS) as u32;
    let est = |s: &str| tt_tokenize::estimate_tokens(ESTIMATE_PROVIDER, s);

    let folded = transcript(&conv.messages[..fold_len]);
    let dropped_tokens = est(&folded); // D
    let summary_cap = (dropped_tokens / 4).clamp(1, SUMMARY_MAX_TOKENS); // S
    let prior_block = conv.summary.as_ref().map_or("", FrozenSummary::wire_block);
    // C_in: the summary call's input (summarizer prompt + prior block + fold).
    let call_input =
        u64::from(est(SUMMARIZER_PROMPT)) + u64::from(est(prior_block)) + u64::from(dropped_tokens);
    // B: the NEGATIVE cache-credit — one full-rate re-write of the new stable
    // prefix [system + summary + kept-verbatim] next turn, charged at 100%.
    let kept = transcript(&conv.messages[fold_len..]);
    let new_prefix = u64::from(conv.system.as_deref().map_or(0, est))
        + u64::from(summary_cap)
        + u64::from(est(&kept));
    let spend_tokens = call_input + u64::from(summary_cap) + new_prefix;
    let save_tokens = u64::from(dropped_tokens.saturating_sub(summary_cap)) * u64::from(every);
    Some(CompactionPlan {
        fold_len,
        fold_turns,
        dropped_tokens,
        summary_cap,
        spend_tokens,
        save_tokens,
        transcript: folded,
    })
}

/// Compact after a successful turn, when due. The ONLY automatic call site —
/// invoked AFTER `dispatch_turn` returns true (never between the caller's
/// snapshot and dispatch, so the existing snapshot/restore contract is
/// untouched and `/retry` neither counts a turn nor compacts).
pub async fn maybe_compact(
    client: &tt_client::Client,
    conv: &mut Conversation,
    cstate: &mut CompactionState,
    ctx: &mut ContextState,
    ledger: &mut Ledger,
) {
    if cstate.due(conv) {
        let _ = compact_now(client, conv, cstate, ctx, ledger).await;
    }
}

/// The compaction transaction (also `/compact now`). Obeys the net-positive
/// guard, meters real spend, and is a strict no-op on history unless a
/// validated new block lands: `conv` is not mutated until the summary has
/// been received, is non-empty, and actually compresses — so the
/// snapshot/restore contract holds by construction.
pub async fn compact_now(
    client: &tt_client::Client,
    conv: &mut Conversation,
    cstate: &mut CompactionState,
    ctx: &mut ContextState,
    ledger: &mut Ledger,
) -> CompactOutcome {
    // NothingToFold intentionally does NOT reset the cadence counter: the
    // automatic path can't reach it (`due()` already requires more history
    // than the kept tail), so it only fires on a manual `/compact now` over a
    // short conversation — where the next `due()` check should not be pushed
    // back by K turns.
    let Some(plan) = build_plan(conv, cstate.every()) else {
        ui::info(&format!(
            "nothing to compact — the most recent {KEEP_RECENT_TURNS} turn(s) are kept verbatim"
        ));
        return CompactOutcome::NothingToFold;
    };
    // Whatever happens next, don't re-attempt every turn.
    cstate.reset_cadence();

    if !plan.net_positive() {
        ui::warn(&format!(
            "compaction skipped: est. net-negative (≈{} tok to save ≈{} tok over ~{} turns)",
            plan.spend_tokens,
            plan.save_tokens,
            cstate.every()
        ));
        return CompactOutcome::SkippedNetNegative;
    }

    let mut input = String::new();
    if let Some(prior) = &conv.summary {
        input.push_str("Previous summary (your output replaces it — fold it in):\n");
        input.push_str(prior.wire_block());
        input.push_str("\n\n");
    }
    input.push_str("Conversation turns to fold into the summary:\n");
    input.push_str(&plan.transcript);

    // A human is in the REPL → interactive (batch-ineligible in code, like
    // every other CLI call); tagged for attribution in telemetry.
    let result = client
        .chat()
        .model(&cstate.model)
        .messages(vec![
            tt_client::system(SUMMARIZER_PROMPT),
            tt_client::user(input),
        ])
        .max_tokens(plan.summary_cap)
        .interactive()
        .tag("chat-compact")
        .send()
        .await;

    let out = match result {
        Ok(o) => o,
        Err(e) => {
            // Meter any REAL SPEND the gateway reported on the failed call.
            if let tt_client::Error::Status { cost, .. } = &e {
                if let Some(c) = cost.cost_usd {
                    ledger.add_compaction(c);
                }
            }
            ui::warn(&format!("compaction failed (history unchanged): {e}"));
            return CompactOutcome::Failed;
        }
    };
    // The call happened: meter it as real spend even if we end up discarding.
    ledger.add_compaction(out.cost.cost_usd.unwrap_or(0.0));

    let text = out.text().map(str::trim).unwrap_or("").to_string();
    if text.is_empty() {
        ui::warn("compaction discarded: the summary model returned no text (history unchanged)");
        return CompactOutcome::Discarded;
    }
    let block = format!("{SUMMARY_HEADER}\n{text}");
    let summary_tokens = tt_tokenize::estimate_tokens(ESTIMATE_PROVIDER, &block);
    if summary_tokens >= plan.dropped_tokens {
        ui::warn(&format!(
            "compaction discarded: summary (~{summary_tokens} tok) is no smaller than the \
             folded turns (~{} tok) — history unchanged",
            plan.dropped_tokens
        ));
        return CompactOutcome::Discarded;
    }

    // Validated — commit the transaction. This is the one construction site
    // of a FrozenSummary; the new prefix is byte-frozen until the next due().
    let folded_total =
        conv.summary.as_ref().map_or(0, FrozenSummary::folded_turns) + plan.fold_turns;
    let model_used = out
        .cost
        .model_used
        .clone()
        .unwrap_or_else(|| cstate.model.clone());
    conv.summary = Some(FrozenSummary::replace_at_compaction(
        block,
        folded_total,
        model_used,
    ));
    conv.messages.drain(..plan.fold_len);
    ctx.reset_warned();

    // Books (D − S_new) per compaction. On a RE-compaction the new block also
    // replaces the prior summary on the wire, so the true cumulative per-turn
    // reduction is larger by the replaced summary's size — i.e. this estimate
    // errs LOW (conservative, the right direction for an unbooked figure).
    let est_delta = u64::from(plan.dropped_tokens - summary_tokens);
    ledger.compaction_est_tok_per_turn += est_delta;
    let cost = out.cost.cost_usd.unwrap_or(0.0);
    ui::info(&format!(
        "compacted {} turn(s) → summary ~{summary_tokens} tok · call cost ${cost:.4} · \
         est. −{est_delta} tok re-sent/turn going forward (estimate, not booked; provider \
         cache re-warms next turn)",
        plan.fold_turns
    ));
    CompactOutcome::Compacted
}

/// Handle a `/compact` REPL command (status / on/off / cadence / model / now).
pub async fn handle(
    arg: CompactArg,
    client: &tt_client::Client,
    conv: &mut Conversation,
    cstate: &mut CompactionState,
    ctx: &mut ContextState,
    ledger: &mut Ledger,
) {
    match arg {
        CompactArg::Status => print_status(cstate, conv),
        CompactArg::Set(b) => {
            cstate.enabled = b;
            print_status(cstate, conv);
        }
        CompactArg::Every(k) => match cstate.set_every(k) {
            Ok(()) => ui::info(&format!("compact cadence → every {k} turn(s)")),
            Err(msg) => ui::warn(&msg),
        },
        CompactArg::Model(m) => {
            cstate.model = m;
            ui::info(&format!("compact model → {}", cstate.model));
        }
        CompactArg::Now => {
            let _ = compact_now(client, conv, cstate, ctx, ledger).await;
        }
        CompactArg::Bad(usage) => ui::warn(&usage),
    }
}

/// One-line `/compact` status.
pub fn print_status(cstate: &CompactionState, conv: &Conversation) {
    let state = if cstate.enabled { "on" } else { "off" };
    let mut s = format!(
        "compact: {state} · every {} turn(s) · model {}",
        cstate.every(),
        cstate.model
    );
    match &conv.summary {
        Some(sum) => s.push_str(&format!(
            " · summary ~{} tok ({} turn(s) folded)",
            tt_tokenize::estimate_tokens(ESTIMATE_PROVIDER, sum.wire_block()),
            sum.folded_turns()
        )),
        None => s.push_str(" · no summary yet"),
    }
    ui::info(&s);
}

#[cfg(test)]
mod tests;
