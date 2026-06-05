# V5b-2 — `tt chat` Agentic Tool-Calling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in (`/tools`) client-side tool-call loop to `tt chat` that lets the model call the 3 stateless `tt-mcp` tools (`find_route_for`, `preview_cost`, `inspect_diff`), executing them locally and feeding results back until the model answers.

**Architecture:** New submodule `crates/cli/src/chat/tools.rs` holds the registry, request/response helpers, and the non-streamed `run_tool_turn` loop. `mod.rs` gains a `/tools` toggle, a `tt_mcp::tools::Registry` + `tools_enabled` state, and a `dispatch_turn` that routes Chat/Editor/Retry to `run_tool_turn` (tools on) or the existing streamed `do_turn` (off). `main.rs` adds `--tools`.

**Tech Stack:** Rust, `tt-mcp` (already a dep — `Registry`/`Tool`/the 3 tool structs), `tt-shared` `Message`/`ToolCall`, `reqwest` (non-streamed), `httpmock` (dev-dep, loop integration test).

---

### Task 1: `tools.rs` submodule — `build_registry` + `tools_json`

**Files:**
- Create: `crates/cli/src/chat/tools.rs`
- Modify: `crates/cli/src/chat/mod.rs` (add `pub mod tools;`)

- [ ] **Step 1: Create the file with the two functions**

Create `crates/cli/src/chat/tools.rs`:

```rust
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

#[cfg(test)]
mod tests {
    use super::*;

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
```

- [ ] **Step 2: Register the submodule**

In `crates/cli/src/chat/mod.rs`, add after the existing `pub mod session;` line:

```rust
pub mod tools;
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p tt-cli --lib chat::tools 2>&1 | tail -15`
Expected: PASS (`tools_json_advertises_three_tools`). (A dead-code warning on later-unused items is fine until Task 3 wires them.)

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/chat/tools.rs crates/cli/src/chat/mod.rs
git commit -m "feat(chat): tool registry + tools_json for agentic chat"
```

---

### Task 2: Response helpers — `parse_tool_calls`, `usage_from_parts`, `format_tool_call`

**Files:**
- Modify: `crates/cli/src/chat/tools.rs`

- [ ] **Step 1: Write the failing tests**

In `tools.rs` `mod tests`, add:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tt-cli --lib chat::tools 2>&1 | tail -15`
Expected: FAIL to compile — `cannot find function parse_tool_calls` / `usage_from_parts` / `format_tool_call`.

- [ ] **Step 3: Add the helpers**

In `tools.rs`, add after `tools_json` (before `#[cfg(test)]`):

```rust
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
```

- [ ] **Step 4: Add the `console` import for the test**

The `format_tool_call` test calls `console::set_colors_enabled`. Confirm `tools.rs` can see `console` (it's a direct dep). No import needed at module top (path-qualified in the test). If the test fails to resolve `console`, add `use console;` is NOT needed — `console::` works as an extern crate path.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p tt-cli --lib chat::tools 2>&1 | tail -15`
Expected: PASS (4 tests in `chat::tools`).

- [ ] **Step 6: Commit**

```bash
git add crates/cli/src/chat/tools.rs
git commit -m "feat(chat): tool-call response + display helpers"
```

---

### Task 3: `run_tool_turn` loop + `/tools` wiring (everything goes live)

**Files:**
- Modify: `crates/cli/src/chat/tools.rs` (the loop)
- Modify: `crates/cli/src/chat/mod.rs` (`Command::Tools`, `dispatch_turn`, `run` state + arms + help + heading)

> The loop and all its wiring land together so `run_tool_turn` is called from the main build (no dead code).

- [ ] **Step 1: Add `run_tool_turn` (+ a header helper) to `tools.rs`**

Add before `#[cfg(test)]`:

```rust
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
            println!("{}", format_tool_call(&tc.function.name, &tc.function.arguments));
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
```

- [ ] **Step 2: Build (loop compiles; still unused until wired below)**

Run: `cargo build -p tt-cli 2>&1 | grep -E "error" | head`
Expected: no `error` lines (a `dead_code`/`never used` warning for `run_tool_turn` is expected until Step 6).

- [ ] **Step 3: Add the `Command::Tools` variant + parse (mod.rs)**

In `enum Command`, add after `Copy,`:

```rust
    Tools(Option<bool>),
```

In `Command::parse`, add before `other => Command::Unknown(...)`:

```rust
            "tools" => Command::Tools(match arg.as_deref() {
                Some("on") | Some("true") | Some("enable") => Some(true),
                Some("off") | Some("false") | Some("disable") => Some(false),
                _ => None,
            }),
```

- [ ] **Step 4: Add `dispatch_turn` (mod.rs)**

Add immediately after the `do_turn` function:

```rust
/// Route a turn to the tool-calling loop (tools on) or the streamed path (off).
async fn dispatch_turn(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    conv: &mut Conversation,
    ledger: &mut Ledger,
    reg: &tt_mcp::tools::Registry,
    tools_enabled: bool,
) -> bool {
    if tools_enabled {
        tools::run_tool_turn(http, base, key, conv, reg, ledger).await
    } else {
        do_turn(http, base, key, conv, ledger).await
    }
}
```

- [ ] **Step 5: Thread registry + `tools_enabled` into `run` (mod.rs)**

Change the `run` signature to add a `tools` param (after `resume`):

```rust
pub async fn run(
    model: Option<String>,
    system: Option<String>,
    resume: Option<String>,
    tools: bool,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
```

After `let mut ledger = Ledger::default();`, add:

```rust
    let registry = tools::build_registry();
    let mut tools_enabled = tools;
```

Change the heading to reflect tool state:

```rust
    ui::heading(&format!(
        "tt chat · {} via TokenTrimmer{}   (/help)",
        conv.model,
        if tools_enabled { " · tools on" } else { "" }
    ));
```

- [ ] **Step 6: Route the three turn arms through `dispatch_turn` (mod.rs)**

Replace each of the three `do_turn(&http, &base, &key, &mut conv, &mut ledger).await` calls (in the `Command::Chat`, `Command::Editor`, and `Command::Retry` arms) with:

```rust
dispatch_turn(&http, &base, &key, &mut conv, &mut ledger, &registry, tools_enabled).await
```

(Three call sites; the surrounding `if !... { conv.messages.pop(); }` / retry restore logic stays unchanged.)

- [ ] **Step 7: Add the `/tools` run arm (mod.rs)**

After the `Command::Cost => ...` arm, add:

```rust
                    Command::Tools(set) => {
                        tools_enabled = set.unwrap_or(!tools_enabled);
                        if tools_enabled {
                            ui::info("tools: on (find_route_for, preview_cost, inspect_diff)");
                        } else {
                            ui::info("tools: off");
                        }
                    }
```

- [ ] **Step 8: Add the help row (mod.rs)**

In `print_help`, after the `("/copy", ...)` row, add:

```rust
        ("/tools [on|off]", "toggle tool-calling (find_route_for, preview_cost, inspect_diff)"),
```

- [ ] **Step 9: Build + existing tests**

Run: `cargo build -p tt-cli 2>&1 | grep -E "error|warning: unused|never used" | head` then `cargo test -p tt-cli --lib chat 2>&1 | tail -8`
Expected: no errors / no dead-code warnings (everything wired); all chat tests pass.

- [ ] **Step 10: Commit**

```bash
git add crates/cli/src/chat/tools.rs crates/cli/src/chat/mod.rs
git commit -m "feat(chat): /tools toggle + non-streamed tool-call loop"
```

---

### Task 4: httpmock integration test — the full loop + rollback

**Files:**
- Modify: `crates/cli/src/chat/tools.rs` (integration tests)

- [ ] **Step 1: Write the loop + rollback tests**

Add to `tools.rs` `mod tests` (httpmock + tokio are dev-deps; use a `#[tokio::test]`):

```rust
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
        // httpmock uses the first matching mock by creation order, so a request
        // WITH a tool result matches the specific mock above; WITHOUT one falls
        // here.
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
        assert!(matches!(conv.messages[1], Message::Assistant { ref tool_calls, .. } if !tool_calls.is_empty()));
        assert!(matches!(&conv.messages[2], Message::Tool { content: MessageContent::Text(t), .. } if t.contains("model")));
        assert!(matches!(&conv.messages[3], Message::Assistant { content: Some(MessageContent::Text(t)), .. } if t == "Use Haiku."));
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
        assert_eq!(conv.messages.len(), start, "history must be unchanged on failure");
    }
```

> Note: the **specific** mock (`body_contains("\"role\":\"tool\"")`, defined first) matches round 2 (the tool result is present) → final answer; the **broad** mock (path only, defined second) matches round 1 (no tool result yet) → tool_call. This relies on httpmock using the first-created matching mock. If the installed httpmock version instead favors the last/most-specific match and round 1 wrongly returns the final answer, swap to a mutually-exclusive matcher for round 1 (the version's negative/`body_excludes` or a regex that requires the user text without a tool result) — do not weaken the assertions.

- [ ] **Step 2: Run the integration tests**

Run: `cargo test -p tt-cli --lib chat::tools 2>&1 | tail -20`
Expected: PASS — `tool_loop_executes_then_answers`, `tool_loop_rolls_back_on_error`, plus the unit tests.

If `tool_loop_executes_then_answers` fails because round 1 also matches the second mock or vice-versa, tighten the matchers (round 1 has no `"role":"tool"`; round 2 does). Adjust `body_contains` accordingly — do not weaken the assertions.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/chat/tools.rs
git commit -m "test(chat): httpmock integration for the tool-call loop"
```

---

### Task 5: `--tools` flag (main.rs)

**Files:**
- Modify: `crates/cli/src/main.rs`

- [ ] **Step 1: Add the flag to the `Chat` command**

In the `Chat { ... }` variant (around line 121), add after the `resume` arg:

```rust
        /// Enable tool-calling from the start (find_route_for, preview_cost, inspect_diff).
        #[arg(long)]
        tools: bool,
```

- [ ] **Step 2: Thread it through dispatch**

In the `Command::Chat { ... }` match arm (around line 454), add `tools,` to the destructure and pass it:

```rust
        Command::Chat {
            model,
            system,
            resume,
            tools,
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::chat::run(model, system, resume, tools, tt_api_key, tt_api_base).await?;
        }
```

- [ ] **Step 3: Build**

Run: `cargo build -p tt-cli 2>&1 | grep -E "error" | head`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/main.rs
git commit -m "feat(chat): --tools flag to start with tool-calling on"
```

---

### Task 6: Gates + smoke + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt -p tt-cli && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings. (If `dispatch_turn`'s 7 args trip `clippy::too_many_arguments`, add `#[allow(clippy::too_many_arguments)]` above it with a one-line note.)

- [ ] **Step 2: Full tests**

Run: `cargo test -p tt-cli 2>&1 | grep -E "test result|error\[" | tail -10`
Expected: all pass.

- [ ] **Step 3: cargo-deny**

Run: `cargo deny check advisories 2>&1 | tail -3`
Expected: `advisories ok` (no new deps).

- [ ] **Step 4: Smoke (piped, no network)**

Run:
```bash
cargo build -q -p tt-cli --bin tt
printf '/tools\n/tools off\n/help\n/exit\n' | TT_API_KEY=test target/debug/tt chat 2>&1 | grep -E "tools:|/tools"
```
Expected: `/tools` (no arg) → `tools: on (...)`; `/tools off` → `tools: off`; `/help` lists `/tools [on|off]`.

- [ ] **Step 5: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** registry (T1), tools_json (T1), parse_tool_calls/usage_from_parts/format_tool_call (T2), `run_tool_turn` non-streamed loop with failure rollback (T3), `/tools` toggle + dispatch + heading + help (T3), `--tools` (T5), httpmock loop + rollback tests (T4), gates/smoke (T6). All spec items covered.
- **Placeholders:** none — every step has complete code.
- **Type consistency:** `run_tool_turn(&Client,&str,&str,&mut Conversation,&Registry,&mut Ledger) -> bool` and `dispatch_turn(...,&tt_mcp::tools::Registry,bool) -> bool` are used consistently; `build_registry`/`tools_json`/`parse_tool_calls`/`usage_from_parts`/`format_tool_call` signatures match their call sites; `Message`/`MessageContent`/`ToolCall`/`ToolCallFunction` come from `tt_shared::messages`; `UsageInfo`/`Conversation`/`Ledger`/`format_turn_footer` imported from `super`.
- **Dead-code boundary:** `run_tool_turn` is introduced and wired (via `dispatch_turn`) in the same task (T3), so no commit leaves it unused under `-D warnings`.
