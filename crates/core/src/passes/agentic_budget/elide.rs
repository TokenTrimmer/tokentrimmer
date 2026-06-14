//! Sub-lever 2a — **lossless field-drop of stale KNOWN-tool results**
//! (token-true gate only, NO judge).
//!
//! The first, cheapest half of Sub-lever 2 (spec §4.1 Sub-lever 2 tier 1):
//! generalize the CLI's ACON tool-result shaping ([`tt-cli`'s `chat/shape.rs`])
//! into a gateway [`RequestPass`](crate::passes::RequestPass) on the **volatile
//! tail only** — the prefix is unreachable by type, so this can never bust the
//! provider's prompt cache. Two lossless operations, applied to `role:"tool"`
//! result blocks:
//!
//! 1. **Lossless JSON minify** of every tool-result blob (whitespace in tool
//!    JSON is machine noise). Registry-built results are already compact, so the
//!    meter reads ~0 for them — the wins are pretty-printed results and field
//!    drops.
//! 2. **Class-safe field drops** for KNOWN built-in tools only, via a static
//!    allowlist (seeded with the W4 dashboard-chat tools so Phase B works). Each
//!    entry is hand-verified against the tool's source; it drops only fields
//!    that are provably redundant or nondeterministic machine noise (e.g.
//!    `inspect_diff`'s tempfile `file` path). Unknown tools and `{"error":…}`
//!    blobs get minify ONLY, never field-dropping — this lossless layer has no
//!    judge, so lossy trimming of unknown shapes is out (that is Sub-lever 2b,
//!    Task 7, gated separately behind the blind paired judge).
//!
//! # Why it commits unverified (spec §4.1: "token-true gate is the only check")
//!
//! Field-drop is lossless by construction (the allowlist is hand-verified to
//! drop only redundant/nondeterministic fields), so it commits on the pipeline's
//! token-true gate alone — no judge. If a degenerate "drop" ever re-inflated
//! tokens (re-pretty-printing, say), the gate
//! ([`PassPipeline::run`](crate::passes::PassPipeline::run)) discards it
//! byte-identical and books zero, exactly like the `InflatingPass` contract.
//!
//! # `keep_recent_pairs` (caveat C1 blast-radius bound)
//!
//! The last `keep_recent_pairs` tool-result blocks are kept **verbatim** even
//! when known: the load-bearing recent context the agent is actively reasoning
//! over is never shaped. This is the same bound Sub-lever 2b's summarization
//! honors — the judge is only a ~2% statistical sample, so the recent tail must
//! stay verbatim independent of any judge verdict.
//!
//! # Sub-lever 2b — the lossy summary half (Task 7)
//!
//! The lossy second tier of Sub-lever 2 lives in the sibling
//! [`super::summarize_judge`] module: [`super::summarize_judge::SummarizeStep`]
//! rewrites OLDER tool blocks to a short summary, but — unlike this lossless
//! field-drop layer, which commits on the token-true gate alone — it commits a
//! drop ONLY behind the blind paired equivalence judge's verdict projection
//! ([`super::summarize_judge::SummaryGate`], caveat C1). The two halves share
//! the SAME `keep_recent_pairs` blast-radius bound: neither ever touches the
//! recent tail. The planner ([`super::AgenticBudgetPlanner`]) WILL compose them
//! in priority order — lossless field-drop first, then judge-gated summary — but
//! that wiring is deferred to a later task: today the planner runs field-drop
//! (Sub-lever 2a) only, and [`super::summarize_judge::SummarizeStep`] is a
//! standalone building block with no planner consumer yet.

use serde_json::Value;

use tt_shared::messages::{Message, MessageContent};

/// A known tool's class-safe shaping decision (ported from the CLI's
/// `chat/shape.rs`): `keep` lists top-level fields to retain; `finding_keep`
/// projects each element of a `findings` array.
struct ToolShape {
    keep: &'static [&'static str],
    finding_keep: Option<&'static [&'static str]>,
}

/// The server-side allowlist, seeded with the W4 dashboard-chat tools so
/// Phase B (the `dashboard-chat` proving ground) works. Every entry is
/// hand-verified against the tool's source and mirrors the CLI's `TOOL_SHAPES`
/// (`chat/shape.rs`) decisions:
///
/// - `find_route_for` → identity {model, rationale} (everything is needed).
/// - `preview_cost` → drop `trace_id` (opaque correlation id; nondeterministic).
///   `warnings` is ALWAYS kept (honesty).
/// - `inspect_diff` → drop per-finding `file`: the random tempfile path the
///   tool scanned — pure nondeterministic noise.
/// - `batch_savings` → drop per-finding `summary` (prose rendered 1:1 from the
///   numeric fields that remain). `note` (the advisory disclaimer) is kept.
const TOOL_SHAPES: &[(&str, ToolShape)] = &[
    (
        "find_route_for",
        ToolShape {
            keep: &["model", "rationale"],
            finding_keep: None,
        },
    ),
    (
        "preview_cost",
        ToolShape {
            keep: &[
                "current",
                "cache_projections",
                "route_suggestions",
                "warnings",
            ],
            finding_keep: None,
        },
    ),
    (
        "inspect_diff",
        ToolShape {
            keep: &["scanned", "detected_language", "reason", "findings"],
            finding_keep: Some(&[
                "rule_id",
                "severity",
                "line",
                "message",
                "confidence",
                "fix_hint",
            ]),
        },
    ),
    (
        "batch_savings",
        ToolShape {
            keep: &["findings", "total_projected_savings_usd", "note"],
            finding_keep: Some(&[
                "tag",
                "eligible_spend_usd",
                "projected_batch_cost_usd",
                "projected_savings_usd",
                "share_of_spend_pct",
                "discount_pct",
                "request_count",
            ]),
        },
    ),
];

fn shape_for(tool: &str) -> Option<&'static ToolShape> {
    TOOL_SHAPES
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, s)| s)
}

fn project_object(v: Value, keep: &[&str]) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| keep.contains(&k.as_str()))
                .collect(),
        ),
        other => other,
    }
}

/// Project a known tool's output through its class-safe allowlist (field-drop),
/// recursing into a `findings` array when `finding_keep` is set. Non-object
/// output → nothing class-safe to drop (the caller minifies only).
fn project(v: Value, shape: &ToolShape) -> Value {
    let Value::Object(map) = v else {
        return v;
    };
    let projected = map
        .into_iter()
        .filter(|(k, _)| shape.keep.contains(&k.as_str()))
        .map(|(k, val)| {
            let val = match (k.as_str(), shape.finding_keep, val) {
                ("findings", Some(keep), Value::Array(items)) => Value::Array(
                    items
                        .into_iter()
                        .map(|item| project_object(item, keep))
                        .collect(),
                ),
                (_, _, val) => val,
            };
            (k, val)
        })
        .collect();
    Value::Object(projected)
}

/// Shape one tool-result blob losslessly. KNOWN tool (and not an `{"error":…}`
/// blob) → project through its class-safe allowlist, then minify; UNKNOWN tool
/// or an error blob → minify only (never field-dropping). Returns the new wire
/// string, or `None` when the content is not JSON (keep the original untouched).
fn shape_tool_result(tool_name: Option<&str>, content: &str) -> Option<String> {
    let v: Value = serde_json::from_str(content).ok()?;
    let is_error = v.as_object().is_some_and(|o| o.contains_key("error"));
    let out = match (tool_name.and_then(shape_for), is_error) {
        (Some(shape), false) => project(v, shape).to_string(),
        // `Value::to_string` is the minified rendering (lossless for unknown
        // tools / error blobs — never field-dropping).
        _ => v.to_string(),
    };
    Some(out)
}

/// Sub-lever 2a — the lossless field-drop / minify pass over stale KNOWN-tool
/// results, operating on the volatile tail only (the prefix is unreachable by
/// type, so this can never bust the cache).
///
/// Keeps the last `keep_recent_pairs` tool-result blocks verbatim (caveat C1);
/// minifies older tool-result JSON, and field-drops known tools through the
/// class-safe allowlist. Self-reports its estimated delta; the pipeline's
/// token-true gate is the only authority (spec §4.1 tier 1).
pub struct ElidePass {
    /// Keep the last N tool-result blocks (`role:"tool"`) verbatim — the
    /// load-bearing recent context is never shaped (caveat C1 blast-radius
    /// bound). Mirrors `AgenticBudget::keep_recent_pairs`.
    keep_recent_pairs: u32,
}

impl ElidePass {
    /// A field-drop pass keeping the last `keep_recent_pairs` tool-result
    /// blocks verbatim.
    #[must_use]
    pub fn new(keep_recent_pairs: u32) -> Self {
        Self { keep_recent_pairs }
    }
}

/// Resolve the tool NAME for the `Message::Tool` at `tail_idx` by matching its
/// `tool_call_id` against the `tool_calls` of any `Message::Assistant` in the
/// tail (the gateway carries only the id on a tool message; the name lives on
/// the assistant turn that emitted the call). `None` when no match is found
/// (treated as an UNKNOWN tool → minify only).
fn resolve_tool_name(msgs: &[Message], tail_idx: usize) -> Option<&str> {
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
            .map(|tc| tc.function.name.as_str())
    })
}

impl crate::passes::RequestPass for ElidePass {
    fn name(&self) -> &'static str {
        "agentic-field-drop"
    }

    fn apply(
        &self,
        _stable: &crate::passes::StablePrefix<'_>,
        tail: &mut crate::passes::VolatileTail<'_>,
        _cx: &crate::passes::PassContext<'_>,
    ) -> crate::passes::PassOutcome {
        // The tail indices of every tool-result block, in order.
        let tool_idxs: Vec<usize> = tail
            .messages()
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m, Message::Tool { .. }))
            .map(|(i, _)| i)
            .collect();

        // Keep the last `keep_recent_pairs` tool-result blocks verbatim
        // (caveat C1): only blocks BEFORE this cutoff are eligible for shaping.
        let keep = self.keep_recent_pairs as usize;
        let eligible_cutoff = tool_idxs.len().saturating_sub(keep);
        let eligible: Vec<usize> = tool_idxs.into_iter().take(eligible_cutoff).collect();

        // Resolve each eligible tool's NAME against the (read-only) tail before
        // mutating, so the name lookup is unaffected by content edits.
        let snapshot = tail.messages().to_vec();
        let mut tokens_removed: u32 = 0;

        for &idx in &eligible {
            let tool_name = resolve_tool_name(&snapshot, idx);
            let Message::Tool { content, .. } = &mut tail.messages_mut()[idx] else {
                continue;
            };
            let MessageContent::Text(text) = content else {
                // Non-text tool content (parts) → nothing JSON to shape.
                continue;
            };
            let Some(shaped) = shape_tool_result(tool_name, text) else {
                // Not JSON → keep the original byte-for-byte (never mangle).
                continue;
            };
            if shaped.len() < text.len() {
                tokens_removed =
                    tokens_removed.saturating_add((text.len().saturating_sub(shaped.len())) as u32);
            }
            *text = shaped;
        }

        // Self-reported estimate only (byte delta, not a token count): the
        // pipeline's token-true gate measures the real tokenizer delta and uses
        // THAT for attribution (a mismatch is logged at debug, never billed).
        crate::passes::PassOutcome {
            tokens_removed,
            warnings: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tt_shared::messages::{Message, MessageContent, ToolCall, ToolCallFunction};
    use tt_shared::ChatCompletionRequest;

    use crate::passes::{PassContext, PassPipeline, SplitRequest};

    fn user(text: &str) -> Message {
        Message::User {
            content: MessageContent::Text(text.into()),
            name: None,
        }
    }
    fn assistant_call(call_id: &str, tool: &str) -> Message {
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: tool.into(),
                    arguments: "{}".into(),
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
    fn tool_result_str(call_id: &str, content: &str) -> Message {
        Message::Tool {
            content: MessageContent::Text(content.into()),
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
    /// prefix): the whole message list is the volatile tail, so the pass can
    /// reach the tool results under test.
    fn cx() -> PassContext<'static> {
        PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        }
    }

    fn run(pass: ElidePass, req: &mut ChatCompletionRequest) -> crate::passes::PipelineOutcome {
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
            panic!("expected text content");
        };
        s.clone()
    }

    /// A `Message::Tool` whose content is the `inspect_diff` result with a
    /// tempfile `file` path drops `file` (lossless: nondeterministic machine
    /// noise), and the token-true gate commits it (tokens decrease). The call
    /// is OLDER than `keep_recent_pairs` so it is eligible for shaping.
    #[test]
    fn field_drop_known_tool_removes_redundant_field() {
        let diff = json!({
            "scanned": true,
            "detected_language": "python",
            "findings": [{
                "rule_id": "cache-missing",
                "severity": "high",
                "file": "/tmp/.tt-scan-8c1f2.py",
                "line": 3,
                "message": "m",
                "confidence": 0.9,
                "fix_hint": "h"
            }]
        });
        let mut req = req_with(vec![
            user("scan my diff"),
            assistant_call("c1", "inspect_diff"),
            tool_result("c1", diff),
        ]);
        // keep=0 makes this single tool block eligible for shaping.
        let out = run(ElidePass::new(0), &mut req);

        let shaped = tool_text(&req, 2);
        assert!(
            !shaped.contains(".tt-scan-8c1f2"),
            "tempfile path kept: {shaped}"
        );
        for kept in [
            "rule_id",
            "severity",
            "line",
            "message",
            "confidence",
            "fix_hint",
        ] {
            assert!(shaped.contains(kept), "missing `{kept}`: {shaped}");
        }
        assert!(
            out.tokens_removed > 0,
            "the token-true gate must commit the field-drop with a positive delta"
        );
        assert!(
            out.rejected.is_empty(),
            "lossless field-drop is not rejected"
        );
    }

    /// An unknown tool's blob gets JSON minify ONLY, never field-dropping (no
    /// judge in the lossless layer → lossy trimming of unknown shapes is out).
    #[test]
    fn field_drop_unknown_tool_minify_only() {
        let pretty = "{\n  \"weird_field\": \"keep me\",\n  \"nested\": {\"deep\": [1, 2, 3]}\n}";
        let mut req = req_with(vec![
            user("call a strange tool"),
            assistant_call("c1", "not_a_registered_tool"),
            tool_result_str("c1", pretty),
        ]);
        let _out = run(ElidePass::new(0), &mut req);

        let shaped = tool_text(&req, 2);
        // No field dropped: value preserved exactly.
        assert_eq!(
            serde_json::from_str::<Value>(&shaped).unwrap(),
            serde_json::from_str::<Value>(pretty).unwrap(),
            "unknown tool must not be field-dropped: {shaped}"
        );
        // …but it WAS minified (whitespace removed).
        assert!(
            !shaped.contains('\n'),
            "unknown tool blob must be minified: {shaped}"
        );
    }

    /// With `keep_recent_pairs=3`, the last 3 tool-result blocks are untouched
    /// even when known (caveat C1 — keep recent verbatim). Only the OLDEST
    /// block (outside the window) is shaped.
    #[test]
    fn field_drop_respects_keep_recent_pairs() {
        let diff = json!({
            "scanned": true,
            "detected_language": "python",
            "findings": [{
                "rule_id": "r",
                "severity": "high",
                "file": "/tmp/.tt-scan-OLD.py",
                "line": 1,
                "message": "m",
                "confidence": 0.9,
                "fix_hint": "h"
            }]
        });
        let recent = |path: &str| {
            json!({
                "scanned": true,
                "detected_language": "python",
                "findings": [{
                    "rule_id": "r",
                    "severity": "high",
                    "file": path,
                    "line": 1,
                    "message": "m",
                    "confidence": 0.9,
                    "fix_hint": "h"
                }]
            })
        };
        let mut req = req_with(vec![
            user("scan repeatedly"),
            assistant_call("c0", "inspect_diff"),
            tool_result("c0", diff),
            assistant_call("c1", "inspect_diff"),
            tool_result("c1", recent("/tmp/.tt-scan-R1.py")),
            assistant_call("c2", "inspect_diff"),
            tool_result("c2", recent("/tmp/.tt-scan-R2.py")),
            assistant_call("c3", "inspect_diff"),
            tool_result("c3", recent("/tmp/.tt-scan-R3.py")),
        ]);
        let _out = run(ElidePass::new(3), &mut req);

        // The OLDEST tool block (idx 2, c0) is OUTSIDE the keep-3 window → shaped.
        let oldest = tool_text(&req, 2);
        assert!(
            !oldest.contains(".tt-scan-OLD"),
            "the oldest (eligible) tool result should be field-dropped: {oldest}"
        );
        // The last 3 (idx 4, 6, 8) are kept VERBATIM — their tempfile paths
        // survive (caveat C1 blast-radius bound).
        for (idx, marker) in [(4, "R1"), (6, "R2"), (8, "R3")] {
            let recent_text = tool_text(&req, idx);
            assert!(
                recent_text.contains(&format!(".tt-scan-{marker}")),
                "the last keep_recent_pairs tool results must stay verbatim (idx {idx}): {recent_text}"
            );
        }
    }

    /// A degenerate "drop" that re-pretty-prints (adds tokens) is discarded by
    /// the token-true gate (reuse of the `InflatingPass` contract): the tail is
    /// restored byte-identical and zero books. We model the degenerate case by
    /// feeding ALREADY-minimal known-tool JSON with a field that, after the
    /// allowlist, would round-trip to MORE bytes only if the pass mis-behaved —
    /// here we instead directly assert the gate rejects a synthetic inflating
    /// variant via a wrapper pass.
    #[test]
    fn field_drop_rejected_if_inflates() {
        // A pass that, in the spirit of a degenerate field-drop, re-pretty-prints
        // a tool result (adding whitespace tokens) while claiming a saving. The
        // token-true gate must discard it byte-identical.
        struct InflatingFieldDrop;
        impl crate::passes::RequestPass for InflatingFieldDrop {
            fn name(&self) -> &'static str {
                "agentic-field-drop"
            }
            fn apply(
                &self,
                _stable: &crate::passes::StablePrefix<'_>,
                tail: &mut crate::passes::VolatileTail<'_>,
                _cx: &crate::passes::PassContext<'_>,
            ) -> crate::passes::PassOutcome {
                if let Some(Message::Tool {
                    content: MessageContent::Text(s),
                    ..
                }) = tail.messages_mut().last_mut()
                {
                    // Re-pretty-print: parse + serialize WITH indentation.
                    if let Ok(v) = serde_json::from_str::<Value>(s) {
                        *s = serde_json::to_string_pretty(&v).unwrap();
                    }
                }
                crate::passes::PassOutcome {
                    tokens_removed: 99, // lies
                    warnings: vec![],
                }
            }
        }

        let diff = json!({
            "scanned": true,
            "detected_language": "python",
            "findings": [{"rule_id": "r", "severity": "high", "line": 1, "message": "m", "confidence": 0.9, "fix_hint": "h"}]
        });
        let mut req = req_with(vec![
            user("scan"),
            assistant_call("c1", "inspect_diff"),
            tool_result("c1", diff),
        ]);
        let before = serde_json::to_string(&req).unwrap();
        let cx = cx();
        let pipe = PassPipeline::new().with(InflatingFieldDrop);
        let mut split = SplitRequest::compute(&mut req, &cx);
        let out = pipe.run(&mut split, &cx);

        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "an inflating field-drop must be rolled back byte-identical"
        );
        assert_eq!(out.tokens_removed, 0, "an inflating pass books zero");
        assert_eq!(out.rejected, vec!["agentic-field-drop"]);
    }

    /// A known tool whose blob is an `{"error":…}` result is never field-dropped
    /// (the allowlist must not strip the error out of history) — minify only.
    #[test]
    fn field_drop_error_blob_is_minify_only() {
        let pretty = "{\n  \"error\": \"invalid params: missing file_path\"\n}";
        let mut req = req_with(vec![
            user("scan"),
            assistant_call("c1", "inspect_diff"),
            tool_result_str("c1", pretty),
        ]);
        let _out = run(ElidePass::new(0), &mut req);

        let shaped = tool_text(&req, 2);
        assert_eq!(
            serde_json::from_str::<Value>(&shaped).unwrap(),
            json!({"error": "invalid params: missing file_path"}),
            "error blob must keep its error verbatim: {shaped}"
        );
    }

    /// Non-JSON tool content is kept byte-for-byte (never mangle non-JSON).
    #[test]
    fn field_drop_non_json_untouched() {
        let mut req = req_with(vec![
            user("call"),
            assistant_call("c1", "inspect_diff"),
            tool_result_str("c1", "not json {"),
        ]);
        let before = serde_json::to_string(&req).unwrap();
        let _out = run(ElidePass::new(0), &mut req);
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "non-JSON tool content must be left untouched"
        );
    }
}
