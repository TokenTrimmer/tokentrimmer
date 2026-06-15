//! Sub-lever 2a (token-moving half) — **drop SUPERSEDED stale tool results**
//! (token-true gated, NO judge).
//!
//! This is the concrete, *safe* answer to COST-4: a transform that actually
//! moves billed input tokens on the dominant agentic shape, where the sibling
//! whitespace [`CompressionPass`](crate::passes::compression::CompressionPass)
//! moves ~0 under BPE.
//!
//! # The signal
//!
//! Inside an agentic loop the model frequently **re-issues an identical tool
//! call** (same tool name + byte-identical, canonicalized arguments) to get a
//! fresh result — a re-read file, a re-run query. When it does, the EARLIER
//! result for that call is, by the model's own behavior, no longer the
//! authoritative one: the later identical call supersedes it. The whole stale
//! result payload (often a large JSON blob) is dead weight re-sent on every
//! subsequent turn.
//!
//! # What it does
//!
//! For each `role:"tool"` block whose call identity `(name, canonical_args)` is
//! re-issued by a LATER tool block, this replaces the earlier (superseded)
//! result's content with a compact [`SUPERSEDED_STUB`] marker. It **keeps the
//! message** (the `tool_call_id` block stays present, so the assistant turn that
//! emitted the call still has its matching tool response — the request stays
//! structurally valid) and only swaps the body. That moves the stale payload's
//! tokens out while leaving an explicit, auditable "this was superseded" trail.
//!
//! # Why it is safe WITHOUT a judge (it rides the token-true gate)
//!
//! - **Cache-stable prefix is unreachable by type** — it operates on the
//!   [`VolatileTail`](crate::passes::VolatileTail) only, so it can never bust the
//!   provider prompt cache (the #1 forbidden failure mode).
//! - **`keep_recent_pairs` blast-radius bound** — the last `keep_recent_pairs`
//!   tool blocks are kept verbatim, exactly like
//!   [`ElidePass`](super::elide::ElidePass): the load-bearing recent context the
//!   agent is actively reasoning over is never touched.
//! - **Identity match is exact** — it only supersedes when a LATER call repeats
//!   the same tool with byte-identical canonical arguments. A differing call is
//!   never treated as a supersession (false negatives are safe; we simply do
//!   nothing).
//! - **Net-positive only** — the stub replaces a block only when it strictly
//!   shrinks it, and the pipeline's token-true gate
//!   ([`PassPipeline::run`](crate::passes::PassPipeline::run)) is the final
//!   authority: if the edit ever failed to reduce the tokenizer count it is
//!   rolled back byte-identical and books zero (the `InflatingPass` contract).
//! - **Deterministic** — a pure function of the request (no RNG, no clock), so
//!   it never perturbs an attested/golden figure for an un-opted request.
//!
//! # Wiring
//!
//! It composes after [`ElidePass`](super::elide::ElidePass) in the planner's
//! field-drop pipeline ([`AgenticBudgetPlanner::run_field_drop`]), so it runs
//! wherever Sub-lever 2a runs and its delta accrues to the same
//! `elide_field_drop_tokens_removed` bucket (both are lossless tail reductions).

use std::collections::HashSet;

use serde_json::Value;
use tt_shared::messages::{Message, MessageContent};

use crate::passes::{PassContext, PassOutcome, RequestPass, StablePrefix, VolatileTail};

/// The compact marker left in place of a superseded tool result. It is valid
/// JSON (so a downstream JSON-expecting reader does not choke), tiny, and
/// deterministic. The original `tool_call_id` block is preserved; only the
/// content is swapped, so the request stays structurally valid.
pub const SUPERSEDED_STUB: &str = r#"{"tt_superseded":true}"#;

/// Drop superseded/stale tool results on the volatile tail (see the module docs).
pub struct SupersedePass {
    /// Keep the last N tool-result blocks verbatim — the load-bearing recent
    /// context is never superseded (mirrors [`ElidePass`](super::elide::ElidePass)
    /// and `AgenticBudget::keep_recent_pairs`).
    keep_recent_pairs: u32,
}

impl SupersedePass {
    /// A supersede pass keeping the last `keep_recent_pairs` tool-result blocks
    /// verbatim.
    #[must_use]
    pub fn new(keep_recent_pairs: u32) -> Self {
        Self { keep_recent_pairs }
    }
}

/// The identity of a tool call: `(tool_name, canonicalized_arguments)`. Two
/// calls supersede iff their identities are equal.
type CallIdentity = (String, String);

/// Canonicalize a tool-call arguments string for identity comparison: parse +
/// re-serialize compactly when it is JSON (normalizing insignificant
/// whitespace), else compare the raw string verbatim. Conservative — when in
/// doubt the identities simply differ and nothing is superseded.
fn canonical_args(args: &str) -> String {
    match serde_json::from_str::<Value>(args) {
        Ok(v) => v.to_string(),
        Err(_) => args.to_string(),
    }
}

/// Resolve the call identity of the tool result at `tail_idx` by matching its
/// `tool_call_id` to an assistant `tool_calls` entry in the tail. `None` when
/// no match is found (an unknown call we must never treat as superseded).
fn call_identity(msgs: &[Message], tail_idx: usize) -> Option<CallIdentity> {
    let Message::Tool { tool_call_id, .. } = &msgs[tail_idx] else {
        return None;
    };
    msgs.iter().find_map(|m| {
        let Message::Assistant { tool_calls, .. } = m else {
            return None;
        };
        tool_calls
            .iter()
            .find(|tc| tc.id == *tool_call_id)
            .map(|tc| {
                (
                    tc.function.name.clone(),
                    canonical_args(&tc.function.arguments),
                )
            })
    })
}

impl RequestPass for SupersedePass {
    fn name(&self) -> &'static str {
        "agentic-supersede-stale-tools"
    }

    fn apply(
        &self,
        _stable: &StablePrefix<'_>,
        tail: &mut VolatileTail<'_>,
        _cx: &PassContext<'_>,
    ) -> PassOutcome {
        // Resolve identities against a read-only snapshot before mutating, so a
        // content edit can never affect the (assistant-derived) identity lookup.
        let snapshot = tail.messages().to_vec();
        let tool_idxs: Vec<usize> = snapshot
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m, Message::Tool { .. }))
            .map(|(i, _)| i)
            .collect();

        // Keep the last `keep_recent_pairs` tool blocks verbatim (blast-radius
        // bound): only blocks before this cutoff are eligible for supersession.
        let keep = self.keep_recent_pairs as usize;
        let eligible_cutoff = tool_idxs.len().saturating_sub(keep);
        let eligible: HashSet<usize> = tool_idxs.iter().copied().take(eligible_cutoff).collect();

        // Identity per tool block, in `tool_idxs` order.
        let identities: Vec<Option<CallIdentity>> = tool_idxs
            .iter()
            .map(|&i| call_identity(&snapshot, i))
            .collect();

        let mut tokens_removed: u32 = 0;
        for (pos, &idx) in tool_idxs.iter().enumerate() {
            if !eligible.contains(&idx) {
                continue;
            }
            let Some(identity) = &identities[pos] else {
                continue; // unknown call → never superseded
            };
            // Superseded iff some LATER tool block repeats this exact identity.
            let superseded = identities
                .iter()
                .skip(pos + 1)
                .any(|later| later.as_ref() == Some(identity));
            if !superseded {
                continue;
            }
            let Message::Tool { content, .. } = &mut tail.messages_mut()[idx] else {
                continue;
            };
            // Only text content is stubbed; multimodal tool parts are left alone.
            let MessageContent::Text(text) = content else {
                continue;
            };
            // Net-positive only: never grow a block (the gate would reject it
            // anyway, but this keeps the self-report honest).
            if SUPERSEDED_STUB.len() < text.len() {
                tokens_removed =
                    tokens_removed.saturating_add((text.len() - SUPERSEDED_STUB.len()) as u32);
                *text = SUPERSEDED_STUB.to_string();
            }
        }

        PassOutcome {
            tokens_removed,
            warnings: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tt_shared::messages::{ContentPart, Message, MessageContent, ToolCall, ToolCallFunction};
    use tt_shared::ChatCompletionRequest;

    use crate::passes::{PassContext, PassPipeline, PipelineOutcome, SplitRequest};

    fn user(text: &str) -> Message {
        Message::User {
            content: MessageContent::Text(text.into()),
            name: None,
        }
    }
    fn assistant_call_args(call_id: &str, tool: &str, args: &str) -> Message {
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: tool.into(),
                    arguments: args.into(),
                },
            }],
            name: None,
        }
    }
    fn tool_result(call_id: &str, content: Value) -> Message {
        Message::Tool {
            content: MessageContent::Text(content.to_string()),
            tool_call_id: call_id.into(),
        }
    }
    fn req_with(messages: Vec<Message>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages,
            ..Default::default()
        }
    }
    /// All-volatile context (no pricing → no cache minimum → empty stable
    /// prefix): the whole message list is the volatile tail.
    fn cx() -> PassContext<'static> {
        PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        }
    }
    fn run(pass: SupersedePass, req: &mut ChatCompletionRequest) -> PipelineOutcome {
        let cx = cx();
        let pipe = PassPipeline::new().with(pass);
        let mut split = SplitRequest::compute(req, &cx);
        pipe.run(&mut split, &cx)
    }
    fn tool_text(req: &ChatCompletionRequest, idx: usize) -> String {
        let Message::Tool { content, .. } = &req.messages[idx] else {
            panic!("expected tool message at {idx}");
        };
        let MessageContent::Text(s) = content else {
            panic!("expected text content at {idx}");
        };
        s.clone()
    }

    /// A bulky result. Big enough that stubbing it strictly removes tokens.
    fn big_result(tag: &str) -> Value {
        json!({
            "tag": tag,
            "rows": (0..40).map(|i| format!("row-{i}-{tag}-some-verbose-payload")).collect::<Vec<_>>(),
            "note": "a verbose tool result that is re-fetched by an identical later call"
        })
    }

    /// The core token-moving proof: an earlier tool result for a call that is
    /// re-issued IDENTICALLY later is stubbed, removing tokens; the most-recent
    /// identical result is kept verbatim.
    #[test]
    fn supersedes_earlier_identical_call_and_removes_tokens() {
        let mut req = req_with(vec![
            user("read the config file"),
            assistant_call_args("c1", "read_file", r#"{"path":"/etc/app.conf"}"#),
            tool_result("c1", big_result("first")),
            user("re-read it, it changed"),
            assistant_call_args("c2", "read_file", r#"{"path":"/etc/app.conf"}"#),
            tool_result("c2", big_result("second")),
        ]);
        // keep_recent_pairs=1 → the latest tool block (c2) stays verbatim; the
        // earlier identical one (c1) is eligible and superseded.
        let out = run(SupersedePass::new(1), &mut req);

        assert_eq!(
            tool_text(&req, 2),
            SUPERSEDED_STUB,
            "stale c1 result stubbed"
        );
        assert!(
            tool_text(&req, 5).contains("second"),
            "the most-recent identical result is kept verbatim"
        );
        assert!(
            out.tokens_removed > 0,
            "the token-true gate must commit a positive delta"
        );
        assert!(out.rejected.is_empty(), "a real shrink is never rejected");
    }

    /// No-op when unsafe: distinct calls (different arguments) are never treated
    /// as supersessions — the request is byte-identical and books zero.
    #[test]
    fn distinct_calls_are_not_superseded() {
        let mut req = req_with(vec![
            user("read two files"),
            assistant_call_args("c1", "read_file", r#"{"path":"/a"}"#),
            tool_result("c1", big_result("a")),
            assistant_call_args("c2", "read_file", r#"{"path":"/b"}"#),
            tool_result("c2", big_result("b")),
        ]);
        let before = serde_json::to_string(&req).unwrap();
        let out = run(SupersedePass::new(0), &mut req);
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "distinct-argument calls must be left byte-identical"
        );
        assert_eq!(out.tokens_removed, 0);
    }

    /// No-op when the only identical results sit inside the keep-recent window.
    #[test]
    fn keep_recent_window_is_never_superseded() {
        let mut req = req_with(vec![
            user("loop"),
            assistant_call_args("c1", "ping", "{}"),
            tool_result("c1", big_result("r1")),
            assistant_call_args("c2", "ping", "{}"),
            tool_result("c2", big_result("r2")),
        ]);
        let before = serde_json::to_string(&req).unwrap();
        // keep_recent_pairs=2 keeps BOTH tool blocks verbatim → nothing eligible.
        let out = run(SupersedePass::new(2), &mut req);
        assert_eq!(serde_json::to_string(&req).unwrap(), before);
        assert_eq!(out.tokens_removed, 0);
    }

    /// A single occurrence of a call is never superseded (no later identical
    /// call exists).
    #[test]
    fn single_call_is_never_superseded() {
        let mut req = req_with(vec![
            user("once"),
            assistant_call_args("c1", "read_file", r#"{"path":"/a"}"#),
            tool_result("c1", big_result("only")),
        ]);
        let before = serde_json::to_string(&req).unwrap();
        let out = run(SupersedePass::new(0), &mut req);
        assert_eq!(serde_json::to_string(&req).unwrap(), before);
        assert_eq!(out.tokens_removed, 0);
    }

    /// Three identical calls: with keep_recent_pairs=1 the two OLDEST are
    /// superseded (both stubbed) and the newest is kept verbatim.
    #[test]
    fn supersedes_all_but_most_recent() {
        let args = r#"{"q":"select 1"}"#;
        let mut req = req_with(vec![
            user("run query repeatedly"),
            assistant_call_args("c1", "run_sql", args),
            tool_result("c1", big_result("q1")),
            assistant_call_args("c2", "run_sql", args),
            tool_result("c2", big_result("q2")),
            assistant_call_args("c3", "run_sql", args),
            tool_result("c3", big_result("q3")),
        ]);
        let out = run(SupersedePass::new(1), &mut req);
        assert_eq!(tool_text(&req, 2), SUPERSEDED_STUB, "c1 superseded");
        assert_eq!(tool_text(&req, 4), SUPERSEDED_STUB, "c2 superseded");
        assert!(tool_text(&req, 6).contains("q3"), "c3 (newest) verbatim");
        assert!(out.tokens_removed > 0);
    }

    /// Already-tiny stale results are not stubbed (the stub would not strictly
    /// shrink them) — a no-op, books zero.
    #[test]
    fn tiny_result_not_grown() {
        let mut req = req_with(vec![
            user("loop"),
            assistant_call_args("c1", "ping", "{}"),
            tool_result("c1", json!(1)),
            assistant_call_args("c2", "ping", "{}"),
            tool_result("c2", json!(2)),
        ]);
        let before = serde_json::to_string(&req).unwrap();
        let out = run(SupersedePass::new(1), &mut req);
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "a stub no larger than the result must not be applied"
        );
        assert_eq!(out.tokens_removed, 0);
    }

    /// Multimodal (Parts) tool content is left untouched (only text is stubbed).
    #[test]
    fn multimodal_tool_content_untouched() {
        let mut req = req_with(vec![
            user("loop"),
            assistant_call_args("c1", "render", "{}"),
            Message::Tool {
                content: MessageContent::Parts(vec![ContentPart::Text {
                    text: "a verbose textual part that would otherwise be stubbed".into(),
                }]),
                tool_call_id: "c1".into(),
            },
            assistant_call_args("c2", "render", "{}"),
            tool_result("c2", big_result("r2")),
        ]);
        let before = serde_json::to_string(&req.messages[2]).unwrap();
        let _out = run(SupersedePass::new(1), &mut req);
        assert_eq!(
            serde_json::to_string(&req.messages[2]).unwrap(),
            before,
            "Parts tool content is not stubbed"
        );
    }
}
