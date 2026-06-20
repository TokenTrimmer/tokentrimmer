//! Driver for the server-side agent loop (`POST /v1/agent/runs`). Unlike the
//! single-request [`ChatBuilder::run_tools`] loop — which drives every
//! model->tool->model round-trip from the client — the gateway's agent-run
//! endpoint owns the loop server-side (mid-loop down-routing, judge-gated
//! summarize, substep cache). The client's job is therefore narrower: kick off
//! a run, and whenever the run pauses on a CLIENT (non-gateway) tool, execute
//! it via the caller's [`ToolExecutor`] and resume by POSTing the tool outputs,
//! until the run reaches a terminal answer.
//!
//! Cost differs from chat: the agent endpoint returns a single JSON [`Run`]
//! whose `usage` aggregates served cost across every server-side turn — there
//! are NO per-turn `x-tokentrimmer-*` headers — so the aggregate cost is read
//! from the response body, not the headers.

use serde::{Deserialize, Serialize};

use crate::{Client, Error, Message, MessageContent, Result, Tool, ToolCall, ToolExecutor};

/// Terminal (or paused) status of a run, mirroring the gateway's `RunStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The model returned a final, tool-call-free answer.
    Completed,
    /// The loop stopped without a final answer (`max_turns` reached, or a
    /// client tool surfaced but the gateway has no run store to pause into).
    Incomplete,
    /// A completion turn errored server-side.
    Failed,
    /// The run paused on a client (non-gateway) tool and is awaiting the
    /// caller's tool outputs (resume via `POST .../tool_outputs`).
    RequiresAction,
}

impl RunStatus {
    /// True once the run can no longer be resumed (anything but `RequiresAction`).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(self, RunStatus::RequiresAction)
    }
}

/// Accumulated token usage + served cost across every turn of a run, mirroring
/// the gateway's `RunUsage`. `cost_usd` is the SUM of each turn's served cost
/// (`x-tokentrimmer-cost-usd`); it is the agent-loop analogue of
/// [`AggregateCost::cost_usd`](crate::AggregateCost::cost_usd).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub cost_usd: f64,
}

/// A run record returned by `POST /v1/agent/runs` and the resume endpoint — the
/// gateway's `Run` view. The full `messages` transcript is included so the
/// caller sees the whole model/tool exchange.
#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub id: String,
    pub status: RunStatus,
    pub messages: Vec<Message>,
    pub turns: u32,
    pub usage: RunUsage,
    #[serde(default)]
    pub note: Option<String>,
    /// Summarizer measurement tax (USD) — a metering side-cost, never folded
    /// into `usage.cost_usd`. `None` when unmetered / no summarization.
    #[serde(default)]
    pub summarizer_tax_usd: Option<f64>,
}

impl Run {
    /// The final assistant text (`messages`' last assistant text block), if any.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.messages.iter().rev().find_map(|m| match m {
            Message::Assistant {
                content: Some(MessageContent::Text(t)),
                ..
            } => Some(t.as_str()),
            _ => None,
        })
    }

    /// The CLIENT tool calls the run is paused on: assistant `tool_calls` in the
    /// transcript that have NO answering `Message::Tool` block. The gateway
    /// answers its own (read-only gateway) tool calls inline before pausing, so
    /// the unanswered remainder are exactly the client tools the caller must run.
    ///
    /// Empty unless [`status`](Self::status) is `RequiresAction`.
    #[must_use]
    pub fn pending_tool_calls(&self) -> Vec<ToolCall> {
        let answered: std::collections::HashSet<&str> = self
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        self.messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { tool_calls, .. } => Some(tool_calls),
                _ => None,
            })
            .flatten()
            .filter(|tc| !answered.contains(tc.id.as_str()))
            .cloned()
            .collect()
    }
}

/// The result of driving an agent run to a terminal state (or hitting the
/// client-side resume cap).
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    /// The final run record (its `status` is terminal unless the resume cap was
    /// hit while the run was still `RequiresAction`).
    pub run: Run,
    /// Number of `POST .../tool_outputs` resume round-trips the client made.
    pub resume_rounds: usize,
}

impl AgentOutcome {
    /// The run's final answer text, if any. See [`Run::text`].
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.run.text()
    }
}

/// One tool output submitted to resume a paused run. Wire shape:
/// `{ "tool_call_id": "...", "output": "..." }`.
#[derive(Debug, Clone, Serialize)]
struct ToolOutput {
    tool_call_id: String,
    output: String,
}

/// Fluent builder for an agent run. See [`Client::agent`].
pub struct AgentBuilder<'a> {
    client: &'a Client,
    model: String,
    messages: Vec<Message>,
    tools: Vec<Tool>,
    /// Server-side per-run turn cap; `None` ⇒ the gateway default. Forwarded as
    /// the request's `max_turns` (the gateway clamps it to `[1, 32]`).
    max_turns: Option<u32>,
    tag: Option<String>,
    cost_limit: Option<f64>,
    interactive: bool,
    /// Client-side cap on resume (`tool_outputs`) round-trips before the driver
    /// returns the still-paused run, so a model that loops forever on client
    /// tools can't drive an unbounded number of resume requests.
    max_resume_rounds: usize,
}

impl<'a> AgentBuilder<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            model: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_turns: None,
            tag: None,
            cost_limit: None,
            interactive: false,
            max_resume_rounds: 8,
        }
    }

    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
    #[must_use]
    pub fn messages(mut self, messages: Vec<Message>) -> Self {
        self.messages = messages;
        self
    }
    #[must_use]
    pub fn message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }
    /// Advertise function `tools` to the model. A run only pauses on tools the
    /// gateway does NOT execute itself (its read-only gateway tools run
    /// server-side); the ones you list here that aren't gateway tools are the
    /// ones your [`ToolExecutor`] will be asked to run.
    #[must_use]
    pub fn tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        self.tools = tools.into_iter().collect();
        self
    }
    /// Server-side per-run turn cap (the gateway clamps to `[1, 32]`).
    #[must_use]
    pub fn max_turns(mut self, n: u32) -> Self {
        self.max_turns = Some(n);
        self
    }
    /// `X-TokenTrimmer-Tag` — free-form cost-attribution tag, forwarded on the
    /// initial create AND every resume request.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }
    /// `X-TokenTrimmer-Cost-Limit-Usd` — the gateway rejects with `402` if a
    /// turn's estimated cost exceeds `usd`.
    #[must_use]
    pub fn cost_limit(mut self, usd: f64) -> Self {
        self.cost_limit = Some(usd);
        self
    }
    /// Mark the run as **interactive** (a human is waiting): sends
    /// `X-TokenTrimmer-Interactive: 1`, clearing the gateway's advisory
    /// batch-eligibility marker.
    #[must_use]
    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self
    }
    /// Max client-side resume (`tool_outputs`) round-trips before the driver
    /// returns a still-paused run (default 8). A guard against a model that
    /// loops forever on client tools — independent of the server's `max_turns`.
    #[must_use]
    pub fn max_resume_rounds(mut self, n: usize) -> Self {
        self.max_resume_rounds = n;
        self
    }

    /// Drive the agent run to completion: create the run, then while it is
    /// paused on client tools, execute them via `executor` and resume — until a
    /// terminal status or the resume cap.
    ///
    /// # Errors
    /// [`Error::MissingModel`] if no model was set; [`Error::Request`] /
    /// [`Error::Status`] / [`Error::Decode`] on a gateway/transport failure of
    /// either the create or a resume call (a gateway failure aborts the run).
    /// Per-tool executor errors do NOT propagate — they are submitted back to
    /// the run as the tool's output so the model can recover.
    pub async fn run(self, executor: &(impl ToolExecutor + Sync)) -> Result<AgentOutcome> {
        if self.model.trim().is_empty() {
            return Err(Error::MissingModel);
        }
        let mut run = self.create().await?;
        let mut resume_rounds = 0usize;

        while run.status == RunStatus::RequiresAction && resume_rounds < self.max_resume_rounds {
            let pending = run.pending_tool_calls();
            // A `requires_action` run always has >=1 pending client tool; an
            // empty list would mean a server contract break. Stop rather than
            // POST an empty resume the gateway would reject (it requires the
            // outputs to exactly cover the pending ids).
            if pending.is_empty() {
                break;
            }
            let mut outputs = Vec::with_capacity(pending.len());
            for tc in &pending {
                let output = match executor
                    .call(&tc.function.name, &tc.function.arguments)
                    .await
                {
                    Ok(s) => s,
                    // Feed the error BACK as the tool output (mirrors the chat
                    // run_tools loop) so the model can react; never abort.
                    Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
                };
                outputs.push(ToolOutput {
                    tool_call_id: tc.id.clone(),
                    output,
                });
            }
            run = self.resume(&run.id, &outputs).await?;
            resume_rounds += 1;
        }

        Ok(AgentOutcome { run, resume_rounds })
    }

    /// POST the initial `/v1/agent/runs` request.
    async fn create(&self) -> Result<Run> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": self.messages,
            "stream": false,
        });
        if !self.tools.is_empty() {
            body["tools"] = serde_json::to_value(&self.tools).unwrap_or(serde_json::Value::Null);
        }
        if let Some(mt) = self.max_turns {
            body["max_turns"] = serde_json::json!(mt);
        }
        let url = format!("{}/v1/agent/runs", self.client.base);
        self.send_run(url, body).await
    }

    /// POST `/v1/agent/runs/{id}/tool_outputs` to resume a paused run.
    async fn resume(&self, run_id: &str, outputs: &[ToolOutput]) -> Result<Run> {
        let body = serde_json::json!({ "tool_outputs": outputs });
        let url = format!("{}/v1/agent/runs/{run_id}/tool_outputs", self.client.base);
        self.send_run(url, body).await
    }

    /// Shared POST helper: attach auth + tt headers, send, surface non-2xx as
    /// [`Error::Status`], decode the [`Run`] body.
    async fn send_run(&self, url: String, body: serde_json::Value) -> Result<Run> {
        let req = self
            .client
            .http
            .post(url)
            .bearer_auth(&self.client.key)
            .json(&body);
        let req =
            crate::apply_tt_headers(req, self.tag.as_deref(), self.cost_limit, self.interactive)?;
        let resp = req.send().await.map_err(Error::Request)?;
        let cost = crate::parse_cost(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
                cost: Box::new(cost),
            });
        }
        resp.json::<Run>().await.map_err(Error::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{async_trait, system, tool, user, Client};
    use httpmock::prelude::*;
    use serde_json::json;

    /// A canned client-tool executor returning a fixed string.
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

    /// A `requires_action` Run paused on a client tool `do_thing` (id `call_1`),
    /// whose assistant tool_call is unanswered in the transcript.
    fn requires_action_run() -> serde_json::Value {
        json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "status": "requires_action",
            "turns": 1,
            "usage": { "prompt_tokens": 10, "completion_tokens": 4, "cost_usd": 0.0002 },
            "messages": [
                { "role": "user", "content": "do it" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call_1", "type": "function",
                      "function": { "name": "do_thing", "arguments": "{\"x\":1}" } }
                ]}
            ]
        })
    }

    /// The terminal Run returned after resume: the tool output was appended and
    /// the model produced a final text answer. Aggregate cost spans both turns.
    fn completed_run_after_resume() -> serde_json::Value {
        json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "status": "completed",
            "turns": 2,
            "usage": { "prompt_tokens": 22, "completion_tokens": 9, "cost_usd": 0.0005 },
            "messages": [
                { "role": "user", "content": "do it" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call_1", "type": "function",
                      "function": { "name": "do_thing", "arguments": "{\"x\":1}" } }
                ]},
                { "role": "tool", "tool_call_id": "call_1", "content": "did the thing" },
                { "role": "assistant", "content": "All done." }
            ]
        })
    }

    #[test]
    fn pending_tool_calls_are_the_unanswered_assistant_calls() {
        let run: Run = serde_json::from_value(requires_action_run()).unwrap();
        let pending = run.pending_tool_calls();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "call_1");
        assert_eq!(pending[0].function.name, "do_thing");

        // After the answering Tool block is present, nothing is pending.
        let done: Run = serde_json::from_value(completed_run_after_resume()).unwrap();
        assert!(done.pending_tool_calls().is_empty());
        assert_eq!(done.text(), Some("All done."));
        assert!(done.status.is_terminal());
    }

    #[tokio::test]
    async fn run_executes_client_tool_then_resumes_to_completion() {
        let server = MockServer::start_async().await;
        // Resume endpoint (more specific path) → terminal completed run.
        let resume = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/agent/runs/11111111-1111-1111-1111-111111111111/tool_outputs")
                .body_includes("did the thing")
                .body_includes("call_1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(completed_run_after_resume());
        });
        // Create endpoint → paused requires_action run.
        let create = server.mock(|when, then| {
            when.method(POST).path("/v1/agent/runs");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(requires_action_run());
        });

        let client = Client::new(server.base_url(), "k");
        let out = client
            .agent()
            .model("gpt-4o-mini")
            .message(system("be helpful"))
            .message(user("do it"))
            .tools(vec![tool("do_thing", "does", json!({"type":"object"}))])
            .run(&Canned("did the thing"))
            .await
            .unwrap();

        create.assert();
        resume.assert();
        assert_eq!(out.resume_rounds, 1);
        assert_eq!(out.run.status, RunStatus::Completed);
        assert_eq!(out.text(), Some("All done."));
        // Aggregate cost is read from the terminal run body, not headers.
        assert!((out.run.usage.cost_usd - 0.0005).abs() < 1e-9);
        assert_eq!(out.run.usage.prompt_tokens, 22);
    }

    #[tokio::test]
    async fn run_returns_immediately_when_first_response_is_terminal() {
        // The gateway ran the whole loop server-side (only gateway tools) and
        // returned a completed run — no client tool, no resume.
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST).path("/v1/agent/runs");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "abc", "status": "completed", "turns": 3,
                    "usage": { "prompt_tokens": 30, "completion_tokens": 12, "cost_usd": 0.0009 },
                    "messages": [
                        { "role": "user", "content": "hi" },
                        { "role": "assistant", "content": "Answer." }
                    ]
                }));
        });
        let client = Client::new(server.base_url(), "k");
        let out = client
            .agent()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .run(&Canned("unused"))
            .await
            .unwrap();
        m.assert();
        assert_eq!(out.resume_rounds, 0);
        assert_eq!(out.text(), Some("Answer."));
    }

    #[tokio::test]
    async fn run_stops_at_resume_cap_when_run_keeps_pausing() {
        // The gateway never finishes: every create/resume returns the SAME
        // requires_action run. With max_resume_rounds(2) the driver makes the
        // create + exactly 2 resumes, then returns the still-paused run.
        let server = MockServer::start_async().await;
        // Both the create AND every resume return the same paused run; one mock
        // per path (httpmock `path` is an exact match).
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/agent/runs/11111111-1111-1111-1111-111111111111/tool_outputs");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(requires_action_run());
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/agent/runs");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(requires_action_run());
        });
        let client = Client::new(server.base_url(), "k");
        let out = client
            .agent()
            .model("gpt-4o-mini")
            .message(user("loop"))
            .tools(vec![tool("do_thing", "does", json!({"type":"object"}))])
            .max_resume_rounds(2)
            .run(&Canned("again"))
            .await
            .unwrap();
        assert_eq!(out.resume_rounds, 2);
        assert_eq!(out.run.status, RunStatus::RequiresAction);
    }

    #[tokio::test]
    async fn run_feeds_executor_error_back_as_tool_output() {
        let server = MockServer::start_async().await;
        // Resume carrying the ERROR output → terminal recovered run.
        let resume = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/agent/runs/11111111-1111-1111-1111-111111111111/tool_outputs")
                .body_includes("exploded");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(completed_run_after_resume());
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/agent/runs");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(requires_action_run());
        });
        let client = Client::new(server.base_url(), "k");
        let out = client
            .agent()
            .model("gpt-4o-mini")
            .message(user("go"))
            .tools(vec![tool("do_thing", "does", json!({"type":"object"}))])
            .run(&AlwaysErr)
            .await
            .unwrap();
        resume.assert(); // the error was submitted, the loop did NOT abort
        assert_eq!(out.run.status, RunStatus::Completed);
    }

    #[tokio::test]
    async fn run_without_model_errors_before_any_request() {
        let client = Client::new("http://127.0.0.1:1", "k");
        let err = client
            .agent()
            .message(user("hi"))
            .run(&Canned("x"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::MissingModel), "{err:?}");
    }

    #[tokio::test]
    async fn run_propagates_gateway_error_on_create() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/agent/runs");
            then.status(500).body("boom");
        });
        let client = Client::new(server.base_url(), "k");
        let result = client
            .agent()
            .model("gpt-4o-mini")
            .message(user("go"))
            .run(&Canned("x"))
            .await;
        assert!(matches!(result, Err(Error::Status { status: 500, .. })));
    }

    #[tokio::test]
    async fn run_propagates_gateway_error_on_resume() {
        // Create pauses fine; the resume fails → the error surfaces (run aborts).
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/agent/runs/11111111-1111-1111-1111-111111111111/tool_outputs");
            then.status(503).body("store unavailable");
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/agent/runs");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(requires_action_run());
        });
        let client = Client::new(server.base_url(), "k");
        let result = client
            .agent()
            .model("gpt-4o-mini")
            .message(user("go"))
            .tools(vec![tool("do_thing", "does", json!({"type":"object"}))])
            .run(&Canned("x"))
            .await;
        assert!(matches!(result, Err(Error::Status { status: 503, .. })));
    }

    #[tokio::test]
    async fn run_forwards_tag_and_max_turns() {
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/agent/runs")
                .header("x-tokentrimmer-tag", "proj-x")
                .body_includes("\"max_turns\":4");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "z", "status": "completed", "turns": 1,
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "cost_usd": 0.0 },
                    "messages": [{ "role": "assistant", "content": "ok" }]
                }));
        });
        let client = Client::new(server.base_url(), "k");
        client
            .agent()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .tag("proj-x")
            .max_turns(4)
            .run(&Canned("x"))
            .await
            .unwrap();
        m.assert();
    }
}
