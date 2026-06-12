//! ACON-style tool-result shaping for the `/tools` loop (research Phase 4):
//! trim verbose tool-result blobs and tool-call args *before they enter
//! history* — the dominant volatile-tail term in agent loops.
//!
//! Two layers, both applied exactly once at append time (shape-at-entry;
//! re-shaping messages already in history would be re-selection inside the
//! provider-cached prefix — the caching-tension rule):
//!
//! 1. **Lossless JSON minify** (default ON): whitespace in tool JSON is
//!    machine noise. Applies to tool RESULTS and to model-emitted tool-call
//!    ARGS (models routinely emit pretty-printed JSON). Registry results are
//!    already compact (`serde_json::Value::to_string`), so the meter honestly
//!    reads ~0 for them — the wins are args, field drops, and future tools.
//! 2. **Class-safe field drops** for KNOWN built-in tools only: a static
//!    allowlist per tool, hand-verified against the tool's source, dropping
//!    only fields that are provably redundant or nondeterministic machine
//!    noise (e.g. `inspect_diff`'s tempfile path). Unknown tools and
//!    `{"error":…}` blobs get minify only, NEVER field-dropping — the CLI
//!    loop has no judge harness, so lossy trimming of unknown shapes is out.
//!
//! Everything trimmed is metered in tokens (via `tt_tokenize`) and surfaced
//! as token counts only — never a USD claim (the gateway attributes no spend
//! to these bytes).

use serde_json::Value;

use tt_shared::messages::Message;

use super::budget::ESTIMATE_PROVIDER;

/// Tokens before/after shaping, for the turn footer + session ledger.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShapeStats {
    pub tokens_before: u32,
    pub tokens_after: u32,
}

impl ShapeStats {
    /// Tokens removed by shaping (0 when shaping grew or didn't change it).
    #[must_use]
    pub fn saved(&self) -> u32 {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}

/// Minify a JSON string (parse → compact re-serialize). Returns `None` when
/// `s` is not valid JSON — the caller must keep the original untouched (never
/// mangle non-JSON). Lossless at the JSON-value level; two caveats: key order
/// normalizes (the workspace `serde_json` has no `preserve_order`) and exotic
/// float literals re-render via `f64` — value-equal either way, and for
/// registry tools (serde-built values) an exact round-trip.
#[must_use]
pub fn minify_json(s: &str) -> Option<String> {
    let v: Value = serde_json::from_str(s).ok()?;
    Some(v.to_string())
}

/// A known tool's class-safe shaping decision. `keep` lists top-level fields
/// to retain; `finding_keep` projects each element of a `findings` array.
struct ToolShape {
    keep: &'static [&'static str],
    finding_keep: Option<&'static [&'static str]>,
}

/// The allowlist table. Every entry is hand-verified against the tool's
/// source and pinned by `shape_known_tools_apply_allowlists`; the guard test
/// `every_registered_tool_has_an_allowlist_decision` forces an explicit entry
/// (even an identity one) for every registered tool.
///
/// - `find_route_for` → identity: {model, rationale} — the rationale carries
///   the honesty disclaimer; everything is needed.
/// - `preview_cost` → drop `trace_id` (opaque correlation id the loop never
///   dereferences; nondeterministic). `warnings` is ALWAYS kept (honesty).
/// - `inspect_diff` → drop per-finding `file`: it is the random tempfile path
///   the tool scanned — pure nondeterministic noise.
/// - `batch_savings` → drop per-finding `summary` ONLY: prose rendered 1:1
///   from the numeric fields that remain. `note` (the advisory-honesty
///   disclaimer) is kept.
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

fn project(v: Value, shape: &ToolShape) -> Value {
    let Value::Object(map) = v else {
        // Non-object output → nothing class-safe to drop; minify only.
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

/// Shape one tool result before it enters history. KNOWN tool → project
/// through its class-safe allowlist, then minify; UNKNOWN tool or an
/// `{"error":…}` blob → minify only (never field-dropping). Returns the wire
/// string plus before/after token estimates.
#[must_use]
pub fn shape_tool_result(tool_name: &str, v: Value) -> (String, ShapeStats) {
    let before = v.to_string();
    let tokens_before = tt_tokenize::estimate_tokens(ESTIMATE_PROVIDER, &before);
    let is_error = v.as_object().is_some_and(|o| o.contains_key("error"));
    let out = match (shape_for(tool_name), is_error) {
        (Some(shape), false) => project(v, shape).to_string(),
        // `Value::to_string` is already the minified rendering.
        _ => before,
    };
    let tokens_after = tt_tokenize::estimate_tokens(ESTIMATE_PROVIDER, &out);
    (
        out,
        ShapeStats {
            tokens_before,
            tokens_after,
        },
    )
}

/// Minify the JSON `arguments` of every tool call on an `Assistant` message
/// (the history copy only — providers don't verify echoed arg bytes, and the
/// value is identical). Non-JSON args are left untouched. Returns the summed
/// before/after token estimates of every argument string that was minified.
pub fn minify_tool_call_args(msg: &mut Message) -> ShapeStats {
    let mut stats = ShapeStats::default();
    if let Message::Assistant { tool_calls, .. } = msg {
        for tc in tool_calls {
            if let Some(min) = minify_json(&tc.function.arguments) {
                stats.tokens_before +=
                    tt_tokenize::estimate_tokens(ESTIMATE_PROVIDER, &tc.function.arguments);
                stats.tokens_after += tt_tokenize::estimate_tokens(ESTIMATE_PROVIDER, &min);
                tc.function.arguments = min;
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minify_json_lossless_and_idempotent() {
        let pretty = "{\n  \"a\": 1,\n  \"b\": [1, 2],\n  \"c\": \"keep  inner  spaces\"\n}";
        let min = minify_json(pretty).expect("valid JSON");
        assert!(!min.contains('\n'), "{min}");
        // lossless: value-equality before/after
        let v1: Value = serde_json::from_str(pretty).unwrap();
        let v2: Value = serde_json::from_str(&min).unwrap();
        assert_eq!(v1, v2);
        // string contents (incl. inner whitespace) untouched
        assert!(min.contains("keep  inner  spaces"), "{min}");
        // idempotent
        assert_eq!(minify_json(&min).as_deref(), Some(min.as_str()));
        // non-JSON → None (caller keeps the original untouched)
        assert!(minify_json("not json {").is_none());
        assert!(minify_json("").is_none());
    }

    #[test]
    fn shape_known_tools_apply_allowlists() {
        // inspect_diff: per-finding `file` (random tempfile path) dropped,
        // everything the loop needs kept.
        let v = json!({
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
        let (out, stats) = shape_tool_result("inspect_diff", v);
        assert!(!out.contains(".tt-scan-8c1f2"), "tempfile path kept: {out}");
        for kept in [
            "rule_id",
            "severity",
            "line",
            "message",
            "confidence",
            "fix_hint",
            "scanned",
            "detected_language",
        ] {
            assert!(out.contains(kept), "missing `{kept}`: {out}");
        }
        assert!(stats.tokens_before > stats.tokens_after, "{stats:?}");

        // preview_cost: trace_id dropped, warnings ALWAYS kept.
        let v = json!({
            "current": {"model": "gpt-4o", "cost_usd": 0.01},
            "cache_projections": {"l1_hit_savings_usd": 0.005},
            "route_suggestions": [],
            "warnings": ["estimate only"],
            "trace_id": "3f2c9a1e-dead-beef"
        });
        let (out, _) = shape_tool_result("preview_cost", v);
        assert!(!out.contains("trace_id"), "{out}");
        assert!(out.contains("estimate only"), "warnings dropped: {out}");

        // batch_savings: per-finding derived `summary` prose dropped; the
        // numeric fields and the advisory-honesty `note` kept.
        let v = json!({
            "findings": [{
                "tag": "nightly",
                "eligible_spend_usd": 10.0,
                "projected_batch_cost_usd": 5.0,
                "projected_savings_usd": 5.0,
                "share_of_spend_pct": 40.0,
                "discount_pct": 50.0,
                "request_count": 12,
                "summary": "tag nightly could save $5.00 (50%) via the Batch API"
            }],
            "total_projected_savings_usd": 5.0,
            "note": "Advisory projection — nothing was submitted."
        });
        let (out, _) = shape_tool_result("batch_savings", v);
        assert!(!out.contains("could save"), "derived prose kept: {out}");
        assert!(out.contains("Advisory projection"), "note dropped: {out}");
        assert!(out.contains("projected_savings_usd"), "{out}");

        // find_route_for: identity (everything is needed).
        let v = json!({"model": "claude-haiku", "rationale": "heuristic — verify on your data"});
        let (out, stats) = shape_tool_result("find_route_for", v.clone());
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), v);
        assert_eq!(stats.tokens_before, stats.tokens_after);
    }

    #[test]
    fn shape_unknown_tool_is_minify_only() {
        let v = json!({"weird_field": "keep me", "nested": {"deep": [1, 2, 3]}});
        let (out, stats) = shape_tool_result("not_a_registered_tool", v.clone());
        // no field dropped — only (already-)minified rendering
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), v);
        assert_eq!(stats.tokens_before, stats.tokens_after);
    }

    #[test]
    fn error_blobs_are_never_field_dropped() {
        // a known tool name, but the registry returned an error blob — the
        // allowlist must not strip the error out of history.
        let v = json!({"error": "invalid params: missing file_path"});
        let (out, _) = shape_tool_result("inspect_diff", v.clone());
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), v);
    }

    #[test]
    fn every_registered_tool_has_an_allowlist_decision() {
        let regs = [
            crate::chat::tools::build_registry(),
            crate::chat::tools::build_advisor_registry(),
        ];
        for reg in &regs {
            for def in reg.list() {
                assert!(
                    shape_for(def.name).is_some(),
                    "tool `{}` is registered without an explicit shaping \
                     decision — add a class-safe allowlist entry (or an \
                     identity entry) to TOOL_SHAPES in chat/shape.rs",
                    def.name
                );
            }
        }
    }

    #[test]
    fn minify_tool_call_args_rewrites_history_copy() {
        use tt_shared::messages::{ToolCall, ToolCallFunction};
        let pretty = "{\n    \"task_description\":   \"classify sentiment of tweets\"\n}";
        let mut msg = Message::Assistant {
            content: None,
            tool_calls: vec![
                ToolCall {
                    id: "c1".into(),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: "find_route_for".into(),
                        arguments: pretty.into(),
                    },
                },
                ToolCall {
                    id: "c2".into(),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: "odd_tool".into(),
                        arguments: "not json".into(),
                    },
                },
            ],
            name: None,
        };
        let stats = minify_tool_call_args(&mut msg);
        let Message::Assistant { tool_calls, .. } = &msg else {
            unreachable!()
        };
        assert!(!tool_calls[0].function.arguments.contains('\n'));
        // value preserved
        assert_eq!(
            serde_json::from_str::<Value>(&tool_calls[0].function.arguments).unwrap(),
            serde_json::from_str::<Value>(pretty).unwrap()
        );
        // non-JSON args untouched
        assert_eq!(tool_calls[1].function.arguments, "not json");
        assert!(stats.tokens_before > stats.tokens_after, "{stats:?}");
    }
}
