//! Agentic tool-calling for `tt chat`: a client-side loop that advertises the
//! stateless `tt-mcp` tools, executes the model's `tool_calls` locally, and
//! feeds results back until the model returns a text answer. Non-streamed.

use anyhow::Context as _;
use serde_json::{json, Value};

use tt_mcp::tools::batch_savings::BatchSavingsTool;
use tt_mcp::tools::find_route_for::FindRouteForTool;
use tt_mcp::tools::inspect_diff::InspectDiffTool;
use tt_mcp::tools::preview_cost::PreviewCostTool;
use tt_mcp::tools::Registry;
use tt_shared::messages::{Message, MessageContent, ToolCall};

use super::{
    format_turn_footer, format_unmeasured_turn_footer, shape, Conversation, Ledger, UsageInfo,
};
use crate::ui;

/// Hard cap on tool-call rounds per turn (loop guard).
const MAX_ROUNDS: usize = 6;

/// The 3 stateless, read-only tools `tt chat` exposes to the model.
#[must_use]
pub fn build_registry() -> Registry {
    let mut r = Registry::new();
    r.register(Box::new(FindRouteForTool));
    r.register(Box::new(PreviewCostTool));
    r.register(Box::new(InspectDiffTool));
    r
}

/// The advisor's tool surface: the chat tools plus `batch_savings`, which
/// projects Batch-API savings over request-log aggregates (Batch/Flex phase 1,
/// advisory only). Kept off the `tt chat` registry — it reasons over telemetry
/// aggregates the advisor assembles, not something a chat user supplies.
#[must_use]
pub fn build_advisor_registry() -> Registry {
    let mut r = build_registry();
    r.register(Box::new(BatchSavingsTool));
    r
}

/// Build the SDK `tools` from the registry's tool definitions.
fn registry_tools(reg: &Registry) -> Vec<tt_client::Tool> {
    reg.list()
        .into_iter()
        .map(|d| tt_client::tool(d.name, d.description, d.input_schema))
        .collect()
}

/// Build a `UsageInfo` from a non-streamed response. Missing raw components
/// remain explicit: this never derives a request-delta baseline from legacy
/// `saved_usd` compatibility data.
#[must_use]
pub fn usage_from_headers(
    cost: &tt_client::CostInfo,
    in_tok: u64,
    out_tok: u64,
) -> Option<UsageInfo> {
    UsageInfo::from_header_cost(cost, in_tok, out_tok)
}

/// The muted one-line preview shown when the model calls a tool.
#[must_use]
pub fn format_tool_call(name: &str, args: &str) -> String {
    let mut a: String = args.chars().take(80).collect();
    if a.len() < args.len() {
        a.push('…');
    }
    ui::muted()
        .apply_to(format!("{} {name}({a})", ui::ARROW))
        .to_string()
}

/// Per-turn cost accumulator. A tool turn spans several gateway calls but is one
/// user turn, so the rounds are summed and added to the session ledger once.
#[derive(Default)]
struct TurnTotals {
    cost: f64,
    baseline: f64,
    provider_cache_saved: f64,
    cache_bust: f64,
    summarizer_tax: f64,
    in_tok: u64,
    out_tok: u64,
    /// Successful gateway responses in this tool turn. Every one must carry a
    /// complete tuple before the turn can report a signed request delta.
    successful_rounds: u32,
    /// Successfully received cost/usage records. These alone may still be
    /// insufficient when a sibling successful round omitted raw accounting.
    billed_rounds: u32,
    measured_delta_rounds: u32,
    legacy_saved: f64,
    legacy_saved_rounds: u32,
    /// Estimated tokens of tool results + tool-call args before/after shaping
    /// (`chat::shape`). Token counts only — never converted to a USD claim.
    trim_before: u64,
    trim_after: u64,
}

impl TurnTotals {
    fn add_round(&mut self, usage: Option<&UsageInfo>) {
        self.successful_rounds += 1;
        let Some(u) = usage else {
            return;
        };
        self.cost += u.cost_usd;
        self.in_tok += u.input_tokens;
        self.out_tok += u.output_tokens;
        self.billed_rounds += 1;
        if let Some(saved_usd) = u.legacy_saved_usd {
            self.legacy_saved += saved_usd;
            self.legacy_saved_rounds += 1;
        }
        if let tt_client::RequestDeltaEstimate::Measured {
            baseline_cost_usd,
            provider_cache_saved_usd,
            cache_bust_usd,
            summarizer_tax_usd,
            ..
        } = u.request_delta_estimate
        {
            self.baseline += baseline_cost_usd;
            self.provider_cache_saved += provider_cache_saved_usd;
            self.cache_bust += cache_bust_usd;
            self.summarizer_tax += summarizer_tax_usd;
            self.measured_delta_rounds += 1;
        }
    }
    fn add_trim(&mut self, s: shape::ShapeStats) {
        self.trim_before += u64::from(s.tokens_before);
        self.trim_after += u64::from(s.tokens_after);
    }
    fn as_usage(&self) -> UsageInfo {
        let request_delta_estimate =
            if self.successful_rounds > 0 && self.measured_delta_rounds == self.successful_rounds {
                tt_client::RequestDeltaEstimate::from_components(
                    Some(self.baseline),
                    Some(self.cost),
                    Some(self.provider_cache_saved),
                    Some(self.cache_bust),
                    Some(self.summarizer_tax),
                )
            } else {
                tt_client::RequestDeltaEstimate::Unmeasured
            };
        UsageInfo {
            cost_usd: self.cost,
            cost_complete: self.billed_rounds == self.successful_rounds,
            baseline_cost_usd: match request_delta_estimate {
                tt_client::RequestDeltaEstimate::Measured {
                    baseline_cost_usd, ..
                } => Some(baseline_cost_usd),
                tt_client::RequestDeltaEstimate::Unmeasured => None,
            },
            legacy_saved_usd: (self.successful_rounds > 0
                && self.legacy_saved_rounds == self.successful_rounds)
                .then_some(self.legacy_saved),
            request_delta_estimate,
            input_tokens: self.in_tok,
            output_tokens: self.out_tok,
            cached_tokens: 0,
        }
    }
}

/// One round's parsed response.
struct Round {
    served_model: String,
    calls: Vec<ToolCall>,
    content: String,
    usage: Option<UsageInfo>,
    /// The typed assistant message the SDK returned (carries content +
    /// tool_calls), pushed to history verbatim when the round requests tools.
    assistant_msg: Option<Message>,
}

/// Send one non-streamed request through the SDK and parse it. `force_no_tools`
/// sets `tool_choice:"none"` so the model must answer with text — used to close
/// out a turn that hit the round cap.
async fn send_round(
    client: &tt_client::Client,
    conv: &Conversation,
    tools: &[tt_client::Tool],
    force_no_tools: bool,
) -> anyhow::Result<Round> {
    let mut builder = client
        .chat()
        .model(&conv.model)
        .messages(conv.wire_messages())
        // The /tools loop is non-streamed but a human is still waiting on it
        // (and `tt advise` reuses run_tool_turn): declare interactivity so the
        // gateway never marks the CLI's own traffic batch-eligible — a ≤24h
        // Batch-API window breaks any interactive UX. Enforced in code here,
        // not docs.
        .interactive()
        .tools(tools.to_vec());
    if force_no_tools {
        builder = builder.tool_choice(tt_client::ToolChoice::Auto("none".to_string()));
    }
    let out = builder.send().await.context("request to gateway failed")?;

    let served_model = out
        .cost
        .model_used
        .clone()
        .unwrap_or_else(|| conv.model.clone());
    let calls = out.tool_calls().to_vec();
    let content = out.text().unwrap_or_default().to_string();
    let usage = usage_from_headers(
        &out.cost,
        out.response.usage.prompt_tokens,
        out.response.usage.completion_tokens,
    );
    let assistant_msg = out.response.choices.first().map(|ch| ch.message.clone());
    Ok(Round {
        served_model,
        calls,
        content,
        usage,
        assistant_msg,
    })
}

/// Push the final assistant answer, print it + a single per-turn footer, and
/// record exactly one ledger turn for the whole (possibly multi-round) turn.
/// A `· tool-trim −N tok` segment is appended ONLY when shaping actually
/// removed tokens (built-in results are already minified, so an honest ~0
/// prints no claim at all).
fn finish_turn(
    conv: &mut Conversation,
    ledger: &mut Ledger,
    turn: &TurnTotals,
    served_model: &str,
    content: String,
) {
    println!("{content}");
    conv.push_assistant(content);
    let trimmed = turn.trim_before.saturating_sub(turn.trim_after);
    if trimmed > 0 {
        ledger.tool_trim_tokens += trimmed;
    }
    if turn.billed_rounds > 0 {
        let u = turn.as_usage();
        ledger.add(&u);
        let mut footer = format_turn_footer(served_model, &u);
        if trimmed > 0 {
            footer.push_str(
                &ui::muted()
                    .apply_to(format!(" · tool-trim −{trimmed} tok"))
                    .to_string(),
            );
        }
        println!("{footer}");
    } else if turn.successful_rounds > 0 {
        // A completed response with no parseable cost must not disappear from
        // the session or masquerade as a zero-cost turn.
        ledger.add_unmeasured_direct_turn();
        println!("{}", format_unmeasured_turn_footer(served_model));
    }
}

/// Run one chat turn with tools enabled: a non-streamed call/execute loop. On
/// success the conversation ALWAYS ends with a real assistant answer (even at
/// the round cap, where a final `tool_choice:"none"` request forces text), and
/// the whole turn is recorded as one ledger entry. On failure the conversation
/// is truncated back to its entry length (no partial tool messages), matching
/// `do_turn`'s contract so the caller's "pop the user on false" stays correct.
///
/// `shape_results` (default ON; `--no-tool-trim` / `/tools trim off` disable)
/// applies `chat::shape` to tool results + tool-call args ONCE, at the moment
/// they are appended to history — never re-applied to messages already in
/// `conv.messages` (shape-at-entry; re-shaping in place would be re-selection
/// inside the provider-cached prefix). `tt advise` inherits this via the same
/// fn — acceptable because all shaping is lossless or derived-field-only.
pub async fn run_tool_turn(
    client: &tt_client::Client,
    conv: &mut Conversation,
    reg: &Registry,
    ledger: &mut Ledger,
    shape_results: bool,
) -> bool {
    let start_len = conv.messages.len();
    let tools = registry_tools(reg);
    let mut turn = TurnTotals::default();

    for _round in 0..MAX_ROUNDS {
        let round = match send_round(client, conv, &tools, false).await {
            Ok(r) => r,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                conv.messages.truncate(start_len);
                return false;
            }
        };
        turn.add_round(round.usage.as_ref());
        if round.calls.is_empty() {
            let Round {
                served_model,
                content,
                ..
            } = round;
            finish_turn(conv, ledger, &turn, &served_model, content);
            return true;
        }

        // push the assistant message the SDK returned (already typed, carrying
        // any accompanying text + the tool_calls); args are minified on the
        // history copy only — `format_tool_call` below previews the original.
        if let Some(mut m) = round.assistant_msg.clone() {
            if shape_results {
                turn.add_trim(shape::minify_tool_call_args(&mut m));
            }
            conv.messages.push(m);
        }
        for tc in &round.calls {
            println!(
                "{}",
                format_tool_call(&tc.function.name, &tc.function.arguments)
            );
            let args: Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or_else(|_| json!({}));
            let out = match reg.call(&tc.function.name, args).await {
                Ok(v) => v,
                Err(e) => json!({ "error": e.to_string() }),
            };
            // shape-at-entry: trim the result once, as it enters history
            let out_str = if shape_results {
                let (s, stats) = shape::shape_tool_result(&tc.function.name, out);
                turn.add_trim(stats);
                s
            } else {
                out.to_string()
            };
            let preview: String = out_str.chars().take(120).collect();
            println!("{}", ui::muted().apply_to(format!("  {preview}")));
            conv.messages.push(Message::Tool {
                content: MessageContent::Text(out_str),
                tool_call_id: tc.id.clone(),
            });
        }
    }

    // Round cap hit: force a final text answer so the turn never ends on a
    // dangling tool result.
    ui::warn("tool loop hit the round cap — requesting a final answer");
    match send_round(client, conv, &tools, true).await {
        Ok(round) => {
            turn.add_round(round.usage.as_ref());
            let Round {
                served_model,
                content,
                ..
            } = round;
            let content = if content.is_empty() {
                "(no answer produced after the tool-call limit)".to_string()
            } else {
                content
            };
            finish_turn(conv, ledger, &turn, &served_model, content);
            true
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            conv.messages.truncate(start_len);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_headers_never_derives_a_delta_from_legacy_saved() {
        let legacy_only = tt_client::CostInfo {
            cost_usd: Some(0.001),
            saved_usd: Some(0.003),
            provider_cache_saved_usd: Some(0.0),
            cache_bust_usd: Some(0.0),
            summarizer_tax_usd: Some(0.0),
            ..tt_client::CostInfo::default()
        };
        let u = usage_from_headers(&legacy_only, 10, 20).expect("known served cost");
        assert_eq!(u.baseline_cost_usd, None);
        assert_eq!(u.legacy_saved_usd, Some(0.003));
        assert_eq!(
            u.request_delta_estimate,
            tt_client::RequestDeltaEstimate::Unmeasured
        );
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 20);

        let complete = tt_client::CostInfo {
            cost_usd: Some(0.001),
            baseline_cost_usd: Some(0.0015),
            provider_cache_saved_usd: Some(0.0002),
            cache_bust_usd: Some(0.0001),
            summarizer_tax_usd: Some(0.0003),
            ..tt_client::CostInfo::default()
        };
        let u2 = usage_from_headers(&complete, 1, 1).expect("known served cost");
        assert!(matches!(
            u2.request_delta_estimate,
            tt_client::RequestDeltaEstimate::Measured { signed_usd, regression_usd, .. }
                if (signed_usd + 0.0001).abs() < 1e-12
                    && (regression_usd - 0.0001).abs() < 1e-12
        ));
    }

    #[test]
    fn turn_totals_marks_partial_or_missing_successful_rounds_unmeasured() {
        let complete = usage_from_headers(
            &tt_client::CostInfo {
                cost_usd: Some(0.0001),
                baseline_cost_usd: Some(0.0004),
                provider_cache_saved_usd: Some(0.0001),
                cache_bust_usd: Some(0.0),
                summarizer_tax_usd: Some(0.0),
                ..tt_client::CostInfo::default()
            },
            10,
            2,
        )
        .expect("known served cost");
        let partial = usage_from_headers(
            &tt_client::CostInfo {
                cost_usd: Some(0.0001),
                baseline_cost_usd: Some(0.0004),
                provider_cache_saved_usd: Some(0.0001),
                cache_bust_usd: Some(0.0),
                // Missing one raw tuple member must not be zero-filled.
                summarizer_tax_usd: None,
                ..tt_client::CostInfo::default()
            },
            8,
            1,
        )
        .expect("known served cost");

        let mut partial_turn = TurnTotals::default();
        partial_turn.add_round(Some(&complete));
        partial_turn.add_round(Some(&partial));
        let combined = partial_turn.as_usage();
        assert_eq!(
            combined.request_delta_estimate,
            tt_client::RequestDeltaEstimate::Unmeasured
        );
        assert!((combined.cost_usd - 0.0002).abs() < 1e-12);
        // Missing delta components do not erase the fact that both served
        // costs were received, so the cost total remains complete even though
        // the signed delta is unmeasured.
        assert!(combined.cost_complete);

        // A successful response without a parseable served cost is also an
        // accounted round, so it poisons the whole turn's signed estimate.
        let mut missing_cost_turn = TurnTotals::default();
        missing_cost_turn.add_round(Some(&complete));
        missing_cost_turn.add_round(None);
        let combined = missing_cost_turn.as_usage();
        assert_eq!(
            combined.request_delta_estimate,
            tt_client::RequestDeltaEstimate::Unmeasured
        );
        assert!((combined.cost_usd - 0.0001).abs() < 1e-12);
        assert!(!combined.cost_complete);
    }

    #[test]
    fn format_tool_call_has_name_and_truncates() {
        console::set_colors_enabled(false);
        let line = format_tool_call("find_route_for", "{\"task_description\":\"x\"}");
        assert!(line.contains("find_route_for"), "{line}");
        let long = "a".repeat(200);
        let l2 = format_tool_call("preview_cost", &long);
        assert!(l2.contains('…'), "should truncate: {l2}");
    }

    use httpmock::prelude::*;

    #[tokio::test]
    async fn tool_loop_executes_then_answers() {
        let server = MockServer::start_async().await;
        // Define the MORE SPECIFIC mock first: round 2's request carries a tool
        // result (`"role":"tool"`) — it returns the final text answer.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_includes("\"role\":\"tool\"")
                // Every /tools round declares interactivity (a human is
                // waiting) — and `tt advise` inherits this via run_tool_turn:
                // the CLI's own traffic is never batch-eligible, in code.
                .header("x-tokentrimmer-interactive", "1");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-saved-usd", "0.0003")
                .header("x-tokentrimmer-baseline-cost-usd", "0.0004")
                .header("x-tokentrimmer-provider-cache-saved-usd", "0.0001")
                .header("x-tokentrimmer-cache-bust-usd", "0.0")
                .header("x-tokentrimmer-summarizer-tax-usd", "0.0")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Use Haiku." } }],
                    "usage": { "prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16 }
                }));
        });
        // Round 1 (no tool result yet): the broad mock returns a tool_call.
        // Also pinned on the interactive header — round 1 declares it too.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("x-tokentrimmer-interactive", "1");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "c1", "type": "function",
                            "function": { "name": "find_route_for",
                                "arguments": "{\"task_description\":\"classify sentiment\"}" } }]
                    }}],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
                }));
        });

        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("what model for sentiment?".into());
        let reg = build_registry();
        let mut ledger = Ledger::default();
        let client = tt_client::Client::new(server.base_url(), "k");

        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger, true).await;
        assert!(ok);
        // [User, Assistant(tool_calls), Tool(result), Assistant("Use Haiku.")]
        assert_eq!(conv.messages.len(), 4);
        assert!(
            matches!(conv.messages[1], Message::Assistant { ref tool_calls, .. } if !tool_calls.is_empty())
        );
        assert!(
            matches!(&conv.messages[2], Message::Tool { content: MessageContent::Text(t), .. } if t.contains("model"))
        );
        assert!(
            matches!(&conv.messages[3], Message::Assistant { content: Some(MessageContent::Text(t)), .. } if t == "Use Haiku.")
        );
        // The final response is fully measurable, but the preceding successful
        // response lacked a parseable cost. That makes the whole tool turn
        // unmeasured rather than silently aggregating the final round alone.
        assert_eq!(ledger.turns, 1);
        assert!((ledger.cost_usd - 0.0001).abs() < 1e-12);
        assert_eq!(ledger.measured_request_deltas, 0);
        assert_eq!(ledger.unmeasured_request_deltas, 1);
        assert_eq!(ledger.unknown_direct_cost_turns, 1);
    }

    #[tokio::test]
    async fn tool_loop_with_no_cost_headers_records_an_unmeasured_turn() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_includes("\"role\":\"tool\"");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "No cost headers." } }],
                    "usage": { "prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16 }
                }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "c1", "type": "function",
                            "function": { "name": "find_route_for",
                                "arguments": "{\"task_description\":\"classify sentiment\"}" } }]
                    }}],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
                }));
        });

        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("what model for sentiment?".into());
        let reg = build_registry();
        let mut ledger = Ledger::default();
        let client = tt_client::Client::new(server.base_url(), "k");

        assert!(run_tool_turn(&client, &mut conv, &reg, &mut ledger, true).await);
        assert_eq!(ledger.turns, 1);
        assert_eq!(ledger.cost_usd, 0.0, "unknown cost is never zero-filled");
        assert_eq!(ledger.unmeasured_request_deltas, 1);
        assert_eq!(ledger.unknown_direct_cost_turns, 1);
        let summary = ledger.summary();
        assert!(summary.contains("known spend"), "{summary}");
        assert!(summary.contains("cost not measured"), "{summary}");
    }

    #[tokio::test]
    async fn tool_loop_forces_answer_at_round_cap() {
        let server = MockServer::start_async().await;
        // The forced final request (tool_choice:"none") returns a text answer.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_includes("\"tool_choice\":\"none\"");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-saved-usd", "0.0002")
                .header("x-tokentrimmer-baseline-cost-usd", "0.0004")
                .header("x-tokentrimmer-provider-cache-saved-usd", "0.0001")
                .header("x-tokentrimmer-cache-bust-usd", "0.0")
                .header("x-tokentrimmer-summarizer-tax-usd", "0.0")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Final answer." } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                }));
        });
        // Every normal request keeps requesting a tool (never converges); each
        // round carries a complete accounting tuple so we can prove N rounds →
        // 1 measured ledger turn.
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-saved-usd", "0.0002")
                .header("x-tokentrimmer-baseline-cost-usd", "0.0004")
                .header("x-tokentrimmer-provider-cache-saved-usd", "0.0001")
                .header("x-tokentrimmer-cache-bust-usd", "0.0")
                .header("x-tokentrimmer-summarizer-tax-usd", "0.0")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "c1", "type": "function",
                            "function": { "name": "find_route_for",
                                "arguments": "{\"task_description\":\"loop\"}" } }]
                    }}],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                }));
        });

        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("go".into());
        let reg = build_registry();
        let mut ledger = Ledger::default();
        let client = tt_client::Client::new(server.base_url(), "k");

        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger, true).await;
        assert!(ok);
        // Must end with a real assistant answer, never a dangling Tool message.
        assert!(
            matches!(conv.messages.last(), Some(Message::Assistant { content: Some(MessageContent::Text(t)), .. }) if t == "Final answer."),
            "last = {:?}",
            conv.messages.last()
        );
        // One user turn → exactly one ledger turn, despite the 7 billed rounds
        // (6 tool rounds + the forced final), whose cost is summed.
        assert_eq!(ledger.turns, 1);
        assert!(
            (ledger.cost_usd - 0.0007).abs() < 1e-9,
            "cost = {}",
            ledger.cost_usd
        );
        assert_eq!(ledger.measured_request_deltas, 1);
        assert_eq!(ledger.unmeasured_request_deltas, 0);
        assert!(
            (ledger.signed_request_delta_usd - 0.0014).abs() < 1e-12,
            "request delta = {}",
            ledger.signed_request_delta_usd
        );
    }

    /// Model emits PRETTY-printed tool-call args (whitespace = machine noise):
    /// the history copy is minified, the delta metered in the ledger as token
    /// counts (never USD), and the loop still converges as before.
    #[tokio::test]
    async fn tool_loop_meters_trim_tokens() {
        console::set_colors_enabled(false);
        let server = MockServer::start_async().await;
        let pretty_args =
            "{\n    \"task_description\" :   \"classify the sentiment of customer tweets\"\n}";
        // round 2 (sees the tool result) → final text answer
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_includes("\"role\":\"tool\"");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-saved-usd", "0.0003")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Use Haiku." } }],
                    "usage": { "prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16 }
                }));
        });
        // round 1 → a tool call with pretty-printed JSON args
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "c1", "type": "function",
                            "function": { "name": "find_route_for", "arguments": pretty_args } }]
                    }}],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
                }));
        });

        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("what model for sentiment?".into());
        let reg = build_registry();
        let mut ledger = Ledger::default();
        let client = tt_client::Client::new(server.base_url(), "k");

        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger, true).await;
        assert!(ok);
        // the HISTORY copy of the args is minified (value-identical)
        let Message::Assistant { tool_calls, .. } = &conv.messages[1] else {
            panic!("expected assistant tool-call message");
        };
        assert!(
            !tool_calls[0].function.arguments.contains('\n'),
            "args not minified: {:?}",
            tool_calls[0].function.arguments
        );
        assert_eq!(
            serde_json::from_str::<Value>(&tool_calls[0].function.arguments).unwrap(),
            serde_json::from_str::<Value>(pretty_args).unwrap()
        );
        // the trim is metered (tokens only) in the session ledger
        assert!(ledger.tool_trim_tokens > 0, "{}", ledger.tool_trim_tokens);
        assert_eq!(ledger.turns, 1);
        let s = ledger.summary();
        assert!(s.contains("tool-trim"), "{s}");
        assert!(s.contains("(est.)"), "{s}");
    }

    /// `--no-tool-trim` / `/tools trim off`: history keeps the model's bytes
    /// verbatim and nothing is metered.
    #[tokio::test]
    async fn tool_loop_trim_off_keeps_args_verbatim() {
        let server = MockServer::start_async().await;
        let pretty_args = "{\n  \"task_description\": \"x\"\n}";
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_includes("\"role\":\"tool\"");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                }));
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "c1", "type": "function",
                            "function": { "name": "find_route_for", "arguments": pretty_args } }]
                    }}],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                }));
        });
        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("go".into());
        let reg = build_registry();
        let mut ledger = Ledger::default();
        let client = tt_client::Client::new(server.base_url(), "k");
        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger, false).await;
        assert!(ok);
        let Message::Assistant { tool_calls, .. } = &conv.messages[1] else {
            panic!("expected assistant tool-call message");
        };
        assert_eq!(tool_calls[0].function.arguments, pretty_args);
        assert_eq!(ledger.tool_trim_tokens, 0);
    }

    #[tokio::test]
    async fn tool_loop_rolls_back_on_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(500).body("boom");
        });
        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("hi".into());
        let start = conv.messages.len();
        let reg = build_registry();
        let mut ledger = Ledger::default();
        let client = tt_client::Client::new(server.base_url(), "k");
        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger, true).await;
        assert!(!ok);
        assert_eq!(
            conv.messages.len(),
            start,
            "history must be unchanged on failure"
        );
    }

    #[test]
    fn registry_tools_advertises_three_tools() {
        let reg = build_registry();
        let t = registry_tools(&reg);
        assert_eq!(t.len(), 3);
        let names: Vec<&str> = t.iter().map(|x| x.function.name.as_str()).collect();
        assert!(names.contains(&"find_route_for"));
        assert!(names.contains(&"preview_cost"));
        assert!(names.contains(&"inspect_diff"));
        // schema carried through
        let fr = t
            .iter()
            .find(|x| x.function.name == "find_route_for")
            .unwrap();
        assert_eq!(fr.function.parameters["required"][0], "task_description");
        // batch_savings is advisor-only — not on the chat registry.
        assert!(
            !names.contains(&"batch_savings"),
            "batch_savings must not be on the chat registry"
        );
    }

    /// The advisor registry is the chat tools plus `batch_savings`, so the
    /// model can flag batch-eligible request-log traffic and project savings.
    #[test]
    fn advisor_registry_adds_batch_savings() {
        let reg = build_advisor_registry();
        let t = registry_tools(&reg);
        let names: Vec<&str> = t.iter().map(|x| x.function.name.as_str()).collect();
        assert!(names.contains(&"batch_savings"), "{names:?}");
        // and still carries the chat tools.
        assert!(names.contains(&"find_route_for"));
        assert!(names.contains(&"preview_cost"));
        assert!(names.contains(&"inspect_diff"));
        assert_eq!(t.len(), 4);
    }
}
