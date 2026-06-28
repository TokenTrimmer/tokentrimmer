//! Pure cost-budget helpers for the server-side agent run loop (W0a).
//!
//! Kept out of the oversized `agent_run.rs` (ADR-011). All functions are pure
//! and unit-tested here; the loop in `agent_run.rs` calls them.
//!
use tt_shared::messages::{Message, MessageContent};

/// A non-success terminal cause for a run, recorded on `Run.stop_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The loop hit `max_turns`.
    MaxTurns,
    /// The run's accumulated served cost reached `max_cost_usd`.
    BudgetExhausted,
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
pub(crate) fn estimate_next_turn_cost(model: &str, messages: &[Message]) -> Option<f64> {
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
        max_tokens: None,
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
        assert_eq!(estimate_next_turn_cost("m", &[]), None);
    }

    #[test]
    fn estimate_known_model_is_some_positive() {
        let msgs = vec![tt_shared::messages::Message::User {
            content: tt_shared::messages::MessageContent::Text("hello world".into()),
            name: None,
        }];
        let est = estimate_next_turn_cost("gpt-4o-mini", &msgs);
        assert!(
            est.is_some(),
            "gpt-4o-mini should resolve in the bundled catalog"
        );
        assert!(est.unwrap() > 0.0);
    }
}
