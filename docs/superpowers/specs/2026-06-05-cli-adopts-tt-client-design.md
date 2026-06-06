# CLI Adopts `tt-client` Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** Post-roadmap follow-up #2. Routes the `tt` CLI's gateway calls through the `tt-client` SDK.
**Depends on:** `tt-client` chat (#36), streaming (#37), tool-calling (#38) merged.

## Goal

Delete the CLI's hand-rolled gateway networking and SSE/cost parsing by routing
`tt chat` (streaming) and the `tt advise` / tool-calling loop (non-streamed)
through `tt-client`. The streaming SSE parser (`parse_sse_frame`/`drain_frames`/
`StreamEvent`) is a byte-for-byte duplicate of the SDK's; the tool path duplicates
cost-header extraction, request building, and `tool_calls` parsing the SDK's typed
response already provides. This is a **behavior-preserving refactor**: every CLI
domain type (`Conversation`, `Ledger`, `UsageInfo`, `format_turn_footer`,
`TurnTotals`) and all UX (live streaming, spinner, footer, tool-call previews,
session save/resume, context budget) stay exactly as they are.

The CLI keeps its **own** tool loop (`run_tool_turn`) — the SDK's `run_tools`
driver can't print the CLI's tool-call/result previews or execute via the CLI's
`tt-mcp` `Registry`. The de-dup target is the protocol/parsing layer (each gateway
call), not the loop.

## Architecture

Add `tt-client` as a dependency of `tt-cli`. Replace the
`(http: &reqwest::Client, base: &str, key: &str)` parameter triple threaded
through the turn functions with a single `client: &tt_client::Client`, built once
in `run()` (and in `advise::run`). The SDK owns the reqwest client, request
bodies, header parsing, and SSE decoding; the CLI keeps the presentation loop.

Files touched:
- `crates/cli/Cargo.toml` — add the `tt-client` dependency.
- `crates/cli/src/chat/mod.rs` — streaming path + threading; delete the SSE parser.
- `crates/cli/src/chat/tools.rs` — tool path; delete header/JSON parsing.
- `crates/cli/src/advise.rs` — build a `tt_client::Client`.

## Dependency

`tt-client` is a workspace member but not yet a `[workspace.dependencies]` entry.
Add it there (`tt-client = { path = "crates/client" }`) and depend via
`tt-client.workspace = true` in `crates/cli/Cargo.toml`, matching how the CLI
already depends on `tt-shared`/`tt-mcp`/`tt-tokenize`. (If the established pattern
for a given dep is a direct `{ path = "../client" }`, follow that instead — verify
against the existing `crates/cli/Cargo.toml` dependency style during implementation.)

## Streaming path (`chat/mod.rs`)

### `stream_turn`
Signature changes to `async fn stream_turn(client: &tt_client::Client, conv: &Conversation) -> anyhow::Result<(String, Option<UsageInfo>)>`.

Body:
```rust
let mut stream = client
    .chat()
    .model(&conv.model)
    .messages(conv.wire_messages())
    .stream()
    .await
    .context("request to gateway failed")?;

let served_model = stream
    .header_cost()
    .model_used
    .clone()
    .unwrap_or_else(|| conv.model.clone());

let mut spinner = Some(ui::spinner("…"));
let mut reply = String::new();
let mut usage: Option<UsageInfo> = None;
while let Some(ev) = stream.next().await.context("stream error")? {
    match ev {
        tt_client::StreamEvent::Delta(t) => {
            spinner.take();
            print!("{t}");
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            reply.push_str(&t);
        }
        tt_client::StreamEvent::Usage(u) => usage = Some(UsageInfo::from(u)),
    }
}
drop(spinner);
println!();
if let Some(u) = &usage { /* unchanged: print format_turn_footer(&served_model, …) */ }
Ok((reply, usage))
```
`StreamEvent` is `#[non_exhaustive]` and lives in another crate, so the `match`
**must** include a `_ => {}` wildcard arm — without it the match fails to compile
(`non-exhaustive patterns`), not merely warn. The wildcard also future-proofs the
CLI against a new SDK variant.

### Error mapping
`tt_client::Error` implements `std::error::Error`, so anyhow's `.context(...)?`
converts it. A non-2xx response surfaces as `Error::Status { status, body, .. }`
whose `Display` is `gateway returned {status}: {body}` — `stream_turn` returns
`Err`, the caller (`do_turn`) prints `{e:#}` and drops the pending user turn,
exactly as today.

### `UsageInfo` conversion
Add to `chat/mod.rs`:
```rust
impl From<tt_client::StreamUsage> for UsageInfo {
    fn from(u: tt_client::StreamUsage) -> Self {
        Self {
            cost_usd: u.cost_usd,
            baseline_cost_usd: u.baseline_cost_usd,
            saved_usd: u.saved_usd,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_tokens: u.cached_tokens,
        }
    }
}
```
`UsageInfo` keeps its fields and derives (its `Deserialize` is now unused by the
SSE path but harmless; leave it to avoid churn). `Ledger`, `format_turn_footer`,
session cost, and `TurnTotals` are unchanged.

### Deletions
Remove `parse_sse_frame`, `drain_frames`, the `StreamEvent` enum, and their unit
tests (`parse_sse_frame_*`, the `drain_frames` regression). The SDK owns and tests
this logic.

## Tool path (`chat/tools.rs`)

### `send_round`
Signature: `async fn send_round(client: &tt_client::Client, conv: &Conversation, tools: &[tt_client::Tool], force_no_tools: bool) -> anyhow::Result<Round>`.

Body:
```rust
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
Ok(Round { served_model, calls, content, usage, assistant_msg })
```
`Round` gains an `assistant_msg: Option<Message>` field carrying the typed
assistant message to push to history (replacing the manual reconstruction). When
`assistant_msg` is `None` (empty `choices`), the loop treats it as "no tool calls"
and finishes the turn — matching the SDK's empty-choices handling.

### `run_tool_turn` loop adjustment
Where the loop currently rebuilds the assistant message from parsed calls:
```rust
conv.messages.push(Message::Assistant { content, tool_calls: round.calls.clone(), name: None });
```
it instead pushes the typed message the SDK already returned:
```rust
if let Some(m) = round.assistant_msg.clone() {
    conv.messages.push(m);
}
```
Everything else in the loop (per-call previews via `format_tool_call`, executing
`reg.call`, pushing `Message::Tool` results, `TurnTotals`, `finish_turn`, the
round cap → forced `tool_choice:"none"` final, truncate-to-`start_len` on failure)
is unchanged.

### `tools` argument
`run_tool_turn` builds the advertised tools once as `Vec<tt_client::Tool>` from the
registry definitions (replacing `tools_json`):
```rust
fn registry_tools(reg: &Registry) -> Vec<tt_client::Tool> {
    reg.list()
        .into_iter()
        .map(|d| tt_client::tool(d.name, d.description, d.input_schema))
        .collect()
}
```

### Deletions
Remove `header_f64`, `parse_tool_calls`, `tools_json`, the manual reqwest
POST/header reads in `send_round`, and their unit tests
(`parse_tool_calls_*`). Keep `usage_from_parts`, `TurnTotals`, `format_tool_call`,
`finish_turn`, `build_registry`.

## `tt advise` (`advise.rs`)

Replace `let http = reqwest::Client::new();` with
`let client = tt_client::Client::new(base, key);` (sourced from the same
`ResolvedContext` it already loads), and pass `&client` to `run_tool_turn`. No
other change; advise rides the refactored tool path.

## Testing

- **Keep** the end-to-end httpmock tests in `tools.rs` (`tool_loop_executes_then_answers`,
  `tool_loop_forces_answer_at_round_cap`, `tool_loop_rolls_back_on_error`). They
  mock the gateway and drive `run_tool_turn`; the requests now flow through
  `tt-client` but carry the same wire format, so the `body_contains("\"role\":\"tool\"")`
  and `"tool_choice":"none"` matchers and the cost/model headers still match.
  These tests must be updated to construct a `tt_client::Client::new(server.base_url(), "k")`
  and pass `&client` instead of `(&http, &base_url, "k")`.
- **Retarget** `tools_json_advertises_three_tools` → `registry_tools_advertises_three_tools`:
  assert `registry_tools(&build_registry())` yields 3 `Tool`s whose function names
  are `find_route_for`/`preview_cost`/`inspect_diff` and that `find_route_for`'s
  parameters carry the `task_description` required field.
- **Add** a streaming httpmock test for `stream_turn`: a mock returning an SSE body
  (two content deltas, a `tokentrimmer.usage` event, `[DONE]`) + `x-tokentrimmer-model-used`
  header; assert the returned reply text and `UsageInfo` tokens. (Construct
  `tt_client::Client::new(server.base_url(), "k")`.)
- **Delete** unit tests of removed functions: `parse_sse_frame_*`, the
  `drain_frames` regression, `parse_tool_calls_*`. (Their behavior is covered by
  `tt-client`'s own tests.)
- **Keep** all other CLI tests (`Command::parse`, `format_turn_footer`,
  `Conversation`, budget/context, session `sanitize_name`, `format_tool_call`,
  `usage_from_parts`) unchanged.
- **Gates:** `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo test -p tt-cli` and the workspace suite; `cargo deny check advisories`;
  `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-cli --no-deps`.

## Error handling

- Streaming/tool gateway failures become `tt_client::Error` → `anyhow` via
  `.context(...)`; the turn fails, the message is printed, and the pending user
  turn is dropped (`run_tool_turn` truncates to `start_len`; `do_turn` returns
  false). No behavior change.
- Per-tool `reg.call` errors keep being caught inside `run_tool_turn` and fed back
  as `Message::Tool` results — that logic is CLI-side and untouched.

## Out of scope

- Using the SDK's `run_tools` driver (the CLI keeps its preview-printing loop).
- Migrating `tt models` / `tt catalog` (`GET /v1/models`) onto a future SDK method
  — the SDK has no models endpoint yet (separate slice).
- Any change to streaming/tool UX, session format, or the cost footer.
- Embeddings / new endpoints (slice #3).
