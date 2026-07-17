//! Pure cost-budget helpers for the server-side agent run loop (W0a).
//!
//! Kept out of the oversized `agent_run.rs` (ADR-011). All functions are pure
//! and unit-tested here; the loop in `agent_run.rs` calls them.
//!
use tt_shared::messages::{Message, MessageContent, ToolCall};

/// A non-success terminal cause for a run, recorded on `Run.stop_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The loop hit `max_turns`.
    MaxTurns,
    /// The run's accumulated served cost reached `max_cost_usd`.
    BudgetExhausted,
    /// The loop made no progress — `RUNAWAY_REPEAT_THRESHOLD` consecutive
    /// byte-identical (tool-call + tool-result) steps. The model saw the same
    /// result and re-issued the same call with no new information; left alone it
    /// would burn the budget turn after turn (the *fastest* way an agent loop
    /// leaks money — it trips well before a static cost cap).
    Runaway,
}

/// Consecutive byte-identical (tool-call + tool-result) steps that mark a
/// gateway-only agent loop as a runaway. Three tolerates one accidental repeat:
/// the 3rd identical step in a row means the model saw the same result twice and
/// still emitted the same call, so it has no new information to act on. The run
/// ends `Incomplete` with [`StopReason::Runaway`]; the partial transcript is
/// preserved (never `Failed`).
pub(crate) const RUNAWAY_REPEAT_THRESHOLD: u32 = 3;

/// A deterministic fingerprint of one loop step's ACTION + OUTCOME: the
/// assistant's tool calls (name + arguments, in order) plus the gateway tool
/// results they produced (in order). The per-call `id` and the assistant's prose
/// content are excluded on purpose — only whether the agent *did the same thing
/// and got the same answer* matters for no-progress detection. A loop that polls
/// a genuinely changing result keeps producing fresh signatures and is never
/// flagged.
pub(crate) fn step_signature(tool_calls: &[ToolCall], tool_results: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for tc in tool_calls {
        tc.function.name.hash(&mut h);
        tc.function.arguments.hash(&mut h);
    }
    // Domain separator so a call's bytes can't alias a result's across the join.
    0xff_u8.hash(&mut h);
    for r in tool_results {
        r.hash(&mut h);
    }
    h.finish()
}

/// Tracks consecutive identical step signatures across loop turns. [`record`]
/// returns `true` once `threshold` identical steps have been seen in a row.
///
/// [`record`]: NoProgressTracker::record
pub(crate) struct NoProgressTracker {
    last: Option<u64>,
    streak: u32,
    threshold: u32,
}

impl NoProgressTracker {
    pub(crate) fn new(threshold: u32) -> Self {
        Self {
            last: None,
            streak: 0,
            threshold,
        }
    }

    /// Feed the current step's signature; returns `true` iff this is the
    /// `threshold`-th identical step in an unbroken run (a runaway). Any
    /// different signature resets the streak to 1.
    pub(crate) fn record(&mut self, sig: u64) -> bool {
        if self.last == Some(sig) {
            self.streak += 1;
        } else {
            self.last = Some(sig);
            self.streak = 1;
        }
        self.streak >= self.threshold
    }
}

/// True iff the accrued cost has reached the cap (no estimate — pure threshold check).
///
/// Used by the workflow engine before each intelligence node.  Returns `false`
/// when no cap is set.
pub(crate) fn budget_reached(accrued_usd: f64, cap: Option<f64>) -> bool {
    would_exceed(accrued_usd, None, cap)
}

/// True iff a cap is set and `accrued + best-effort next-turn estimate` reaches
/// it. When no estimate is available (`est_next_usd == None`), this reduces to the pure `accrued >= cap` check.
pub(crate) fn would_exceed(accrued_usd: f64, est_next_usd: Option<f64>, cap: Option<f64>) -> bool {
    match cap {
        None => false,
        Some(c) => accrued_usd + est_next_usd.unwrap_or(0.0) >= c,
    }
}

/// Best-effort projection of the NEXT turn's served cost, via the pure
/// `tt-preview` projector. Returns `None` when the model is not in any catalog
/// (e.g. local/$0 models, or test models) so callers fall back to the accrued
/// check. The estimate is directional (token heuristics + catalog pricing), so
/// it tightens the cap but is not the hard guarantee.
pub(crate) fn estimate_next_turn_cost(
    model: &str,
    messages: &[Message],
    max_output_tokens: Option<u32>,
) -> Option<f64> {
    let preview_messages: Vec<tt_preview::types::Message> = messages
        .iter()
        .map(|m| {
            let (role, content) = message_role_and_text(m);
            tt_preview::types::Message {
                role: role.to_string(),
                content: serde_json::Value::String(content),
            }
        })
        .collect();
    let req = tt_preview::PreviewRequest {
        model: model.to_string(),
        messages: preview_messages,
        // `tt-preview` treats this as an explicit completion ceiling and
        // clamps it to the catalogued model maximum.  Workflow node caps map
        // to the same `ChatCompletionRequest::max_tokens` field at dispatch.
        max_tokens: max_output_tokens,
        tools: None,
        stream: None,
        tt_extras: std::collections::HashMap::new(),
    };
    tt_preview::preview(&req).ok().map(|r| r.current.cost_usd)
}

/// Flatten a transcript message into (role, text) for cost estimation. Only the
/// text is needed for token counting; tool-call structure is ignored.
fn message_role_and_text(m: &Message) -> (&'static str, String) {
    match m {
        Message::System { content } => ("system", content_text(content)),
        Message::User { content, .. } => ("user", content_text(content)),
        Message::Assistant { content, .. } => (
            "assistant",
            content.as_ref().map(content_text).unwrap_or_default(),
        ),
        Message::Tool { content, .. } => ("tool", content_text(content)),
    }
}

fn content_text(c: &MessageContent) -> String {
    match c {
        MessageContent::Text(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn would_exceed_uses_accrued_plus_estimate_when_estimate_present() {
        // accrued 0.30 + est 0.15 = 0.45 >= cap 0.40 -> would exceed
        assert!(would_exceed(0.30, Some(0.15), Some(0.40)));
        // accrued 0.30 + est 0.05 = 0.35 < cap 0.40 -> would not
        assert!(!would_exceed(0.30, Some(0.05), Some(0.40)));
    }

    #[test]
    fn would_exceed_falls_back_to_accrued_when_estimate_absent() {
        // no estimate -> reduces to the pure accrued >= cap check
        assert!(would_exceed(0.40, None, Some(0.40)));
        assert!(!would_exceed(0.39, None, Some(0.40)));
    }

    #[test]
    fn would_exceed_is_false_without_a_cap() {
        assert!(!would_exceed(999.0, Some(999.0), None));
    }

    #[test]
    fn estimate_unknown_model_is_none() {
        // The agent-loop test model "m" is not in any pricing catalog.
        assert_eq!(estimate_next_turn_cost("m", &[], None), None);
    }

    fn tc(name: &str, args: &str, id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            r#type: "function".into(),
            function: tt_shared::messages::ToolCallFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    #[test]
    fn step_signature_ignores_call_id_and_keys_on_action_and_outcome() {
        let a = step_signature(&[tc("find_route_for", "{\"q\":1}", "c1")], &["ok".into()]);
        // Same name+args+result, DIFFERENT call id → same signature.
        let b = step_signature(&[tc("find_route_for", "{\"q\":1}", "c2")], &["ok".into()]);
        assert_eq!(a, b, "call id must not affect the signature");
        // Different arguments → different signature.
        let c = step_signature(&[tc("find_route_for", "{\"q\":2}", "c1")], &["ok".into()]);
        assert_ne!(a, c, "different args must change the signature");
        // Same call, DIFFERENT result (e.g. a poll that advanced) → different.
        let d = step_signature(&[tc("find_route_for", "{\"q\":1}", "c1")], &["done".into()]);
        assert_ne!(a, d, "a changed result must change the signature");
    }

    #[test]
    fn no_progress_trips_only_after_threshold_identical_steps() {
        let mut t = NoProgressTracker::new(RUNAWAY_REPEAT_THRESHOLD); // 3
        assert!(!t.record(1)); // streak 1
        assert!(!t.record(1)); // streak 2
        assert!(t.record(1)); //  streak 3 → runaway
    }

    #[test]
    fn no_progress_resets_on_any_change() {
        let mut t = NoProgressTracker::new(3);
        assert!(!t.record(1));
        assert!(!t.record(1)); // streak 2
        assert!(!t.record(2)); // different → reset to 1
        assert!(!t.record(2)); // streak 2
        assert!(!t.record(1)); // different → reset to 1
                               // An alternating A,B,A,B,… loop never trips.
        assert!(!t.record(2));
        assert!(!t.record(1));
    }

    #[test]
    fn estimate_known_model_is_some_positive() {
        let msgs = vec![tt_shared::messages::Message::User {
            content: tt_shared::messages::MessageContent::Text("hello world".into()),
            name: None,
        }];
        let est = estimate_next_turn_cost("gpt-4o-mini", &msgs, None);
        assert!(
            est.is_some(),
            "gpt-4o-mini should resolve in the bundled catalog"
        );
        assert!(est.unwrap() > 0.0);
    }
}
