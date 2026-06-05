# `tt-client` Tool-Calling Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** Post-roadmap follow-up #1 (tt-client). Adds tool-calling to the SDK.
**Depends on:** `tt-client` (V7 #36) + streaming (V7b #37) merged.

## Goal

Make `tt-client` usable for building agents: let the SDK user advertise their own
tools, surface the model's `tool_calls`, and either drive the call→execute→re-send
loop themselves (low-level surface) or hand it to a built-in driver (`run_tools`).
Tools ride the **non-streaming `send()` path**; streaming tool-call deltas stay
out of scope (deferred, as in the roadmap).

Unlike the CLI's `chat::tools::run_tool_turn` — which owns a *fixed* registry of 3
`tt-mcp` tools, executes them locally, and is wired to the CLI's `Conversation`/
`Ledger`/`ui` — the SDK cannot assume any tools. The SDK *user* owns the tools, so
the SDK provides (a) the wire plumbing to advertise tools and read calls, and (b)
a generic driver parameterised by a user-supplied `ToolExecutor`.

## Architecture

All in `crates/client`. The builder plumbing + message/tool constructors live in
`src/lib.rs` (next to the existing `user`/`system`/`assistant` helpers and the
`ChatBuilder`); the driver machinery lives in a new `src/tools.rs` module
(`mod tools;` + re-exports from `lib.rs`). Same-crate privacy lets `tools.rs`
access `ChatBuilder`'s private fields and the private `send`-side helpers.

New dependency: `async-trait` (workspace dep — same one `tt-mcp::Tool` uses). No
other new deps. `async-trait` is chosen over native async-fn-in-trait because a
public `async fn` in a trait trips the `async_fn_in_trait` lint, which CI's
`cargo clippy --workspace --all-targets -- -D warnings` would reject; `#[async_trait]`
also guarantees the returned future is `Send`, which the async loop needs.

## Low-level surface (`lib.rs`)

### `ChatBuilder` gains tool fields + setters
- Fields: `tools: Vec<Tool>`, `tool_choice: Option<ToolChoice>`, `max_tool_rounds: usize`
  (defaulted in `Client::chat()` to **8**).
- Setters (consume + return `self`, matching the existing builder style):
  - `pub fn tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self`
  - `pub fn tool_choice(mut self, choice: ToolChoice) -> Self`
  - `pub fn max_tool_rounds(mut self, n: usize) -> Self`

### `send()` injects tools
`send()` keeps its current body-building flow; after `build_body(&self.model,
&self.messages, self.max_tokens, self.temperature, false)` it injects the tool
fields onto the returned `Value` when present. A shared private helper
`inject_tools(body, tools, tool_choice)` does this so `send` and the loop share
one path:
```rust
fn inject_tools(body: &mut Value, tools: &[Tool], tool_choice: Option<&ToolChoice>) {
    if !tools.is_empty() {
        body["tools"] = serde_json::to_value(tools).unwrap_or(Value::Null);
    }
    if let Some(tc) = tool_choice {
        body["tool_choice"] = serde_json::to_value(tc).unwrap_or(Value::Null);
    }
}
```
`build_body`'s signature is **unchanged** (tool logic is localized to
`inject_tools`). Serialization of `Tool`/`ToolChoice` is infallible in practice
(plain structs), so `inject_tools` uses `unwrap_or(Value::Null)` to stay
panic-free without adding a serde error variant to `Error` — `tool_choice: Null`
is ignored by the gateway, and an empty `tools` is never injected (the `is_empty`
guard).

### Message/tool constructors (next to `user`/`system`/`assistant`)
- `pub fn tool(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Tool`
  → `Tool { r#type: "function".into(), function: ToolFunction { name, description: Some(description), parameters } }`.
- `pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Message`
  → `Message::Tool { content: MessageContent::Text(content.into()), tool_call_id: tool_call_id.into() }`.

### `ChatOutcome::tool_calls`
- `pub fn tool_calls(&self) -> &[ToolCall]` — the first choice's `message`
  `tool_calls` (empty slice when there are none, or no choices). Lets the manual
  path branch on "did the model ask for tools?".

### Re-exports
Add `Tool, ToolFunction, ToolChoice, ToolChoiceFunction` to the `pub use
tt_shared::messages::{…}` block (`ToolCall`/`ToolCallFunction` are already
re-exported).

### Manual-loop usage (low-level)
```rust
let out = client.chat().model("gpt-4o-mini")
    .messages(history.clone())
    .tools(vec![tt_client::tool("get_weather", "Look up weather", schema)])
    .send().await?;
if !out.tool_calls().is_empty() {
    history.push(out.response.choices[0].message.clone()); // assistant w/ tool_calls
    for tc in out.tool_calls() {
        let result = run_my_tool(&tc.function.name, &tc.function.arguments);
        history.push(tt_client::tool_result(&tc.id, result));
    }
    // …re-send with the same .tools(...)
}
```

## Driven loop (`tools.rs`)

### `ToolExecutor` trait
```rust
#[async_trait::async_trait]
pub trait ToolExecutor {
    /// Run the tool named `name` with the model's raw JSON `arguments` string.
    /// Return the tool result as a string (any format; JSON conventional).
    ///
    /// An `Err` is fed BACK to the model as the tool result (so it can recover)
    /// and does NOT abort the loop. Use this for unknown-tool / per-call failures.
    async fn call(&self, name: &str, arguments: &str)
        -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>>;
}
```

### `run_tools`
```rust
impl ChatBuilder<'_> {
    /// Drive the agentic loop: advertise the builder's `.tools(...)`, execute the
    /// model's tool calls via `executor`, feed results back, until the model
    /// returns a text answer or `max_tool_rounds` is hit (then one forced
    /// `tool_choice:"none"` call guarantees a final text answer).
    ///
    /// # Errors
    /// Propagates the same `Error` as `send` (gateway non-2xx / request / decode)
    /// — a gateway failure aborts the loop. Per-tool executor errors do NOT
    /// propagate; they are fed back to the model.
    pub async fn run_tools(self, executor: &(impl ToolExecutor + Sync)) -> Result<ToolOutcome>;
}
```

### Result types
```rust
pub struct ToolOutcome {
    /// The final assistant response (its `tool_calls` is empty).
    pub response: ChatCompletionResponse,
    /// Cost/savings summed across every round of the loop.
    pub cost: AggregateCost,
    /// The full message transcript (the builder's input messages plus every
    /// assistant tool-call message, tool result, and the final answer), so the
    /// caller can persist the conversation.
    pub messages: Vec<Message>,
    /// Number of gateway round-trips made (includes the forced final, if any).
    pub rounds: usize,
}

pub struct AggregateCost {
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub baseline_cost_usd: f64,
}
impl AggregateCost {
    /// `saved / baseline * 100`, or `None` when baseline is 0.
    pub fn savings_pct(&self) -> Option<f64>;
}
impl ToolOutcome {
    /// The final answer text (`choices[0].message.content`), if any.
    pub fn text(&self) -> Option<&str>;
}
```

### Loop algorithm (mirrors the CLI's `run_tool_turn` contract)
1. `messages` starts as the builder's input messages (cloned/owned). `cost` =
   `AggregateCost::default()`, `rounds = 0`.
2. For up to `max_tool_rounds` iterations:
   a. Send one non-streamed request (`model`, current `messages`, `tools`,
      `tool_choice` if set, `stream:false`) via a private `send_round` helper that
      returns `(ChatCompletionResponse, CostInfo)`; `rounds += 1`; accumulate cost.
   b. Take `choices[0].message`. **If its `tool_calls` is empty** → push it to
      `messages`, set `response` to this response, **return** `ToolOutcome`.
   c. Otherwise push the assistant message (clone) to `messages`; for each
      `ToolCall`, `executor.call(name, arguments)` → on `Ok(s)` use `s`, on
      `Err(e)` use `{"error": e.to_string()}`; push `tool_result(tc.id, result)`.
3. **Cap reached:** one forced request with `tool_choice:"none"` (overriding any
   user `tool_choice`) → `rounds += 1`, accumulate cost, push the assistant
   message, return its `ToolOutcome`. After `tool_choice:"none"` the model cannot
   call tools, so a text answer is guaranteed; the response is returned as-is (the
   SDK never fabricates content).
4. Empty `choices` is treated as "no tool_calls" (step 2b) — the loop returns the
   response so the caller can inspect it rather than spinning.

### Shared `send_round` helper
A private `async fn send_round(client, model, messages, tools, tool_choice,
force_no_tools, max_tokens, temperature, tag) -> Result<(ChatCompletionResponse,
CostInfo)>` does one non-streamed call: build body, inject tools + (forced or
user) tool_choice, bearer, `X-TokenTrimmer-Tag`, parse headers → `CostInfo`,
non-2xx → `Error::Status { cost: Box::new(cost) }`, success → decode →
`(response, cost)`. `send()` MAY be refactored to delegate to `send_round` (with
`force_no_tools=false`, no tools) to keep one code path, but that refactor is
optional and behavior-preserving; if done, `send()`'s existing tests must still
pass byte-identically.

## Error handling & cost

- **Gateway errors propagate.** `send_round` returns the same `Error` variants as
  `send` (`Request` / `Status{status,body,cost:Box<CostInfo>}` / `Decode`). The
  loop bubbles them up, aborting.
- **Per-tool errors are fed back.** An `executor.call` `Err(e)` becomes a
  `tool_result` with content `{"error":"<e>"}` (JSON-stringified), exactly like the
  CLI feeds `json!({"error": e.to_string()})`. The loop continues so the model can
  recover or report.
- **Cost accumulation.** Each round's `CostInfo` contributes
  `cost_usd.unwrap_or(0.0)`, `saved_usd.unwrap_or(0.0)`, and `baseline_cost_usd`
  (`unwrap_or(cost + saved)` when the header is absent — same derivation as the
  CLI's `usage_from_parts`). Non-numeric header fields (`model_used`, `trace_id`)
  are not summable and are dropped from the aggregate; the final served model is
  available on `response.model`.

## Testing

`#[cfg(test)]` in `tools.rs` (+ the `lib.rs` surface tests), httpmock 0.7
(first-created matching mock wins). A small test `ToolExecutor` impl returns a
canned string (and an error-returning variant for the recovery test).

- **unit (lib.rs):** `tool()` builds `{type:"function", function:{name,description,parameters}}`;
  `tool_result()` builds `Message::Tool`; a `send()`-built body with `.tools(...)`/
  `.tool_choice(...)` serializes `tools` (array, function names present) and
  `tool_choice`.
- **`send_with_tools_surfaces_tool_calls`:** mock returns a `tool_calls` message →
  `out.tool_calls()` has the call; `out.response.choices[0].message` is an
  assistant-with-tool_calls.
- **`run_tools_executes_then_answers`:** two mocks (role:tool → final text; broad →
  tool_call), canned executor → final `response` text == expected, `rounds == 2`,
  `cost.cost_usd` summed across rounds, `messages` ends with the assistant answer
  and contains the `Tool` result.
- **`run_tools_forces_answer_at_cap`:** broad mock always returns a tool_call;
  `tool_choice:"none"` mock returns text → final text answer, `rounds ==
  max_tool_rounds + 1`, cost summed, last message is the assistant answer.
- **`run_tools_feeds_executor_error_back`:** executor returns `Err` on the first
  call; the request that carries the tool result (`"role":"tool"`) returns a text
  answer → loop does NOT abort, the fed-back tool result contains `error`, final
  answer is returned.
- **`run_tools_propagates_gateway_error`:** 500 → `Err(Error::Status{status:500,..})`.
- **gates:** `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo test -p tt-client`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D
  warnings" cargo doc -p tt-client --no-deps`.

## Out of scope

- Streaming + tools (`tool_calls` deltas over SSE) — deferred.
- Parallel tool execution within a round (the loop runs a round's calls
  sequentially, matching the CLI; concurrency can be added later without breaking
  the trait).
- A built-in tool registry / `tt-mcp` integration in the SDK — the SDK stays
  tool-agnostic; the CLI keeps its own registry and will wire it to the SDK's
  surface in follow-up #2 (CLI-adopts-tt-client).
- Embeddings, `response_format`/structured output, and the other
  `ChatCompletionRequest` fields — separate slices.
