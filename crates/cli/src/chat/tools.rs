//! Agentic tool-calling for `tt chat`: a client-side loop that advertises the
//! stateless `tt-mcp` tools, executes the model's `tool_calls` locally, and
//! feeds results back until the model returns a text answer. Non-streamed.

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

/// Build a `UsageInfo` from a non-streamed response: cost/saved come from the
/// gateway headers, tokens from the body's `usage`; baseline = cost + saved.
#[must_use]
pub fn usage_from_parts(cost_usd: f64, saved_usd: f64, in_tok: u64, out_tok: u64) -> UsageInfo {
    UsageInfo {
        cost_usd,
        baseline_cost_usd: cost_usd + saved_usd,
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

/// Run one chat turn with tools enabled: a non-streamed call/execute loop.
/// Returns `true` on success. On any failure the conversation is truncated
/// back to its entry length (no partial tool messages), matching `do_turn`'s
/// contract so the caller's "pop the user on false" stays correct.
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
    for _round in 0..MAX_ROUNDS {
        let body = json!({
            "model": conv.model,
            "messages": conv.wire_messages(),
            "tools": tools,
            "stream": false,
        });
        let resp = match http
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                ui::error(&format!("request to gateway failed: {e}"));
                conv.messages.truncate(start_len);
                return false;
            }
        };
        let served_model = resp
            .headers()
            .get("x-tokentrimmer-model-used")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(&conv.model)
            .to_string();
        let cost = header_f64(resp.headers(), "x-tokentrimmer-cost-usd");
        let saved = header_f64(resp.headers(), "x-tokentrimmer-saved-usd").unwrap_or(0.0);
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            ui::error(&format!("gateway returned {status}: {}", text.trim()));
            conv.messages.truncate(start_len);
            return false;
        }
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                ui::error(&format!("invalid gateway response: {e}"));
                conv.messages.truncate(start_len);
                return false;
            }
        };
        let message = &v["choices"][0]["message"];
        let calls = parse_tool_calls(message);
        let in_tok = v["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let out_tok = v["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        let usage = cost.map(|c| usage_from_parts(c, saved, in_tok, out_tok));
        if let Some(u) = &usage {
            ledger.add(u);
        }

        if calls.is_empty() {
            let content = message["content"].as_str().unwrap_or_default();
            println!("{content}");
            conv.push_assistant(content.to_string());
            if let Some(u) = &usage {
                println!(
                    "{}",
                    format_turn_footer(
                        &served_model,
                        u.input_tokens,
                        u.output_tokens,
                        u.cost_usd,
                        u.saved_usd,
                        u.baseline_cost_usd
                    )
                );
            }
            return true;
        }

        // assistant turn that requests tools — preserve any accompanying text
        let content = message["content"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| MessageContent::Text(s.to_string()));
        conv.messages.push(Message::Assistant {
            content,
            tool_calls: calls.clone(),
            name: None,
        });
        for tc in &calls {
            println!(
                "{}",
                format_tool_call(&tc.function.name, &tc.function.arguments)
            );
            let args: Value = serde_json::from_str(&tc.function.arguments).unwrap_or_else(|_| json!({}));
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
    ui::warn("tool loop hit the round cap");
    true
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
    fn usage_baseline_is_cost_plus_saved() {
        let u = usage_from_parts(0.001, 0.003, 10, 20);
        assert!((u.baseline_cost_usd - 0.004).abs() < 1e-9);
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 20);
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
