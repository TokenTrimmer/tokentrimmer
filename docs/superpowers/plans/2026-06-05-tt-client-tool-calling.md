# tt-client Tool-Calling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add tool-calling to the `tt-client` SDK — a low-level surface (advertise tools, read `tool_calls`, build tool messages) plus a driven `run_tools` loop parameterised by a user `ToolExecutor`.

**Architecture:** Builder plumbing + message/tool constructors go in `crates/client/src/lib.rs`; the driver machinery (`ToolExecutor`, `run_tools`, `ToolOutcome`, `AggregateCost`, `send_round`) goes in a new `crates/client/src/tools.rs`. Tools ride the non-streaming `send()` path. A shared private `inject_tools` helper adds `tools`/`tool_choice` to the request body for both `send` and the loop.

**Tech Stack:** Rust (edition 2021, rust 1.88), reqwest, serde_json, `async-trait` (new workspace dep), httpmock (tests).

Spec: `docs/superpowers/specs/2026-06-05-tt-client-tool-calling-design.md`. Work on branch `tool-calling-tt-client` (already created off `main`, with the spec committed).

---

### Task 1: Tool/message constructors + re-exports

**Files:**
- Modify: `crates/client/src/lib.rs` (re-export block ~11-14; helper fns after `assistant` ~42)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/client/src/lib.rs` (after the existing `message_helpers` test, ~line 501):

```rust
    #[test]
    fn tool_constructors() {
        let t = tool("get_weather", "Look up weather", json!({"type":"object"}));
        assert_eq!(t.r#type, "function");
        assert_eq!(t.function.name, "get_weather");
        assert_eq!(t.function.description.as_deref(), Some("Look up weather"));
        assert_eq!(t.function.parameters["type"], "object");

        assert!(matches!(
            tool_result("call_1", "42"),
            Message::Tool { content: MessageContent::Text(c), tool_call_id }
                if c == "42" && tool_call_id == "call_1"
        ));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-client tool_constructors`
Expected: FAIL — `cannot find function 'tool'` / `'tool_result'`.

- [ ] **Step 3: Add the constructors**

In `crates/client/src/lib.rs`, after the `assistant` fn (ends ~line 42), add:

```rust
/// Build a function `tool` definition to advertise to the model.
#[must_use]
pub fn tool(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Tool {
    Tool {
        r#type: "function".to_string(),
        function: ToolFunction {
            name: name.into(),
            description: Some(description.into()),
            parameters,
        },
    }
}

/// Build a `tool` result message answering the call `tool_call_id`.
#[must_use]
pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Message {
    Message::Tool {
        content: MessageContent::Text(content.into()),
        tool_call_id: tool_call_id.into(),
    }
}
```

- [ ] **Step 4: Add the re-exports**

In `crates/client/src/lib.rs`, change the `pub use tt_shared::messages::{…}` block (~lines 11-14) to add `Tool, ToolFunction, ToolChoice, ToolChoiceFunction`:

```rust
pub use tt_shared::messages::{
    ChatCompletionResponse, Choice, ContentPart, ImageUrl, InputAudio, Message, MessageContent,
    Tool, ToolCall, ToolCallFunction, ToolChoice, ToolChoiceFunction, ToolFunction,
};
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p tt-client tool_constructors`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(tt-client): tool()/tool_result() constructors + tool-type re-exports"
```

---

### Task 2: Builder tool fields, `inject_tools`, `send` wiring, `tool_calls()`

**Files:**
- Modify: `crates/client/src/lib.rs` (`ChatBuilder` struct ~245-252; `Client::chat` ~232-241; setters in `impl ChatBuilder` ~285; `send` body ~297-303; `ChatOutcome` impl ~460; new private `inject_tools` fn)

- [ ] **Step 1: Write the failing unit test for `inject_tools`**

Add to `mod tests`:

```rust
    #[test]
    fn inject_tools_adds_fields() {
        let mut body = json!({ "model": "m", "messages": [] });
        let tools = vec![tool("f", "desc", json!({"type":"object"}))];
        inject_tools(&mut body, &tools, Some(&ToolChoice::Auto("none".to_string())));
        assert_eq!(body["tools"][0]["function"]["name"], "f");
        assert_eq!(body["tool_choice"], "none");

        // empty tools + no choice → nothing added
        let mut bare = json!({ "model": "m" });
        inject_tools(&mut bare, &[], None);
        assert!(bare.get("tools").is_none());
        assert!(bare.get("tool_choice").is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-client inject_tools_adds_fields`
Expected: FAIL — `cannot find function 'inject_tools'`.

- [ ] **Step 3: Add `inject_tools`**

In `crates/client/src/lib.rs`, after `build_body` (ends ~line 98), add:

```rust
/// Inject `tools`/`tool_choice` onto a request body. No-ops on an empty tool
/// list; a `None` choice is left off. Serialization of these plain structs is
/// infallible in practice, so a `Null` fallback keeps this panic-free without a
/// serde error variant (the gateway ignores `tool_choice: null`).
fn inject_tools(body: &mut Value, tools: &[Tool], tool_choice: Option<&ToolChoice>) {
    if !tools.is_empty() {
        body["tools"] = serde_json::to_value(tools).unwrap_or(Value::Null);
    }
    if let Some(tc) = tool_choice {
        body["tool_choice"] = serde_json::to_value(tc).unwrap_or(Value::Null);
    }
}
```

- [ ] **Step 4: Add the builder fields + `chat()` init + setters**

In `crates/client/src/lib.rs`, extend the `ChatBuilder` struct (~245-252):

```rust
pub struct ChatBuilder<'a> {
    client: &'a Client,
    model: String,
    messages: Vec<Message>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    tag: Option<String>,
    tools: Vec<Tool>,
    tool_choice: Option<ToolChoice>,
    max_tool_rounds: usize,
}
```

In `Client::chat` (~232-241), initialise the new fields:

```rust
    pub fn chat(&self) -> ChatBuilder<'_> {
        ChatBuilder {
            client: self,
            model: String::new(),
            messages: Vec::new(),
            max_tokens: None,
            temperature: None,
            tag: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tool_rounds: 8,
        }
    }
```

In `impl ChatBuilder`, after the `tag` setter (~285), add:

```rust
    /// Advertise function `tools` to the model.
    #[must_use]
    pub fn tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.tools = tools.into_iter().collect();
        self
    }
    /// Constrain tool selection (`auto`/`none`/`required`/a specific function).
    #[must_use]
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }
    /// Max gateway round-trips in [`run_tools`](Self::run_tools) before a forced
    /// final answer (default 8).
    #[must_use]
    pub fn max_tool_rounds(mut self, n: usize) -> Self {
        self.max_tool_rounds = n;
        self
    }
```

- [ ] **Step 5: Wire `inject_tools` into `send`**

In `send` (~297-303), change `let body = build_body(...)` to `let mut body` and inject:

```rust
        let mut body = build_body(
            &self.model,
            &self.messages,
            self.max_tokens,
            self.temperature,
            false,
        );
        inject_tools(&mut body, &self.tools, self.tool_choice.as_ref());
```

- [ ] **Step 6: Add `ChatOutcome::tool_calls`**

In `impl ChatOutcome` (after `text`, ~474), add:

```rust
    /// The first choice's requested tool calls (empty when the model asked for
    /// none, or there are no choices).
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCall] {
        match self.response.choices.first().map(|c| &c.message) {
            Some(Message::Assistant { tool_calls, .. }) => tool_calls,
            _ => &[],
        }
    }
```

- [ ] **Step 7: Write the httpmock surface test**

Add to `mod tests` (the `httpmock::prelude::*` import + `json!`/`POST`/`MockServer` are already in scope):

```rust
    #[tokio::test]
    async fn send_advertises_tools_and_surfaces_calls() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"get_weather\"");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
                    "model": "gpt-4o-mini",
                    "choices": [{ "index": 0, "finish_reason": "tool_calls", "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "call_1", "type": "function",
                            "function": { "name": "get_weather", "arguments": "{\"city\":\"SF\"}" } }]
                    }}],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
                }));
        });

        let client = Client::new(server.base_url(), "k");
        let out = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("weather in SF?"))
            .tools(vec![tool("get_weather", "Look up weather", json!({"type":"object"}))])
            .send()
            .await
            .unwrap();

        assert_eq!(out.tool_calls().len(), 1);
        assert_eq!(out.tool_calls()[0].function.name, "get_weather");
        assert!(out.text().is_none()); // content was null
    }
```

- [ ] **Step 8: Run the tests**

Run: `cargo test -p tt-client inject_tools_adds_fields send_advertises_tools_and_surfaces_calls`
Expected: PASS (the mock requires the request body to contain `"get_weather"`, proving `tools` was sent; `tool_calls()` reads the response).

- [ ] **Step 9: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(tt-client): builder .tools()/.tool_choice()/.max_tool_rounds() + send wiring + tool_calls()"
```

---

### Task 3: `tools.rs` — `ToolExecutor` trait + `run_tools` driver

**Files:**
- Modify: `crates/client/Cargo.toml` (add `async-trait`)
- Create: `crates/client/src/tools.rs`
- Modify: `crates/client/src/lib.rs` (add `mod tools;` + re-exports)

- [ ] **Step 1: Add the `async-trait` dependency**

In `crates/client/Cargo.toml`, under `[dependencies]` (after `bytes.workspace = true`):

```toml
async-trait.workspace = true
```

- [ ] **Step 2: Create `tools.rs` with the driver (no tests yet)**

Create `crates/client/src/tools.rs`:

```rust
//! Agentic tool-calling driver for [`ChatBuilder::run_tools`]. The SDK is
//! tool-agnostic: the caller supplies a [`ToolExecutor`]; this module runs the
//! call -> execute -> re-send loop on the non-streaming path, accumulating cost.

use async_trait::async_trait;
use serde_json::json;

use crate::{
    build_body, inject_tools, parse_cost, tool_result, ChatBuilder, ChatCompletionResponse,
    Client, CostInfo, Error, Message, MessageContent, Result, ToolCall, ToolChoice,
};

/// Executes the model's tool calls. Implement this for your tools.
#[async_trait]
pub trait ToolExecutor {
    /// Run the tool named `name` with the model's raw JSON `arguments` string,
    /// returning the result as a string (any format; JSON conventional).
    ///
    /// # Errors
    /// An `Err` is fed BACK to the model as the tool result (so it can recover)
    /// and does NOT abort the loop — use it for unknown-tool / per-call failures.
    async fn call(
        &self,
        name: &str,
        arguments: &str,
    ) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>>;
}

/// Cost/savings summed across every round of a [`ChatBuilder::run_tools`] loop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AggregateCost {
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub baseline_cost_usd: f64,
}

impl AggregateCost {
    fn add(&mut self, c: &CostInfo) {
        let cost = c.cost_usd.unwrap_or(0.0);
        let saved = c.saved_usd.unwrap_or(0.0);
        self.cost_usd += cost;
        self.saved_usd += saved;
        self.baseline_cost_usd += c.baseline_cost_usd.unwrap_or(cost + saved);
    }

    /// `saved / baseline * 100`, or `None` when baseline is 0.
    #[must_use]
    pub fn savings_pct(&self) -> Option<f64> {
        if self.baseline_cost_usd > 0.0 {
            Some(self.saved_usd / self.baseline_cost_usd * 100.0)
        } else {
            None
        }
    }
}

/// The result of a completed [`ChatBuilder::run_tools`] loop.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// The final assistant response (its `tool_calls` is empty).
    pub response: ChatCompletionResponse,
    /// Cost/savings summed across every round.
    pub cost: AggregateCost,
    /// The full transcript: the builder's input messages plus every assistant
    /// tool-call message, tool result, and the final answer.
    pub messages: Vec<Message>,
    /// Gateway round-trips made (includes the forced final, if any).
    pub rounds: usize,
}

impl ToolOutcome {
    /// The final answer text (`choices[0].message.content`), if any.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.response.choices.first().and_then(|c| match &c.message {
            Message::Assistant {
                content: Some(MessageContent::Text(t)),
                ..
            } => Some(t.as_str()),
            _ => None,
        })
    }
}

/// One non-streamed call, returning the typed response + header cost.
/// `force_no_tools` sets `tool_choice:"none"` to force a text answer.
#[allow(clippy::too_many_arguments)]
async fn send_round(
    client: &Client,
    model: &str,
    messages: &[Message],
    tools: &[crate::Tool],
    tool_choice: Option<&ToolChoice>,
    force_no_tools: bool,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    tag: Option<&str>,
) -> Result<(ChatCompletionResponse, CostInfo)> {
    let mut body = build_body(model, messages, max_tokens, temperature, false);
    let none = ToolChoice::Auto("none".to_string());
    let effective = if force_no_tools { Some(&none) } else { tool_choice };
    inject_tools(&mut body, tools, effective);
    let mut req = client
        .http
        .post(format!("{}/v1/chat/completions", client.base))
        .bearer_auth(&client.key)
        .json(&body);
    if let Some(t) = tag {
        req = req.header("X-TokenTrimmer-Tag", t);
    }
    let resp = req.send().await.map_err(Error::Request)?;
    let cost = parse_cost(resp.headers());
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Status {
            status: status.as_u16(),
            body,
            cost: Box::new(cost),
        });
    }
    let response = resp
        .json::<ChatCompletionResponse>()
        .await
        .map_err(Error::Decode)?;
    Ok((response, cost))
}

impl ChatBuilder<'_> {
    /// Drive the agentic loop: advertise the builder's `.tools(...)`, execute the
    /// model's tool calls via `executor`, feed results back, until the model
    /// returns a text answer or `.max_tool_rounds(n)` is hit — then one forced
    /// `tool_choice:"none"` call guarantees a final text answer.
    ///
    /// # Errors
    /// Propagates the same [`Error`] as [`send`](Self::send) (gateway non-2xx /
    /// request / decode) — a gateway failure aborts the loop. Per-tool executor
    /// errors do NOT propagate; they are fed back to the model.
    pub async fn run_tools(self, executor: &(impl ToolExecutor + Sync)) -> Result<ToolOutcome> {
        if self.model.trim().is_empty() {
            return Err(Error::MissingModel);
        }
        let ChatBuilder {
            client,
            model,
            mut messages,
            max_tokens,
            temperature,
            tag,
            tools,
            tool_choice,
            max_tool_rounds,
        } = self;
        let tag = tag.as_deref();
        let mut cost = AggregateCost::default();
        let mut rounds = 0usize;

        for _ in 0..max_tool_rounds {
            let (response, ci) = send_round(
                client,
                &model,
                &messages,
                &tools,
                tool_choice.as_ref(),
                false,
                max_tokens,
                temperature,
                tag,
            )
            .await?;
            rounds += 1;
            cost.add(&ci);

            let msg = response.choices.first().map(|c| c.message.clone());
            let calls: Vec<ToolCall> = match &msg {
                Some(Message::Assistant { tool_calls, .. }) => tool_calls.clone(),
                _ => Vec::new(),
            };
            if calls.is_empty() {
                if let Some(m) = msg {
                    messages.push(m);
                }
                return Ok(ToolOutcome {
                    response,
                    cost,
                    messages,
                    rounds,
                });
            }
            // calls non-empty → msg is Some(Assistant)
            if let Some(m) = msg {
                messages.push(m);
            }
            for tc in &calls {
                let result = match executor.call(&tc.function.name, &tc.function.arguments).await {
                    Ok(s) => s,
                    Err(e) => json!({ "error": e.to_string() }).to_string(),
                };
                messages.push(tool_result(&tc.id, result));
            }
        }

        // Round cap hit: force a final text answer.
        let (response, ci) = send_round(
            client,
            &model,
            &messages,
            &tools,
            tool_choice.as_ref(),
            true,
            max_tokens,
            temperature,
            tag,
        )
        .await?;
        rounds += 1;
        cost.add(&ci);
        if let Some(c) = response.choices.first() {
            messages.push(c.message.clone());
        }
        Ok(ToolOutcome {
            response,
            cost,
            messages,
            rounds,
        })
    }
}
```

- [ ] **Step 3: Wire the module into `lib.rs`**

In `crates/client/src/lib.rs`, after the `use` block / before the first helper (e.g. right after the `pub use tt_shared::Usage;` line ~15), add:

```rust
mod tools;
pub use tools::{AggregateCost, ToolExecutor, ToolOutcome};
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p tt-client`
Expected: builds (a `dead_code`/unused-import warning is acceptable until the tests below exercise the loop).

- [ ] **Step 5: Write the happy-path test**

In `crates/client/src/tools.rs`, add the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{tool, user, Client};
    use httpmock::prelude::*;
    use serde_json::json;

    struct Canned(&'static str);
    #[async_trait]
    impl ToolExecutor for Canned {
        async fn call(
            &self,
            _name: &str,
            _arguments: &str,
        ) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.0.to_string())
        }
    }

    struct AlwaysErr;
    #[async_trait]
    impl ToolExecutor for AlwaysErr {
        async fn call(
            &self,
            _name: &str,
            _arguments: &str,
        ) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
            Err("tool exploded".into())
        }
    }

    fn tool_call_response() -> serde_json::Value {
        json!({
            "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
            "model": "gpt-4o-mini",
            "choices": [{ "index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{ "id": "call_1", "type": "function",
                    "function": { "name": "lookup", "arguments": "{\"q\":\"x\"}" } }]
            }}],
            "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
        })
    }

    fn text_response(text: &str) -> serde_json::Value {
        json!({
            "id": "c2", "object": "chat.completion", "created": 1_700_000_000_i64,
            "model": "gpt-4o-mini",
            "choices": [{ "index": 0, "finish_reason": "stop",
                "message": { "role": "assistant", "content": text } }],
            "usage": { "prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11 }
        })
    }

    #[tokio::test]
    async fn run_tools_executes_then_answers() {
        let server = MockServer::start_async().await;
        // More specific mock first: the request carrying the tool result returns text.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"tool\"");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-saved-usd", "0.0003")
                .json_body(text_response("It is sunny."));
        });
        // Round 1 (no tool result yet): broad mock returns a tool_call.
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-cost-usd", "0.0002")
                .header("x-tokentrimmer-saved-usd", "0.0006")
                .json_body(tool_call_response());
        });

        let client = Client::new(server.base_url(), "k");
        let out = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("weather?"))
            .tools(vec![tool("lookup", "look up", json!({"type":"object"}))])
            .run_tools(&Canned("sunny data"))
            .await
            .unwrap();

        assert_eq!(out.rounds, 2);
        assert_eq!(out.text(), Some("It is sunny."));
        // cost summed across both rounds: 0.0002 + 0.0001
        assert!((out.cost.cost_usd - 0.0003).abs() < 1e-9, "{}", out.cost.cost_usd);
        // transcript: [User, Assistant(tool_calls), Tool(result), Assistant(text)]
        assert_eq!(out.messages.len(), 4);
        assert!(matches!(&out.messages[2],
            Message::Tool { content: MessageContent::Text(t), .. } if t == "sunny data"));
        assert!(matches!(out.messages.last(),
            Some(Message::Assistant { content: Some(MessageContent::Text(t)), .. }) if t == "It is sunny."));
    }
}
```

- [ ] **Step 6: Run the happy-path test**

Run: `cargo test -p tt-client run_tools_executes_then_answers`
Expected: PASS.

- [ ] **Step 7: Write the cap + error + gateway-error tests**

Append inside the same `mod tests`:

```rust
    #[tokio::test]
    async fn run_tools_forces_answer_at_cap() {
        let server = MockServer::start_async().await;
        // Forced final (tool_choice:"none") returns text — created first.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"tool_choice\":\"none\"");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .json_body(text_response("Final."));
        });
        // Every other request keeps asking for a tool (never converges).
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .json_body(tool_call_response());
        });

        let client = Client::new(server.base_url(), "k");
        let out = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("go"))
            .tools(vec![tool("lookup", "look up", json!({"type":"object"}))])
            .max_tool_rounds(2)
            .run_tools(&Canned("data"))
            .await
            .unwrap();

        assert_eq!(out.rounds, 3); // 2 tool rounds + 1 forced
        assert_eq!(out.text(), Some("Final."));
        assert!((out.cost.cost_usd - 0.0003).abs() < 1e-9, "{}", out.cost.cost_usd);
        assert!(matches!(out.messages.last(),
            Some(Message::Assistant { content: Some(MessageContent::Text(t)), .. }) if t == "Final."));
    }

    #[tokio::test]
    async fn run_tools_feeds_executor_error_back() {
        let server = MockServer::start_async().await;
        // Request carrying the (error) tool result → final text.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"tool\"");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(text_response("Recovered."));
        });
        // Round 1: broad mock asks for a tool.
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(tool_call_response());
        });

        let client = Client::new(server.base_url(), "k");
        let out = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("go"))
            .tools(vec![tool("lookup", "look up", json!({"type":"object"}))])
            .run_tools(&AlwaysErr)
            .await
            .unwrap();

        assert_eq!(out.text(), Some("Recovered."));
        // the fed-back tool result carries the error, loop did NOT abort
        assert!(matches!(&out.messages[2],
            Message::Tool { content: MessageContent::Text(t), .. } if t.contains("error") && t.contains("exploded")));
    }

    #[tokio::test]
    async fn run_tools_propagates_gateway_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(500).body("boom");
        });
        let client = Client::new(server.base_url(), "k");
        let result = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("go"))
            .run_tools(&Canned("x"))
            .await;
        assert!(matches!(result, Err(Error::Status { status: 500, .. })));
    }
```

- [ ] **Step 8: Run the full tt-client suite**

Run: `cargo test -p tt-client`
Expected: PASS (all prior tests + the 4 new `run_tools*` tests + the surface/constructor tests).

- [ ] **Step 9: Commit**

```bash
git add crates/client/Cargo.toml crates/client/src/tools.rs crates/client/src/lib.rs
git commit -m "feat(tt-client): ToolExecutor trait + run_tools driver (loop, cost aggregation, forced final)"
```

---

### Task 4: Gates + finish the branch

**Files:** none (verification + PR)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `git diff --quiet || git commit -am "style: cargo fmt"`

- [ ] **Step 2: Clippy (workspace, all targets, deny warnings)**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0, no warnings. (If `clippy::too_many_arguments` fires on `send_round` despite the `#[allow]`, confirm the attribute is on the fn; if a `needless-borrow`/`is_some_and` pattern trips, fix per the usual gotchas.)

- [ ] **Step 3: Tests + advisories + docs**

Run: `cargo test -p tt-client`
Expected: all pass.
Run: `cargo deny check advisories`
Expected: ok (async-trait is already a vetted workspace dep).
Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-client --no-deps`
Expected: exit 0, no doc warnings.

- [ ] **Step 4: Finish the branch**

Use the **superpowers:finishing-a-development-branch** skill: verify tests, push `tool-calling-tt-client`, and create the PR (option 2). PR body should summarise the low-level surface + `run_tools` driver and the test plan.

- [ ] **Step 5: Adversarial review + CI**

After the PR is open, run a Workflow-based adversarial review (3 lenses — loop correctness/termination/cost-accumulation, error & resource handling, public-API/contract) with per-finding verification against the real source, and watch CI. Fix any confirmed findings on the branch before merge. Update the roadmap memory when green.

---

## Notes for the implementer

- **Same-crate privacy:** `tools.rs` reaches `build_body`, `inject_tools`, `parse_cost`, `tool_result`, and `ChatBuilder`'s private fields via `crate::…` — child modules can see ancestor-private items, so this compiles even though `inject_tools` is not `pub`.
- **`ToolChoice::Auto("none")`** serialises (untagged) to the bare string `"none"`, matching the OpenAI `tool_choice:"none"` wire form — the cap test's `body_contains("\"tool_choice\":\"none\"")` depends on this.
- **httpmock ordering:** the first-created matching mock wins, so always register the more specific `body_contains("\"role\":\"tool\"")` / `"tool_choice":"none"` mock *before* the broad catch-all.
- **No `expect`/`unwrap` in `run_tools`:** the two-phase `if let Some(m) = msg` avoids a double-move and any panic path (the empty-`calls` branch returns before the second `if let`).
