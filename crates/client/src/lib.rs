//! Typed Rust client for the TokenTrimmer gateway. OpenAI-compatible chat that
//! returns a typed [`CostInfo`] parsed from the `x-tokentrimmer-*` headers, plus
//! the `X-TokenTrimmer-Tag` extension as a first-class builder option.

use futures::StreamExt as _;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use std::time::Duration;

// Re-export every tt-shared type reachable through the public API (so embedders
// don't need a direct `tt-shared` dependency to read `outcome.response`).
pub use tt_shared::messages::{
    ChatCompletionResponse, Choice, ContentPart, EmbeddingData, EmbeddingInput, EmbeddingsRequest,
    EmbeddingsResponse, ImageUrl, InputAudio, Message, MessageContent, Tool, ToolCall,
    ToolCallFunction, ToolChoice, ToolChoiceFunction, ToolFunction,
};
pub use tt_shared::Usage;

mod embeddings;
pub use embeddings::{EmbedBuilder, EmbedOutcome};

mod tools;
/// Re-exported so downstream `impl ToolExecutor` blocks can annotate with
/// `#[tt_client::async_trait]` without taking a direct `async-trait` dependency.
pub use async_trait::async_trait;
pub use tools::{AggregateCost, ToolExecutor, ToolOutcome};

/// Build a `user` message.
#[must_use]
pub fn user(content: impl Into<String>) -> Message {
    Message::User {
        content: MessageContent::Text(content.into()),
        name: None,
    }
}

/// Build a `system` message.
#[must_use]
pub fn system(content: impl Into<String>) -> Message {
    Message::System {
        content: MessageContent::Text(content.into()),
    }
}

/// Build an `assistant` message.
#[must_use]
pub fn assistant(content: impl Into<String>) -> Message {
    Message::Assistant {
        content: Some(MessageContent::Text(content.into())),
        tool_calls: Vec::new(),
        name: None,
    }
}

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

/// Cost/savings + routing metadata parsed from the gateway's `x-tokentrimmer-*`
/// response headers. Each field is `None` when its header is absent/unparseable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostInfo {
    pub cost_usd: Option<f64>,
    pub saved_usd: Option<f64>,
    pub baseline_cost_usd: Option<f64>,
    pub model_used: Option<String>,
    pub provider: Option<String>,
    pub trace_id: Option<String>,
    pub cache: Option<String>,
}

fn header_str(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
}

fn header_f64(h: &HeaderMap, name: &str) -> Option<f64> {
    header_str(h, name).and_then(|s| s.parse().ok())
}

/// Parse the gateway's cost/savings headers.
#[must_use]
pub fn parse_cost(headers: &HeaderMap) -> CostInfo {
    CostInfo {
        cost_usd: header_f64(headers, "x-tokentrimmer-cost-usd"),
        saved_usd: header_f64(headers, "x-tokentrimmer-saved-usd"),
        baseline_cost_usd: header_f64(headers, "x-tokentrimmer-baseline-cost-usd"),
        model_used: header_str(headers, "x-tokentrimmer-model-used"),
        provider: header_str(headers, "x-tokentrimmer-provider"),
        trace_id: header_str(headers, "x-tokentrimmer-trace-id"),
        cache: header_str(headers, "x-tokentrimmer-cache"),
    }
}

/// The `/v1/chat/completions` request body (non-streamed).
#[must_use]
pub fn build_body(
    model: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    stream: bool,
) -> Value {
    let mut body = json!({ "model": model, "messages": messages, "stream": stream });
    if let Some(mt) = max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    body
}

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

/// The terminal `tokentrimmer.usage` SSE event payload (streaming cost/usage).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StreamUsage {
    pub cost_usd: f64,
    pub baseline_cost_usd: f64,
    pub saved_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
}

/// An event yielded by [`ChatStream::next`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StreamEvent {
    /// A chunk of assistant text.
    Delta(String),
    /// Complete tool call(s) the model requested (emitted at finish).
    ToolCalls(Vec<ToolCall>),
    /// The terminal cost/usage event.
    Usage(StreamUsage),
}

/// Internal parse result for one SSE frame.
enum Frame {
    Delta(String),
    ToolCalls(Vec<ToolCall>),
    Usage(StreamUsage),
    Done,
    Ignore,
}

/// Parse a single SSE frame (the text between `\n\n` separators).
fn parse_sse_frame(frame: &str) -> Frame {
    let mut event_name: Option<&str> = None;
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event_name = Some(v.trim());
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    let data = data.trim();
    if data.is_empty() {
        return Frame::Ignore;
    }
    if data == "[DONE]" {
        return Frame::Done;
    }
    if event_name == Some("tokentrimmer.usage") {
        return serde_json::from_str::<StreamUsage>(data)
            .map(Frame::Usage)
            .unwrap_or(Frame::Ignore);
    }
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Frame::Ignore,
    };
    if let Some(tcs) = v["choices"][0]["delta"]["tool_calls"].as_array() {
        if !tcs.is_empty() {
            // We expect COMPLETE `ToolCall`s here: the TokenTrimmer gateway's
            // provider adapters reassemble fragmented upstream tool-call deltas
            // into whole tool calls before the SSE wire. A *raw* OpenAI-compatible
            // endpoint that streams partial deltas (`{index, function:{arguments:
            // "..."}}`) won't deserialize into a full `ToolCall` and is skipped —
            // streaming tool calls require routing through the gateway (see
            // `ChatRequest::stream` docs). Treated as `Ignore` rather than erroring
            // so a benign keep-alive/partial frame doesn't abort the stream.
            if let Ok(calls) = serde_json::from_value::<Vec<ToolCall>>(Value::Array(tcs.clone())) {
                if !calls.is_empty() {
                    return Frame::ToolCalls(calls);
                }
            }
        }
    }
    match v["choices"][0]["delta"]["content"].as_str() {
        Some(c) if !c.is_empty() => Frame::Delta(c.to_string()),
        _ => Frame::Ignore,
    }
}

/// Drain complete SSE frames (separated by a blank line) from the byte buffer.
/// Incomplete trailing bytes stay in `buf`, so a multi-byte UTF-8 char (or a
/// frame) split across network chunks is never decoded mid-sequence.
fn drain_frames(buf: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
        let frame: Vec<u8> = buf.drain(..idx + 2).collect();
        out.push(String::from_utf8_lossy(&frame).trim_end().to_string());
    }
    out
}

/// Errors from a [`Client`] call.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No model was set on the builder — call `.model(...)`.
    #[error("model is required — call `.model(...)`")]
    MissingModel,
    /// No input was set on the embeddings builder — call `.input(...)`.
    #[error("input is required — call `.input(...)`")]
    MissingInput,
    #[error("request to the gateway failed: {0}")]
    Request(#[source] reqwest::Error),
    /// A non-2xx response. `cost` carries the `x-tokentrimmer-*` telemetry
    /// (incl. `trace_id`) the gateway sends on errors too.
    #[error("gateway returned {status}: {body}")]
    Status {
        status: u16,
        body: String,
        cost: Box<CostInfo>,
    },
    #[error("failed to decode the gateway response: {0}")]
    Decode(#[source] reqwest::Error),
    /// The `tag` is not a valid HTTP header value. Rejected bytes are the
    /// control chars (`< 0x20`, incl. CR/LF/NUL) and DEL (`0x7F`); high bytes
    /// (`0x80..=0xFF`, e.g. non-ASCII UTF-8) pass through as opaque octets.
    #[error("invalid tag (not a valid HTTP header value): {0:?}")]
    InvalidTag(String),
    /// The cost limit is not a finite number (NaN / infinity).
    #[error("invalid cost limit (must be a finite number): {0}")]
    InvalidCostLimit(f64),
}

/// Result alias for `tt-client`.
pub type Result<T> = std::result::Result<T, Error>;

/// Attach the optional `X-TokenTrimmer-Tag` + `X-TokenTrimmer-Cost-Limit-Usd`
/// headers, validating both. Rejects a tag that isn't a legal HTTP header value
/// (control chars/CR/LF/DEL; high bytes pass as opaque octets) and a non-finite
/// cost limit — surfaced at send time, before any network I/O. A finite negative
/// cost limit is sent as-is; the gateway rejects it with 402.
pub(crate) fn apply_tt_headers(
    mut req: reqwest::RequestBuilder,
    tag: Option<&str>,
    cost_limit: Option<f64>,
) -> Result<reqwest::RequestBuilder> {
    if let Some(tag) = tag {
        let value = reqwest::header::HeaderValue::from_str(tag)
            .map_err(|_| Error::InvalidTag(tag.to_string()))?;
        req = req.header("X-TokenTrimmer-Tag", value);
    }
    if let Some(limit) = cost_limit {
        if !limit.is_finite() {
            return Err(Error::InvalidCostLimit(limit));
        }
        req = req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
    }
    Ok(req)
}

/// Fail fast if the gateway host is unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Read-INACTIVITY timeout (not a total cap): aborts a connection that goes
/// silent for this long — including the wait for the FIRST byte. Set generously
/// (10 min, matching the OpenAI/Anthropic SDK default) so it bounds a truly hung
/// gateway without aborting a reasoning model that thinks for a while before its
/// first token; a healthy stream that keeps emitting resets the window each chunk.
const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// A typed TokenTrimmer gateway client.
pub struct Client {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl Client {
    /// New client for `base` (e.g. `https://api.tokentrimmer.com`) with `key`.
    ///
    /// API-key precedence: the explicit `key` argument wins; pass an EMPTY string
    /// to fall back to the `TOKENTRIMMER_API_KEY` environment variable (also empty
    /// if that is unset — the gateway then rejects the request with `401`).
    ///
    /// Uses safe defaults: a 10s connect timeout, a 10-minute read-inactivity
    /// timeout (aborts a gateway that goes silent — incl. before the first byte —
    /// for 10 min, so a hung connection can't hang the caller forever, while
    /// still allowing a reasoning model that thinks for minutes before its first
    /// token), and a `tt-client/<version>` User-Agent. For different timeouts,
    /// retry, proxies, etc., configure a `reqwest::Client` yourself and use
    /// [`with_http_client`](Self::with_http_client).
    #[must_use]
    pub fn new(base: impl Into<String>, key: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .user_agent(concat!("tt-client/", env!("CARGO_PKG_VERSION")))
            .build()
            // Keep `new` infallible; the only failure is a rare TLS-backend init
            // error, where the plain default client is no worse than today.
            .unwrap_or_else(|_| reqwest::Client::new());
        // Explicit key wins; an empty key falls back to TOKENTRIMMER_API_KEY.
        let mut key = key.into();
        if key.is_empty() {
            if let Ok(env_key) = std::env::var("TOKENTRIMMER_API_KEY") {
                key = env_key;
            }
        }
        Self::with_http_client(http, base, key)
    }

    /// New client reusing an existing `reqwest::Client`.
    #[must_use]
    pub fn with_http_client(
        http: reqwest::Client,
        base: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            key: key.into(),
        }
    }

    /// Start building a chat completion.
    #[must_use]
    pub fn chat(&self) -> ChatBuilder<'_> {
        ChatBuilder {
            client: self,
            model: String::new(),
            messages: Vec::new(),
            max_tokens: None,
            temperature: None,
            tag: None,
            cost_limit: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tool_rounds: 8,
        }
    }
}

/// Fluent builder for a chat completion. See [`Client::chat`].
pub struct ChatBuilder<'a> {
    client: &'a Client,
    model: String,
    messages: Vec<Message>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    tag: Option<String>,
    cost_limit: Option<f64>,
    tools: Vec<Tool>,
    tool_choice: Option<ToolChoice>,
    max_tool_rounds: usize,
}

impl ChatBuilder<'_> {
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
    #[must_use]
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }
    #[must_use]
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }
    /// `X-TokenTrimmer-Tag` — free-form cost-attribution tag.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }
    /// `X-TokenTrimmer-Cost-Limit-Usd` — the gateway rejects the request with
    /// `402` if its estimated cost exceeds `usd`.
    #[must_use]
    pub fn cost_limit(mut self, usd: f64) -> Self {
        self.cost_limit = Some(usd);
        self
    }

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

    /// Send the request and return the typed response + cost.
    ///
    /// # Errors
    /// [`Error::MissingModel`] if no model was set, [`Error::Request`] on
    /// transport failure, [`Error::Status`] on a non-2xx response (carrying any
    /// cost/trace telemetry), [`Error::Decode`] if the body isn't a completion.
    pub async fn send(self) -> Result<ChatOutcome> {
        if self.model.trim().is_empty() {
            return Err(Error::MissingModel);
        }
        let mut body = build_body(
            &self.model,
            &self.messages,
            self.max_tokens,
            self.temperature,
            false,
        );
        inject_tools(&mut body, &self.tools, self.tool_choice.as_ref());
        let req = self
            .client
            .http
            .post(format!("{}/v1/chat/completions", self.client.base))
            .bearer_auth(&self.client.key)
            .json(&body);
        let req = apply_tt_headers(req, self.tag.as_deref(), self.cost_limit)?;
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
        Ok(ChatOutcome { response, cost })
    }

    /// Send the request and stream the response. Yields [`StreamEvent::Delta`]
    /// text chunks then the terminal [`StreamEvent::Usage`] cost event.
    ///
    /// # Streaming tool calls
    /// [`StreamEvent::ToolCalls`] are emitted only for *complete* tool calls.
    /// The TokenTrimmer gateway reassembles fragmented upstream tool-call deltas
    /// before the wire, so this works end-to-end through the gateway. If you point
    /// the client at a *raw* OpenAI-compatible endpoint that streams partial
    /// tool-call deltas, those partial frames are skipped (not surfaced) — route
    /// streaming tool calls through the gateway, or use the non-streamed
    /// [`send`](Self::send).
    ///
    /// # Errors
    /// Same as [`send`](Self::send): `MissingModel` / `Request` / `Status`.
    pub async fn stream(self) -> Result<ChatStream> {
        if self.model.trim().is_empty() {
            return Err(Error::MissingModel);
        }
        let body = build_body(
            &self.model,
            &self.messages,
            self.max_tokens,
            self.temperature,
            true,
        );
        let req = self
            .client
            .http
            .post(format!("{}/v1/chat/completions", self.client.base))
            .bearer_auth(&self.client.key)
            .json(&body);
        let req = apply_tt_headers(req, self.tag.as_deref(), self.cost_limit)?;
        let resp = req.send().await.map_err(Error::Request)?;
        let header_cost = parse_cost(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
                cost: Box::new(header_cost),
            });
        }
        Ok(ChatStream {
            inner: Box::pin(resp.bytes_stream()),
            buf: Vec::new(),
            pending: std::collections::VecDeque::new(),
            done: false,
            header_cost,
        })
    }
}

/// A live chat stream. Iterate with [`ChatStream::next`], or use
/// `futures::StreamExt` combinators via the [`futures::Stream`] impl
/// (`Item = Result<StreamEvent>`); it is also a [`futures::stream::FusedStream`].
pub struct ChatStream {
    inner: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
    pending: std::collections::VecDeque<StreamEvent>,
    done: bool,
    header_cost: CostInfo,
}

// `inner` is a boxed `dyn Stream` (not `Debug`), so derive can't apply — hand-roll
// one over the inspectable fields so `Result<ChatStream>` stays `unwrap_err`-able.
impl std::fmt::Debug for ChatStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatStream")
            .field("buf_len", &self.buf.len())
            .field("pending", &self.pending.len())
            .field("done", &self.done)
            .field("header_cost", &self.header_cost)
            .finish_non_exhaustive()
    }
}

impl ChatStream {
    /// Header-based cost/trace (`model_used`, `provider`, `trace_id`) — known
    /// before the body streams.
    #[must_use]
    pub fn header_cost(&self) -> &CostInfo {
        &self.header_cost
    }

    /// Parse every complete frame currently in `buf`, queueing the events.
    fn drain_into_pending(&mut self) {
        for frame in drain_frames(&mut self.buf) {
            match parse_sse_frame(&frame) {
                Frame::Delta(t) => self.pending.push_back(StreamEvent::Delta(t)),
                Frame::ToolCalls(t) => self.pending.push_back(StreamEvent::ToolCalls(t)),
                Frame::Usage(u) => self.pending.push_back(StreamEvent::Usage(u)),
                Frame::Done => self.done = true,
                Frame::Ignore => {}
            }
        }
    }

    /// The next [`StreamEvent`], or `None` at end of stream.
    ///
    /// # Errors
    /// [`Error::Request`] if the underlying byte stream errors.
    pub async fn next(&mut self) -> Result<Option<StreamEvent>> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Ok(Some(ev));
            }
            if self.done {
                return Ok(None);
            }
            match self.inner.next().await {
                Some(Ok(chunk)) => {
                    self.buf.extend_from_slice(&chunk);
                    self.drain_into_pending();
                }
                Some(Err(e)) => return Err(Error::Request(e)),
                None => {
                    // SSE allows a stream to end without a trailing blank line —
                    // EOF itself terminates the final event. Flush the residual
                    // frame (append a synthetic separator so `drain_frames` sees
                    // it) so a terminal `tokentrimmer.usage`/delta isn't dropped.
                    if !self.buf.is_empty() {
                        self.buf.extend_from_slice(b"\n\n");
                        self.drain_into_pending();
                    }
                    self.done = true;
                }
            }
        }
    }
}

impl futures::Stream for ChatStream {
    type Item = Result<StreamEvent>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        // `ChatStream: Unpin` (every field is Unpin), so we can take `&mut Self`.
        let this = self.get_mut();
        loop {
            if let Some(ev) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(ev)));
            }
            if this.done {
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buf.extend_from_slice(&chunk);
                    this.drain_into_pending();
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(Error::Request(e)))),
                Poll::Ready(None) => {
                    // Mirror next(): SSE may end without a trailing blank line, so
                    // flush the residual frame before terminating.
                    if !this.buf.is_empty() {
                        this.buf.extend_from_slice(b"\n\n");
                        this.drain_into_pending();
                    }
                    this.done = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl futures::stream::FusedStream for ChatStream {
    fn is_terminated(&self) -> bool {
        // Once `done` is set and the queue drains, poll_next only returns None.
        self.done && self.pending.is_empty()
    }
}

/// A completed chat call: the typed response plus parsed cost/savings.
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub response: ChatCompletionResponse,
    pub cost: CostInfo,
}

impl ChatOutcome {
    /// The first choice's assistant text, if any.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.response
            .choices
            .first()
            .and_then(|c| match &c.message {
                Message::Assistant {
                    content: Some(MessageContent::Text(t)),
                    ..
                } => Some(t.as_str()),
                _ => None,
            })
    }

    /// The first choice's requested tool calls (empty when the model asked for
    /// none, or there are no choices).
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCall] {
        match self.response.choices.first().map(|c| &c.message) {
            Some(Message::Assistant { tool_calls, .. }) => tool_calls,
            _ => &[],
        }
    }

    /// Savings as a percentage of the baseline, when both are known.
    #[must_use]
    pub fn savings_pct(&self) -> Option<f64> {
        match (self.cost.saved_usd, self.cost.baseline_cost_usd) {
            (Some(s), Some(b)) if b > 0.0 => Some(s / b * 100.0),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_helpers() {
        assert!(
            matches!(user("hi"), Message::User { content: MessageContent::Text(t), .. } if t == "hi")
        );
        assert!(
            matches!(system("s"), Message::System { content: MessageContent::Text(t) } if t == "s")
        );
        assert!(
            matches!(assistant("a"), Message::Assistant { content: Some(MessageContent::Text(t)), .. } if t == "a")
        );
    }

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

    #[test]
    fn inject_tools_adds_fields() {
        let mut body = json!({ "model": "m", "messages": [] });
        let tools = vec![tool("f", "desc", json!({"type":"object"}))];
        inject_tools(&mut body, &tools, Some(&ToolChoice::none()));
        assert_eq!(body["tools"][0]["function"]["name"], "f");
        assert_eq!(body["tool_choice"], "none");

        // empty tools + no choice → nothing added
        let mut bare = json!({ "model": "m" });
        inject_tools(&mut bare, &[], None);
        assert!(bare.get("tools").is_none());
        assert!(bare.get("tool_choice").is_none());
    }

    #[test]
    fn parse_cost_reads_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-tokentrimmer-cost-usd", "0.0001".parse().unwrap());
        h.insert("x-tokentrimmer-saved-usd", "0.0003".parse().unwrap());
        h.insert("x-tokentrimmer-model-used", "gpt-4o-mini".parse().unwrap());
        h.insert("x-tokentrimmer-cache", "miss".parse().unwrap());
        let c = parse_cost(&h);
        assert_eq!(c.cost_usd, Some(0.0001));
        assert_eq!(c.saved_usd, Some(0.0003));
        assert_eq!(c.model_used.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(c.cache.as_deref(), Some("miss"));
        assert_eq!(c.provider, None); // absent
        assert_eq!(parse_cost(&HeaderMap::new()), CostInfo::default());
    }

    #[test]
    fn parse_cost_ignores_non_numeric() {
        let mut h = HeaderMap::new();
        h.insert("x-tokentrimmer-cost-usd", "n/a".parse().unwrap());
        assert_eq!(parse_cost(&h).cost_usd, None);
    }

    use httpmock::prelude::*;

    fn sample_response() -> serde_json::Value {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1_700_000_000_i64,
            "model": "gpt-4o-mini",
            "choices": [{ "index": 0,
                "message": { "role": "assistant", "content": "Hello there." },
                "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }
        })
    }

    #[tokio::test]
    async fn send_returns_typed_response_and_cost() {
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("x-tokentrimmer-tag", "feature=demo"); // tag forwarded
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-baseline-cost-usd", "0.0004")
                .header("x-tokentrimmer-saved-usd", "0.0003")
                .json_body(sample_response());
        });

        let client = Client::new(server.base_url(), "tt_live_test");
        let out = client
            .chat()
            .model("gpt-4o-mini")
            .messages(vec![user("hi")])
            .tag("feature=demo")
            .send()
            .await
            .unwrap();

        m.assert();
        assert_eq!(out.text(), Some("Hello there."));
        assert_eq!(out.cost.cost_usd, Some(0.0001));
        assert!((out.savings_pct().unwrap() - 75.0).abs() < 1e-9);
        assert_eq!(out.response.model, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn stream_yields_deltas_then_usage() {
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "event: tokentrimmer.usage\n",
            "data: {\"cost_usd\":0.0001,\"baseline_cost_usd\":0.0004,\"saved_usd\":0.0003,\"input_tokens\":10,\"output_tokens\":2,\"cached_tokens\":0}\n\n",
            "data: [DONE]\n\n",
        );
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"stream\":true");
            then.status(200)
                .header("content-type", "text/event-stream")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .body(sse);
        });

        let client = Client::new(server.base_url(), "k");
        let mut stream = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .stream()
            .await
            .unwrap();
        assert_eq!(
            stream.header_cost().model_used.as_deref(),
            Some("gpt-4o-mini")
        );

        let mut text = String::new();
        let mut usage: Option<StreamUsage> = None;
        while let Some(ev) = stream.next().await.unwrap() {
            match ev {
                StreamEvent::Delta(t) => text.push_str(&t),
                StreamEvent::ToolCalls(_) => {}
                StreamEvent::Usage(u) => usage = Some(u),
            }
        }
        assert_eq!(text, "Hello");
        let u = usage.expect("usage event");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 2);
    }

    #[tokio::test]
    async fn stream_yields_tool_calls() {
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Let me check\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "event: tokentrimmer.usage\n",
            "data: {\"cost_usd\":0.0001,\"baseline_cost_usd\":0.0004,\"saved_usd\":0.0003,\"input_tokens\":10,\"output_tokens\":2,\"cached_tokens\":0}\n\n",
            "data: [DONE]\n\n",
        );
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"stream\":true");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse);
        });

        let client = Client::new(server.base_url(), "k");
        let mut stream = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("weather in SF?"))
            .stream()
            .await
            .unwrap();

        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage: Option<StreamUsage> = None;
        while let Some(ev) = stream.next().await.unwrap() {
            match ev {
                StreamEvent::Delta(t) => text.push_str(&t),
                StreamEvent::ToolCalls(t) => tool_calls = t,
                StreamEvent::Usage(u) => usage = Some(u),
            }
        }
        assert_eq!(text, "Let me check");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].function.name, "get_weather");
        assert_eq!(tool_calls[0].function.arguments, "{\"city\":\"SF\"}");
        assert!(usage.is_some());
    }

    #[tokio::test]
    async fn stream_surfaces_status_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(500).body("boom");
        });
        let client = Client::new(server.base_url(), "k");
        let result = client.chat().model("m").message(user("hi")).stream().await;
        assert!(matches!(result, Err(Error::Status { status: 500, .. })));
    }

    #[tokio::test]
    async fn stream_flushes_unterminated_final_frame_at_eof() {
        // Server closes right after the usage frame, with NO trailing blank line.
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "event: tokentrimmer.usage\n",
            "data: {\"cost_usd\":0.0001,\"baseline_cost_usd\":0.0004,\"saved_usd\":0.0003,\"input_tokens\":7,\"output_tokens\":1,\"cached_tokens\":0}",
        );
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse);
        });

        let client = Client::new(server.base_url(), "k");
        let mut stream = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .stream()
            .await
            .unwrap();

        let mut text = String::new();
        let mut usage: Option<StreamUsage> = None;
        while let Some(ev) = stream.next().await.unwrap() {
            match ev {
                StreamEvent::Delta(t) => text.push_str(&t),
                StreamEvent::ToolCalls(_) => {}
                StreamEvent::Usage(u) => usage = Some(u),
            }
        }
        assert_eq!(text, "Hi");
        assert_eq!(
            usage
                .expect("terminal usage event survives EOF")
                .input_tokens,
            7
        );
    }

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
            .tools(vec![tool(
                "get_weather",
                "Look up weather",
                json!({"type":"object"}),
            )])
            .send()
            .await
            .unwrap();

        assert_eq!(out.tool_calls().len(), 1);
        assert_eq!(out.tool_calls()[0].function.name, "get_weather");
        assert!(out.text().is_none()); // content was null
    }

    #[tokio::test]
    async fn send_sends_cost_limit_header() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("x-tokentrimmer-cost-limit-usd", "0.05");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(sample_response());
        });
        let client = Client::new(server.base_url(), "k");
        let out = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .cost_limit(0.05)
            .send()
            .await
            .unwrap();
        assert_eq!(out.response.model, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn cost_limit_402_surfaces_as_status() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(402).body("cost limit exceeded");
        });
        let client = Client::new(server.base_url(), "k");
        let result = client
            .chat()
            .model("m")
            .message(user("hi"))
            .cost_limit(0.0001)
            .send()
            .await;
        assert!(matches!(result, Err(Error::Status { status: 402, .. })));
    }

    #[tokio::test]
    async fn stream_sends_cost_limit_header() {
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("x-tokentrimmer-cost-limit-usd", "0.05");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse);
        });
        let client = Client::new(server.base_url(), "k");
        let mut stream = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .cost_limit(0.05)
            .stream()
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(ev) = stream.next().await.unwrap() {
            if let StreamEvent::Delta(t) = ev {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "Hi");
    }

    #[tokio::test]
    async fn stream_impl_collects_deltas_via_combinators() {
        use futures::StreamExt;
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse);
        });
        let client = Client::new(server.base_url(), "k");
        let stream = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .stream()
            .await
            .unwrap();
        // Exercises Stream::poll_next via filter_map + collect (no inherent next()).
        let text = stream
            .filter_map(|ev| async move {
                match ev {
                    Ok(StreamEvent::Delta(t)) => Some(t),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .await
            .concat();
        assert_eq!(text, "Hello");
    }

    #[tokio::test]
    async fn stream_is_terminated_after_exhaustion() {
        use futures::stream::FusedStream;
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse);
        });
        let client = Client::new(server.base_url(), "k");
        let mut stream = client
            .chat()
            .model("m")
            .message(user("hi"))
            .stream()
            .await
            .unwrap();
        assert!(!stream.is_terminated(), "fresh stream is not terminated");
        // Drain via the inherent next() until end.
        while stream.next().await.unwrap().is_some() {}
        assert!(stream.is_terminated(), "stream is terminated after None");
    }

    #[tokio::test]
    async fn stream_impl_flushes_terminal_frame_on_eof() {
        use futures::StreamExt;
        let server = MockServer::start_async().await;
        // No trailing blank line, no [DONE] — EOF must flush the last frame.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"end\"}}]}";
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse);
        });
        let client = Client::new(server.base_url(), "k");
        let stream = client
            .chat()
            .model("m")
            .message(user("hi"))
            .stream()
            .await
            .unwrap();
        let text = stream
            .filter_map(|ev| async move {
                match ev {
                    Ok(StreamEvent::Delta(t)) => Some(t),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .await
            .concat();
        assert_eq!(text, "end");
    }

    #[tokio::test]
    async fn send_without_model_errors_before_any_request() {
        // no .model() → MissingModel, no network touched (dead base is fine)
        let client = Client::new("http://127.0.0.1:1", "k");
        let err = client.chat().message(user("hi")).send().await.unwrap_err();
        assert!(matches!(err, Error::MissingModel), "{err:?}");
    }

    #[tokio::test]
    async fn send_surfaces_status_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(429).body("rate limited");
        });
        let client = Client::new(server.base_url(), "k");
        let err = client
            .chat()
            .model("m")
            .message(user("hi"))
            .send()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Status { status: 429, .. }), "{err:?}");
    }

    #[test]
    fn parse_sse_frames() {
        assert!(matches!(
            parse_sse_frame(r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#),
            Frame::Delta(t) if t == "Hi"
        ));
        let usage = "event: tokentrimmer.usage\ndata: {\"cost_usd\":0.0001,\"baseline_cost_usd\":0.0004,\"saved_usd\":0.0003,\"input_tokens\":10,\"output_tokens\":20,\"cached_tokens\":0}";
        assert!(matches!(
            parse_sse_frame(usage),
            Frame::Usage(u) if u.input_tokens == 10 && (u.saved_usd - 0.0003).abs() < 1e-9
        ));
        assert!(matches!(parse_sse_frame("data: [DONE]"), Frame::Done));
        // A tool-calls delta frame → Frame::ToolCalls with the complete call.
        let tool = r#"data: {"choices":[{"delta":{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"SF\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        assert!(matches!(
            parse_sse_frame(tool),
            Frame::ToolCalls(calls)
                if calls.len() == 1
                    && calls[0].id == "call_1"
                    && calls[0].function.name == "get_weather"
                    && calls[0].function.arguments == "{\"city\":\"SF\"}"
        ));
        assert!(matches!(
            parse_sse_frame(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#),
            Frame::Ignore
        ));
        assert!(matches!(parse_sse_frame(""), Frame::Ignore));
    }

    #[test]
    fn drain_frames_handles_chunk_split_multibyte() {
        let full = "data: {\"choices\":[{\"delta\":{\"content\":\"café\"}}]}\n\n".as_bytes();
        let (a, b) = full.split_at(full.len() - 3);
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(a);
        assert!(drain_frames(&mut buf).is_empty(), "no complete frame yet");
        buf.extend_from_slice(b);
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert!(matches!(parse_sse_frame(&frames[0]), Frame::Delta(t) if t == "café"));
        assert!(buf.is_empty());
    }

    #[test]
    fn build_body_shape() {
        let b = build_body("gpt-4o", &[user("hi")], None, None, false);
        assert_eq!(b["model"], "gpt-4o");
        assert_eq!(b["stream"], false);
        assert!(b["messages"].is_array());
        assert!(b.get("max_tokens").is_none());
        let b2 = build_body("m", &[], Some(256), Some(0.2), true);
        assert_eq!(b2["stream"], true);
        assert_eq!(b2["max_tokens"], 256);
        assert!((b2["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn apply_tt_headers_accepts_valid_and_rejects_invalid() {
        let http = reqwest::Client::new();
        let url = "http://127.0.0.1:0/x";
        assert!(super::apply_tt_headers(http.get(url), Some("team-a"), Some(0.5)).is_ok());
        assert!(super::apply_tt_headers(http.get(url), None, None).is_ok());
        let e = super::apply_tt_headers(http.get(url), Some("bad\ntag"), None).unwrap_err();
        assert!(matches!(e, Error::InvalidTag(_)), "{e:?}");
        let nan = super::apply_tt_headers(http.get(url), None, Some(f64::NAN)).unwrap_err();
        assert!(matches!(nan, Error::InvalidCostLimit(_)), "{nan:?}");
        let inf = super::apply_tt_headers(http.get(url), None, Some(f64::INFINITY)).unwrap_err();
        assert!(matches!(inf, Error::InvalidCostLimit(_)), "{inf:?}");
    }

    #[tokio::test]
    async fn invalid_tag_fails_before_any_request() {
        let server = httpmock::MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST)
                    .path("/v1/chat/completions");
                then.status(200).json_body(serde_json::json!({
                    "id":"x","object":"chat.completion","created":0,"model":"gpt-4o-mini",
                    "choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}
                }));
            })
            .await;
        let client = Client::new(server.base_url(), "tt_test_k");
        let err = client
            .chat()
            .model("gpt-4o-mini")
            .messages(vec![])
            .tag("bad\ntag")
            .send()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTag(_)), "{err:?}");
        m.assert_hits_async(0).await;
    }

    #[tokio::test]
    async fn read_timeout_surfaces_as_timeout_not_hang() {
        use std::time::Duration;
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .delay(Duration::from_secs(2))
                .json_body(serde_json::json!({
                    "id":"x","object":"chat.completion","created":0,"model":"m",
                    "choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}
                }));
        });
        let http = reqwest::Client::builder()
            .read_timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let client = Client::with_http_client(http, server.base_url(), "k");
        let result = client.chat().model("m").message(user("hi")).send().await;
        match result {
            Err(Error::Request(e)) => assert!(e.is_timeout(), "expected timeout, got {e:?}"),
            other => panic!("expected Err(Request(timeout)), got {other:?}"),
        }
    }

    #[test]
    fn new_does_not_panic() {
        // The default builder config (connect/read timeouts + user-agent) is
        // valid, so `new` builds and never falls through to the panic-free
        // fallback for a bad reason. (reqwest exposes no getter to assert the
        // configured timeouts, so this only guards against a build/panic
        // regression — the read-timeout behaviour is covered by the test above.)
        let _client = Client::new("http://127.0.0.1:0", "tt_test_k");
    }

    // --- API-key resolution -------------------------------------------------
    // Precedence: an explicit `key` arg wins; an EMPTY key falls back to the
    // `TOKENTRIMMER_API_KEY` env var. These tests mutate that process-global env
    // var, so they serialize on a shared lock (the multi-threaded runner would
    // otherwise race them). The `tests` module lives inside the crate, so it can
    // read `Client`'s private `key` field directly — the most precise assertion
    // of what `new` resolved (no network needed).
    static KEY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn explicit_key_wins_over_env() {
        let _guard = KEY_ENV_LOCK.lock().expect("env lock");
        std::env::set_var("TOKENTRIMMER_API_KEY", "tt_from_env");
        let client = Client::new("http://127.0.0.1:0", "tt_explicit");
        std::env::remove_var("TOKENTRIMMER_API_KEY");
        assert_eq!(client.key, "tt_explicit");
    }

    #[test]
    fn empty_key_falls_back_to_env() {
        let _guard = KEY_ENV_LOCK.lock().expect("env lock");
        std::env::set_var("TOKENTRIMMER_API_KEY", "tt_from_env");
        let client = Client::new("http://127.0.0.1:0", "");
        std::env::remove_var("TOKENTRIMMER_API_KEY");
        assert_eq!(client.key, "tt_from_env");
    }

    #[test]
    fn empty_key_with_no_env_stays_empty() {
        let _guard = KEY_ENV_LOCK.lock().expect("env lock");
        std::env::remove_var("TOKENTRIMMER_API_KEY");
        // No explicit key and no env var → the key stays empty (the gateway then
        // rejects the request with 401). Crucially it must NOT pick up a stale
        // value from anywhere.
        let client = Client::new("http://127.0.0.1:0", "");
        assert_eq!(client.key, "");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn resolved_env_key_is_sent_as_bearer() {
        // End-to-end check that the env-resolved key reaches the wire as the
        // Authorization bearer (complements the field-level tests above).
        let _guard = KEY_ENV_LOCK.lock().expect("env lock");
        std::env::set_var("TOKENTRIMMER_API_KEY", "tt_from_env");
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("authorization", "Bearer tt_from_env");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(sample_response());
        });
        let result = Client::new(server.base_url(), "")
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .send()
            .await;
        std::env::remove_var("TOKENTRIMMER_API_KEY");
        result.unwrap();
        m.assert();
    }
}
