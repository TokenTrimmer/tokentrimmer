# V5b-2 — `tt chat` Agentic Tool-Calling Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V5b-2 (second of three V5b sub-slices; V5b-1 ergonomics merged in #27, V5b-3 = context management).
**Depends on:** V5a + V5b-1 merged.

## Goal

Let the chat model call TokenTrimmer's own tools mid-conversation: ask "what model should I use for X?", "what would this prompt cost?", "what's risky in this diff?" and `tt chat` actually runs the tool and feeds the result back. Reuses the existing, tested `tt-mcp` tools.

## Decisions (confirmed)

- **Tools:** the 3 stateless, read-only tools only — `find_route_for`, `preview_cost`, `inspect_diff`. (Defer the stateful `lookup_semantic_cache` / `simulate_plan`, which need backends.) All three operate purely on their JSON params (no fs/network side effects), so they **auto-execute** with no approval prompt.
- **Streaming:** when tools are active a turn runs **non-streamed** (each round returns complete `tool_calls`, easy to assemble + execute); the final text answer prints at once. Normal (no-tools) chat keeps the existing live streaming via `do_turn`.
- **Enablement:** a `/tools` toggle (and `--tools` flag), **off by default** (advertising tools capability-gates routing to tool-capable models and can change cost/behavior).

## Architecture

A client-side tool-call loop. `tt chat` advertises the tools, the provider returns `tool_calls`, the CLI executes each via `Registry::call`, appends `Tool` result messages, and re-sends until the model returns a text answer (capped rounds). All new tool code lives in a new submodule `crates/cli/src/chat/tools.rs`; `mod.rs` gains the `/tools` toggle, the registry/`tools_enabled` state, and a turn dispatcher.

### `crates/cli/src/chat/tools.rs`

- `build_registry() -> tt_mcp::tools::Registry` — registers `FindRouteForTool`, `PreviewCostTool`, `InspectDiffTool` (all unit structs).
- `tools_json(reg: &Registry) -> Vec<serde_json::Value>` — maps each `ToolDef{name,description,input_schema}` to OpenAI form `{"type":"function","function":{"name","description","parameters": input_schema}}`. (input_schema is already JSON Schema.)
- `parse_tool_calls(message: &Value) -> Vec<tt_shared::messages::ToolCall>` — from `choices[0].message.tool_calls`; each → `ToolCall{ id, type, function: ToolCallFunction{ name, arguments } }` (arguments kept as the stringified-JSON the API returns). Filters malformed entries.
- `usage_from_parts(cost_usd, saved_usd, in_tok, out_tok) -> UsageInfo` (pure, tested) — non-streamed responses carry `cost`/`saved` in **headers** (`x-tokentrimmer-cost-usd`, `x-tokentrimmer-saved-usd`) and tokens in the body's `usage`; `baseline_cost_usd = cost + saved`.
- `format_tool_call(name: &str, args: &str) -> String` (pure, tested) — the muted inline line shown per call, args truncated to a sane width.
- `async fn run_tool_turn(http, base, key, conv: &mut Conversation, reg: &Registry, ledger: &mut Ledger) -> bool`:
  - `let start_len = conv.messages.len();` (for failure rollback).
  - `const MAX_ROUNDS: usize = 6;`
  - Each round: POST `{model, messages: wire_messages, tools, stream:false}`; on a non-2xx or transport error → `ui::error`, `conv.messages.truncate(start_len)`, return `false`.
  - Read `x-tokentrimmer-model-used` + cost headers; parse the JSON body.
  - `let calls = parse_tool_calls(&body["choices"][0]["message"]);`
  - **If `calls` is empty** → final answer: print `content`, `conv.push_assistant(content)`, build `UsageInfo` from headers+body, `ledger.add`, print `format_turn_footer`, return `true`.
  - **Else** → push `Message::Assistant{ content, tool_calls: calls.clone(), name: None }`; for each call: print `format_tool_call`, `let out = reg.call(&name, args).await` (errors captured as `{"error": …}` JSON, never abort the loop), print a truncated result line, push `Message::Tool{ content: Text(out_json), tool_call_id: id }`. Accumulate the round's usage into the ledger. Loop.
  - If `MAX_ROUNDS` is hit without a text answer → `ui::warn("tool loop hit the round cap")`, return `true` (history kept; partial is coherent).
  - **Failure rollback contract** matches `do_turn`: on the early-return false path the conversation is truncated back to `start_len`, so the caller's existing "pop the user on failure" stays correct.

### `crates/cli/src/chat/mod.rs`

- `pub mod tools;`
- `Command` gains `Tools(Option<bool>)`; `parse`: `"tools"` → `Tools(None)` (toggle/show), `"tools on"|"off"` → `Tools(Some(_))`.
- `run`: build `let registry = tools::build_registry();` once; add `let mut tools_enabled = tools_flag;` (from the new `--tools`).
- **Turn dispatcher**: replace the three direct `do_turn(...)` calls (Chat, Editor, Retry) with `dispatch_turn(...)`:
  ```rust
  async fn dispatch_turn(http, base, key, conv, ledger, reg, tools_enabled) -> bool {
      if tools_enabled { tools::run_tool_turn(http, base, key, conv, reg, ledger).await }
      else { do_turn(http, base, key, conv, ledger).await }
  }
  ```
  (Both return `true` on success and leave the conversation in the "caller pops the user on false" state.)
- `/tools` arm: `Tools(None)` → flip + `ui::info("tools: on/off")`; `Tools(Some(b))` → set + info. When turning on, note the active tool names.
- `print_help`: add `/tools [on|off]  enable tool-calling (find_route_for, preview_cost, inspect_diff)`.
- Heading shows `· tools on` when enabled.

### `crates/cli/src/main.rs`

- `Chat` clap command gains `--tools` (bool); threaded into `chat::run(..., tools)`.

## Cargo

No new deps — `tt-mcp` is already a `tt-cli` dependency. (`async-trait` comes in transitively via `tt-mcp`.)

## Testing

- **`tools_json`**: 3 entries; each `function.name` ∈ {find_route_for, preview_cost, inspect_diff}; `parameters` carries the tool's `input_schema` (e.g. `find_route_for` requires `task_description`).
- **`parse_tool_calls`**: a `message` with one `tool_calls` entry → one `ToolCall` with the right `id`/`function.name`/`arguments`; missing/empty → `[]`; a malformed entry (no id) is skipped.
- **`usage_from_parts`**: `baseline_cost_usd == cost + saved`; fields mapped.
- **`format_tool_call`**: contains the tool name; long args truncated.
- **Integration (httpmock, already a dev-dep): the full loop.** Mock the gateway: 1st POST → an assistant message with a `find_route_for` `tool_call`; 2nd POST → a final text answer. Drive `run_tool_turn` against the mock base; assert it returns `true`, the conversation ends `[User, Assistant(tool_calls), Tool(result), Assistant(text)]`, and the `Tool` message contains the real `find_route_for` output (executed locally). A 2-hit mock proves the loop sends tool results back.
- **Failure rollback**: mock a single 500 → `run_tool_turn` returns `false` and `conv.messages.len() == start_len`.
- `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo deny`; smoke (`/tools on`, `/help`, `/tools off`, `/exit` — piped, no network).

## Out of Scope (later)
- Streaming the final answer during a tool turn (kept non-streamed).
- The 2 stateful tools (`lookup_semantic_cache`, `simulate_plan`) — **V5b-2b or later** once the CLI can supply their backends.
- Per-call approval prompts (the 3 tools are read-only); parallel tool execution (calls run sequentially).
- `tool_choice` forcing — left as provider default (auto).

## Risk / spike
The design assumes the gateway forwards `tools` and returns provider `tool_calls` on the non-streamed `/v1/chat/completions` path (supported by `capability_check` routing + passthrough). The httpmock integration test validates the **loop**; a first implementation task does a quick real-or-mock round-trip sanity check before building out.
