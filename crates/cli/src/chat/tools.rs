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

use super::{format_turn_footer, Conversation, Ledger, UsageInfo};
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

/// Build a `UsageInfo` from a non-streamed response: cost/saved/baseline come
/// from the gateway headers, tokens from the body's `usage`. Falls back to
/// `cost + saved` when the gateway sends no baseline header.
#[must_use]
pub fn usage_from_parts(
    cost_usd: f64,
    saved_usd: f64,
    baseline_usd: Option<f64>,
    in_tok: u64,
    out_tok: u64,
) -> UsageInfo {
    UsageInfo {
        cost_usd,
        baseline_cost_usd: baseline_usd.unwrap_or(cost_usd + saved_usd),
        saved_usd,
        input_tokens: in_tok,
        output_tokens: out_tok,
        cached_tokens: 0,
    }
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
    saved: f64,
    baseline: f64,
    in_tok: u64,
    out_tok: u64,
    billed_rounds: u32,
}

impl TurnTotals {
    fn add(&mut self, u: &UsageInfo) {
        self.cost += u.cost_usd;
        self.saved += u.saved_usd;
        self.baseline += u.baseline_cost_usd;
        self.in_tok += u.input_tokens;
        self.out_tok += u.output_tokens;
        self.billed_rounds += 1;
    }
    fn as_usage(&self) -> UsageInfo {
        UsageInfo {
            cost_usd: self.cost,
            baseline_cost_usd: self.baseline,
            saved_usd: self.saved,
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
    let usage = out.cost.cost_usd.map(|c| {
        usage_from_parts(
            c,
            out.cost.saved_usd.unwrap_or(0.0),
            out.cost.baseline_cost_usd,
            out.response.usage.prompt_tokens,
            out.response.usage.completion_tokens,
        )
    });
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
fn finish_turn(
    conv: &mut Conversation,
    ledger: &mut Ledger,
    turn: &TurnTotals,
    served_model: &str,
    content: String,
) {
    println!("{content}");
    conv.push_assistant(content);
    if turn.billed_rounds > 0 {
        let u = turn.as_usage();
        ledger.add(&u);
        println!(
            "{}",
            format_turn_footer(
                served_model,
                u.input_tokens,
                u.output_tokens,
                u.cost_usd,
                u.saved_usd,
                u.baseline_cost_usd
            )
        );
    }
}

/// Run one chat turn with tools enabled: a non-streamed call/execute loop. On
/// success the conversation ALWAYS ends with a real assistant answer (even at
/// the round cap, where a final `tool_choice:"none"` request forces text), and
/// the whole turn is recorded as one ledger entry. On failure the conversation
/// is truncated back to its entry length (no partial tool messages), matching
/// `do_turn`'s contract so the caller's "pop the user on false" stays correct.
pub async fn run_tool_turn(
    client: &tt_client::Client,
    conv: &mut Conversation,
    reg: &Registry,
    ledger: &mut Ledger,
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
        if let Some(u) = &round.usage {
            turn.add(u);
        }
        if round.calls.is_empty() {
            let Round {
                served_model,
                content,
                ..
            } = round;
            finish_turn(conv, ledger, &turn, &served_model, content);
            return true;
        }

        // push the assistant message the SDK returned verbatim (already typed,
        // carrying any accompanying text + the tool_calls)
        if let Some(m) = round.assistant_msg.clone() {
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
            let out_str = out.to_string();
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
            if let Some(u) = &round.usage {
                turn.add(u);
            }
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
    fn usage_baseline_from_header_or_derived() {
        // no baseline header → cost + saved
        let u = usage_from_parts(0.001, 0.003, None, 10, 20);
        assert!((u.baseline_cost_usd - 0.004).abs() < 1e-9);
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 20);
        // explicit (authoritative) baseline header wins
        let u2 = usage_from_parts(0.001, 0.003, Some(0.005), 1, 1);
        assert!((u2.baseline_cost_usd - 0.005).abs() < 1e-9);
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
                .body_contains("\"role\":\"tool\"");
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
        // Round 1 (no tool result yet): the broad mock returns a tool_call.
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

        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger).await;
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
        assert_eq!(ledger.turns, 1); // only the final round carried cost headers
    }

    #[tokio::test]
    async fn tool_loop_forces_answer_at_round_cap() {
        let server = MockServer::start_async().await;
        // The forced final request (tool_choice:"none") returns a text answer.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"tool_choice\":\"none\"");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-saved-usd", "0.0002")
                .json_body(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Final answer." } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                }));
        });
        // Every normal request keeps requesting a tool (never converges); each
        // round carries cost headers so we can prove N rounds → 1 ledger turn.
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-saved-usd", "0.0002")
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

        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger).await;
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
        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger).await;
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
