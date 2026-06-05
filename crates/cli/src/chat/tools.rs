//! Agentic tool-calling for `tt chat`: a client-side loop that advertises the
//! stateless `tt-mcp` tools, executes the model's `tool_calls` locally, and
//! feeds results back until the model returns a text answer. Non-streamed.

use anyhow::Context as _;
use serde_json::{json, Value};

use tt_mcp::tools::find_route_for::FindRouteForTool;
use tt_mcp::tools::inspect_diff::InspectDiffTool;
use tt_mcp::tools::preview_cost::PreviewCostTool;
use tt_mcp::tools::Registry;
use tt_shared::messages::{Message, MessageContent, ToolCall, ToolCallFunction};

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

/// Build the OpenAI `tools` array from the registry's tool definitions.
#[must_use]
pub fn tools_json(reg: &Registry) -> Vec<Value> {
    reg.list()
        .into_iter()
        .map(|d| {
            json!({
                "type": "function",
                "function": {
                    "name": d.name,
                    "description": d.description,
                    "parameters": d.input_schema,
                }
            })
        })
        .collect()
}

/// Extract `ToolCall`s from a response `message` (`choices[0].message`).
/// Entries missing an `id` or function `name` are skipped.
#[must_use]
pub fn parse_tool_calls(message: &Value) -> Vec<ToolCall> {
    message["tool_calls"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    Some(ToolCall {
                        id: tc["id"].as_str()?.to_string(),
                        r#type: tc["type"].as_str().unwrap_or("function").to_string(),
                        function: ToolCallFunction {
                            name: tc["function"]["name"].as_str()?.to_string(),
                            arguments: tc["function"]["arguments"]
                                .as_str()
                                .unwrap_or("{}")
                                .to_string(),
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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

fn header_f64(h: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
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
}

/// Send one non-streamed request and parse it. `force_no_tools` sets
/// `tool_choice:"none"` so the model must answer with text — used to close out a
/// turn that hit the round cap.
async fn send_round(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    conv: &Conversation,
    tools: &[Value],
    force_no_tools: bool,
) -> anyhow::Result<Round> {
    let mut body = json!({
        "model": conv.model,
        "messages": conv.wire_messages(),
        "tools": tools,
        "stream": false,
    });
    if force_no_tools {
        body["tool_choice"] = json!("none");
    }
    let resp = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .context("request to gateway failed")?;
    let served_model = resp
        .headers()
        .get("x-tokentrimmer-model-used")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&conv.model)
        .to_string();
    let cost = header_f64(resp.headers(), "x-tokentrimmer-cost-usd");
    let saved = header_f64(resp.headers(), "x-tokentrimmer-saved-usd").unwrap_or(0.0);
    let baseline = header_f64(resp.headers(), "x-tokentrimmer-baseline-cost-usd");
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("gateway returned {status}: {}", text.trim());
    }
    let v: Value = resp.json().await.context("invalid gateway response")?;
    let message = &v["choices"][0]["message"];
    let calls = parse_tool_calls(message);
    let content = message["content"].as_str().unwrap_or_default().to_string();
    let in_tok = v["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let out_tok = v["usage"]["completion_tokens"].as_u64().unwrap_or(0);
    let usage = cost.map(|c| usage_from_parts(c, saved, baseline, in_tok, out_tok));
    Ok(Round {
        served_model,
        calls,
        content,
        usage,
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
    http: &reqwest::Client,
    base: &str,
    key: &str,
    conv: &mut Conversation,
    reg: &Registry,
    ledger: &mut Ledger,
) -> bool {
    let start_len = conv.messages.len();
    let tools = tools_json(reg);
    let mut turn = TurnTotals::default();

    for _round in 0..MAX_ROUNDS {
        let round = match send_round(http, base, key, conv, &tools, false).await {
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

        // assistant turn that requests tools — preserve any accompanying text
        let content =
            (!round.content.is_empty()).then(|| MessageContent::Text(round.content.clone()));
        conv.messages.push(Message::Assistant {
            content,
            tool_calls: round.calls.clone(),
            name: None,
        });
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
    match send_round(http, base, key, conv, &tools, true).await {
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
    fn parse_tool_calls_extracts_and_skips_malformed() {
        let msg = json!({
            "tool_calls": [
                { "id": "call_1", "type": "function",
                  "function": { "name": "find_route_for", "arguments": "{\"task_description\":\"sort a list\"}" } },
                { "type": "function", "function": { "name": "nope" } } // no id → skipped
            ]
        });
        let calls = parse_tool_calls(&msg);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "find_route_for");
        assert!(calls[0].function.arguments.contains("sort a list"));
        // no tool_calls field → empty
        assert!(parse_tool_calls(&json!({"content": "hi"})).is_empty());
    }

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
                    "choices": [{ "message": { "role": "assistant", "content": "Use Haiku." } }],
                    "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
                }));
        });
        // Round 1 (no tool result yet): the broad mock returns a tool_call.
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .json_body(json!({
                    "choices": [{ "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "c1", "type": "function",
                            "function": { "name": "find_route_for",
                                "arguments": "{\"task_description\":\"classify sentiment\"}" } }]
                    }}],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 1 }
                }));
        });

        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("what model for sentiment?".into());
        let reg = build_registry();
        let mut ledger = Ledger::default();
        let http = reqwest::Client::new();

        let ok = run_tool_turn(&http, &server.base_url(), "k", &mut conv, &reg, &mut ledger).await;
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
                    "choices": [{ "message": { "role": "assistant", "content": "Final answer." } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
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
                    "choices": [{ "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "c1", "type": "function",
                            "function": { "name": "find_route_for",
                                "arguments": "{\"task_description\":\"loop\"}" } }]
                    }}],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                }));
        });

        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("go".into());
        let reg = build_registry();
        let mut ledger = Ledger::default();
        let http = reqwest::Client::new();

        let ok = run_tool_turn(&http, &server.base_url(), "k", &mut conv, &reg, &mut ledger).await;
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
        let http = reqwest::Client::new();
        let ok = run_tool_turn(&http, &server.base_url(), "k", &mut conv, &reg, &mut ledger).await;
        assert!(!ok);
        assert_eq!(
            conv.messages.len(),
            start,
            "history must be unchanged on failure"
        );
    }

    #[test]
    fn tools_json_advertises_three_tools() {
        let reg = build_registry();
        let t = tools_json(&reg);
        assert_eq!(t.len(), 3);
        let names: Vec<&str> = t
            .iter()
            .map(|v| v["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"find_route_for"));
        assert!(names.contains(&"preview_cost"));
        assert!(names.contains(&"inspect_diff"));
        // schema carried through
        let fr = t
            .iter()
            .find(|v| v["function"]["name"] == "find_route_for")
            .unwrap();
        assert_eq!(
            fr["function"]["parameters"]["required"][0],
            "task_description"
        );
    }
}
