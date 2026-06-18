//! Server-side execution of the read-only TT "gateway tools" the agent loop
//! can run inline. Mirrors the MCP tools' logic but calls the underlying libs
//! directly (no `core->mcp` dependency cycle). Read-only/idempotent only —
//! the allowlist mirrors `agentic_budget::substep_cache::READ_ONLY_TOOLS`.
//!
//! The four tools are ported from `crates/mcp/src/tools/{find_route_for,
//! preview_cost,inspect_diff,batch_savings}.rs`. The `find_route_for` keyword
//! heuristic is inlined here (it has no shared lib); the other three call their
//! underlying libraries (`tt_preview`, `tt_inspect_core` + `tt_inspect_rules_tier1`,
//! `tt_shared::batch_advisor` + `tt_shared::pricing`) directly.
//!
//! `dead_code` is allowed module-wide: the public surface (`execute`,
//! `is_gateway_tool`, `GATEWAY_TOOLS`) and the private `run_*` helpers are
//! exercised by the in-module tests but have no non-test caller until the agent
//! loop (`routes::agent_run`, slice 1a Task 3) consumes them.
#![allow(dead_code)]

use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

use tt_shared::batch_advisor::{
    project_batch_savings, project_batch_savings_with_tags, RequestAggregate,
};
use tt_shared::pricing::catalog;

/// The hand-verified read-only tool allowlist. A tool not on this list is NOT
/// gateway-executable (the loop round-trips it to the client in slice 1b).
///
/// Mirrors `crates/core/src/passes/agentic_budget/substep_cache.rs::READ_ONLY_TOOLS`.
pub(crate) const GATEWAY_TOOLS: &[&str] = &[
    "find_route_for",
    "preview_cost",
    "inspect_diff",
    "batch_savings",
];

/// True when `name` is one of the gateway-executable read-only tools.
pub(crate) fn is_gateway_tool(name: &str) -> bool {
    GATEWAY_TOOLS.contains(&name)
}

/// The only hard error the executor can raise: a tool that is not on the
/// gateway allowlist and therefore cannot be run server-side (the loop maps it
/// to `incomplete` and round-trips it to the client in slice 1b).
///
/// A tool-*internal* error (bad arguments, an underlying-lib failure) is NOT an
/// `Err`; it is returned as `Ok(error_text)` so the model can read it and react.
#[derive(Debug, Error)]
pub(crate) enum GatewayToolError {
    /// `name` is not a gateway tool — it cannot be executed inline.
    #[error("tool '{0}' is not a gateway-executable read-only tool")]
    NotExecutable(String),
}

/// Execute a gateway tool by name with its JSON `arguments` string (OpenAI
/// tool-call convention). Returns the tool result as a string for a
/// `Message::Tool` body. A tool error is returned as `Ok(error_text)` so the
/// model can react (NOT an `Err` that aborts the run) — except a genuinely
/// non-gateway tool which is `Err(GatewayToolError::NotExecutable)`.
pub(crate) fn execute(name: &str, arguments: &str) -> Result<String, GatewayToolError> {
    match name {
        "find_route_for" => Ok(run_find_route_for(arguments)),
        "preview_cost" => Ok(run_preview_cost(arguments)),
        "inspect_diff" => Ok(run_inspect_diff(arguments)),
        "batch_savings" => Ok(run_batch_savings(arguments)),
        _ => Err(GatewayToolError::NotExecutable(name.to_string())),
    }
}

// ── find_route_for (pure keyword heuristic, inlined) ─────────────────────────

#[derive(Deserialize)]
struct FindRouteInput {
    task_description: String,
}

/// Task class assigned by keyword matching. Ported from
/// `crates/mcp/src/tools/find_route_for.rs::TaskClass`.
#[derive(Debug, PartialEq)]
enum TaskClass {
    Classification,
    Extraction,
    Code,
    Reasoning,
    General,
}

/// Classify a task description into a [`TaskClass`] by keyword heuristic.
fn classify_task(lower: &str) -> TaskClass {
    if lower.contains("classif")
        || lower.contains("yes or no")
        || lower.contains("boolean")
        || lower.contains("is it a")
        || lower.contains("label")
        || lower.contains("categorize")
        || lower.contains("categorise")
        || lower.contains("spam")
        || lower.contains("sentiment")
    {
        TaskClass::Classification
    } else if lower.contains("extract")
        || lower.contains("parse")
        || lower.contains("json")
        || lower.contains("structured output")
        || lower.contains("schema")
        || lower.contains("fill in")
    {
        TaskClass::Extraction
    } else if lower.contains("reason")
        || lower.contains("analyz")
        || lower.contains("analys")
        || lower.contains("compare")
        || lower.contains("evaluate")
        || lower.contains("explain in depth")
        || lower.contains("step by step")
        || lower.contains("summarize")
        || lower.contains("summarise")
        || lower.contains("complex")
    {
        // Reasoning is checked before Code so reasoning intent ("analyze this
        // code", "compare these diffs") wins over incidental code nouns; a
        // prompt with only code keywords still falls through to Code below.
        TaskClass::Reasoning
    } else if lower.contains("code")
        || lower.contains("function")
        || lower.contains("refactor")
        || lower.contains("debug")
        || lower.contains("implement")
        || lower.contains("unit test")
        || lower.contains("diff")
        || lower.contains("compile")
    {
        TaskClass::Code
    } else {
        TaskClass::General
    }
}

/// Return the (model_id, rationale) heuristic pair for a task class.
///
/// Model IDs are cross-referenced against `crates/shared/data/pricing.toml`.
/// This is a static heuristic — not backed by per-organisation telemetry.
fn route(class: &TaskClass) -> (&'static str, &'static str) {
    match class {
        TaskClass::Classification => (
            "claude-haiku-4-5",
            "classification / boolean — keyword heuristic: short yes/no \
             or label tasks typically need minimal reasoning; \
             claude-haiku-4-5 is the cheapest Anthropic model in the catalog \
             ($1/$5 per M tokens). Not based on your telemetry.",
        ),
        TaskClass::Extraction => (
            "claude-haiku-4-5",
            "extraction / parsing — keyword heuristic: structured-output tasks \
             with an explicit schema are reliably handled by small models when \
             given a clear prompt; claude-haiku-4-5 is the cheapest option \
             in the catalog ($1/$5 per M tokens). Not based on your telemetry.",
        ),
        TaskClass::Code => (
            "claude-sonnet-4-6",
            "code / refactoring — keyword heuristic: code generation, debugging \
             and multi-step refactors benefit from stronger reasoning; \
             claude-sonnet-4-6 is the mid-tier Anthropic model \
             ($3/$15 per M tokens). Not based on your telemetry.",
        ),
        TaskClass::Reasoning => (
            "claude-sonnet-4-6",
            "reasoning / analysis — keyword heuristic: tasks involving \
             comparison, evaluation or detailed explanation benefit from \
             claude-sonnet-4-6's stronger reasoning \
             ($3/$15 per M tokens). Not based on your telemetry.",
        ),
        TaskClass::General => (
            "claude-haiku-4-5",
            "general / chat — keyword heuristic: no strong signal detected; \
             defaulting to claude-haiku-4-5 as the lowest-cost option \
             in the catalog ($1/$5 per M tokens). \
             Escalate to claude-sonnet-4-6 or claude-opus-4-7 if quality \
             is insufficient. Not based on your telemetry.",
        ),
    }
}

fn run_find_route_for(arguments: &str) -> String {
    let inp: FindRouteInput = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return format!("find_route_for: invalid arguments: {e}"),
    };
    let lower = inp.task_description.to_lowercase();
    let class = classify_task(&lower);
    let (model, rationale) = route(&class);
    json!({ "model": model, "rationale": rationale }).to_string()
}

// ── preview_cost (tt_preview) ────────────────────────────────────────────────

fn run_preview_cost(arguments: &str) -> String {
    let req: tt_preview::PreviewRequest = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return format!("preview_cost: invalid arguments: {e}"),
    };
    match tt_preview::preview(&req) {
        Ok(resp) => match serde_json::to_string(&resp) {
            Ok(s) => s,
            Err(e) => format!("preview_cost: serialize error: {e}"),
        },
        Err(e) => format!("preview_cost: error: {e}"),
    }
}

// ── inspect_diff (tt_inspect_core + tt_inspect_rules_tier1) ──────────────────

#[derive(Deserialize)]
struct InspectDiffInput {
    file_path: String,
    proposed_content: String,
}

/// Sanitize a caller-supplied file extension into a short alphanumeric token.
///
/// The extension only steers language detection for the temp file, so we keep
/// it to ASCII-alphanumeric and cap its length — a caller can't inject path or
/// suffix surprises through `file_path`.
fn sanitize_ext(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect()
}

fn run_inspect_diff(arguments: &str) -> String {
    let inp: InspectDiffInput = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return format!("inspect_diff: invalid arguments: {e}"),
    };
    let raw_ext = std::path::Path::new(&inp.file_path)
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("");
    let ext = sanitize_ext(raw_ext);

    // Resolve the language the engine would assign this file up front. If it's
    // one inspect doesn't scan, return an explicit reason rather than a bare
    // empty findings list — otherwise the caller can't tell "clean" from
    // "silently skipped because .txt isn't a scanned language".
    let Some(lang) = tt_inspect_core::Language::from_extension(&ext) else {
        return json!({
            "findings": [],
            "scanned": false,
            "detected_language": Value::Null,
            "reason": format!(
                "inspect does not scan '{}' — supported extensions: \
                 .py, .ts/.tsx, .js/.jsx/.mjs/.cjs, .md",
                inp.file_path
            ),
        })
        .to_string();
    };

    let suffix = format!(".{ext}");
    let mut tmp = match tempfile::Builder::new().suffix(&suffix).tempfile() {
        Ok(t) => t,
        Err(e) => return format!("inspect_diff: tempfile error: {e}"),
    };
    use std::io::Write;
    if let Err(e) = write!(tmp, "{}", inp.proposed_content) {
        return format!("inspect_diff: write error: {e}");
    }
    let mut engine = tt_inspect_core::Engine::new();
    for rule in tt_inspect_rules_tier1::all_rules() {
        engine.add_rule(rule);
    }
    let findings = engine.scan(tmp.path());
    json!({
        "findings": findings,
        "scanned": true,
        "detected_language": lang,
    })
    .to_string()
}

// ── batch_savings (tt_shared::batch_advisor + tt_shared::pricing) ────────────

#[derive(Deserialize)]
struct BatchSavingsInput {
    /// Request-log aggregates, one row per `(provider, model, tag)` segment.
    aggregates: Vec<RequestAggregate>,
    /// Optional override for the non-interactive tag set.
    #[serde(default)]
    eligible_tags: Option<Vec<String>>,
}

fn run_batch_savings(arguments: &str) -> String {
    let inp: BatchSavingsInput = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return format!("batch_savings: invalid arguments: {e}"),
    };

    let findings = match &inp.eligible_tags {
        Some(tags) => {
            let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
            project_batch_savings_with_tags(&inp.aggregates, catalog(), &refs)
        }
        None => project_batch_savings(&inp.aggregates, catalog()),
    };

    let total_savings: f64 = findings.iter().map(|f| f.projected_savings_usd).sum();
    let findings_json: Vec<Value> = findings
        .iter()
        .map(|f| {
            json!({
                "tag": f.tag,
                "eligible_spend_usd": f.eligible_spend_usd,
                "projected_batch_cost_usd": f.projected_batch_cost_usd,
                "projected_savings_usd": f.projected_savings_usd,
                "share_of_spend_pct": f.share_of_spend_pct,
                "discount_pct": f.discount_pct(),
                "request_count": f.request_count,
                "summary": f.summary(),
            })
        })
        .collect();

    json!({
        "findings": findings_json,
        "total_projected_savings_usd": total_savings,
        "note": "Advisory projection over request_logs — savings use real catalog \
            Batch-API rates. Nothing was submitted to a batch API. Building the \
            durable batch queue is deferred.",
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_gating() {
        assert!(is_gateway_tool("preview_cost"));
        assert!(!is_gateway_tool("write_file"));
    }

    /// The gateway allowlist must stay byte-for-byte in sync with the cache
    /// allowlist it mirrors.
    #[test]
    fn allowlist_matches_substep_cache() {
        assert_eq!(
            GATEWAY_TOOLS,
            &[
                "find_route_for",
                "preview_cost",
                "inspect_diff",
                "batch_savings"
            ]
        );
    }

    #[test]
    fn find_route_for_executes_pure_heuristic() {
        let out = execute(
            "find_route_for",
            r#"{"task_description":"classify this short text"}"#,
        )
        .unwrap();
        assert!(!out.is_empty()); // returns a model recommendation + rationale
        let v: Value = serde_json::from_str(&out).unwrap();
        // "classify ..." → Classification → haiku.
        assert_eq!(v["model"], "claude-haiku-4-5");
        assert!(v["rationale"].as_str().unwrap().contains("heuristic"));
    }

    #[test]
    fn unknown_tool_is_not_executable() {
        assert!(matches!(
            execute("write_file", "{}"),
            Err(GatewayToolError::NotExecutable(_))
        ));
    }

    #[test]
    fn bad_args_returns_error_text_not_panic() {
        // a gateway tool with unparseable args returns Ok(error_text), not Err/panic
        let out = execute("preview_cost", "not json").unwrap();
        assert!(out.to_lowercase().contains("error") || out.to_lowercase().contains("invalid"));
    }

    #[test]
    fn preview_cost_happy_path_returns_estimate() {
        // Minimal valid PreviewRequest: model + a one-message array (content is
        // a string per tt_preview::Message). Should produce a current estimate.
        let out = execute(
            "preview_cost",
            r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello there"}]}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["current"]["model"], "gpt-4o-mini");
        assert!(v["current"]["cost_usd"].is_number());
    }

    #[test]
    fn inspect_diff_unsupported_language_returns_reason() {
        let out = execute(
            "inspect_diff",
            r#"{"file_path":"notes.txt","proposed_content":"just prose"}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["scanned"], json!(false));
        assert_eq!(v["detected_language"], Value::Null);
    }

    #[test]
    fn inspect_diff_supported_language_scans() {
        let out = execute(
            "inspect_diff",
            r#"{"file_path":"mod.py","proposed_content":"x = 1\n"}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["scanned"], json!(true));
        assert_eq!(v["detected_language"], json!("python"));
        assert!(v["findings"].is_array());
    }

    #[test]
    fn batch_savings_projects_for_eligible_segment() {
        let out = execute(
            "batch_savings",
            r#"{"aggregates":[{"provider":"openai","model":"gpt-5.5","tag":"nightly","input_tokens":1000000,"output_tokens":1000000,"cost_usd":35.0,"request_count":10}]}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let findings = v["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["tag"], "nightly");
    }
}
