//! Agentic tool-calling driver for [`ChatBuilder::run_tools`]. The SDK is
//! tool-agnostic: the caller supplies a [`ToolExecutor`]; this module runs the
//! call -> execute -> re-send loop on the non-streaming path, accumulating cost.

use async_trait::async_trait;
use serde_json::json;

use crate::{
    build_body, inject_tools, parse_cost, tool_result, ChatBuilder, ChatCompletionResponse,
    Client, CostInfo, Error, Message, MessageContent, Result, Tool, ToolCall, ToolChoice,
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
    tools: &[Tool],
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
        assert!(
            (out.cost.cost_usd - 0.0003).abs() < 1e-9,
            "{}",
            out.cost.cost_usd
        );
        // transcript: [User, Assistant(tool_calls), Tool(result), Assistant(text)]
        assert_eq!(out.messages.len(), 4);
        assert!(matches!(&out.messages[2],
            Message::Tool { content: MessageContent::Text(t), .. } if t == "sunny data"));
        assert!(matches!(out.messages.last(),
            Some(Message::Assistant { content: Some(MessageContent::Text(t)), .. }) if t == "It is sunny."));
    }

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
        assert!(
            (out.cost.cost_usd - 0.0003).abs() < 1e-9,
            "{}",
            out.cost.cost_usd
        );
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
}
