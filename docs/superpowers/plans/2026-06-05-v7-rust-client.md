# V7 — `tt-client` Rust SDK Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new `tt-client` crate: a typed Rust client over the gateway with a fluent chat builder and typed cost/savings.

**Architecture:** New `crates/client` (lib only). Pure `parse_cost`/`build_body`/message-helpers (tested), then `Client` + `ChatBuilder::send` (httpmock-tested). Reuses `tt-shared` types.

**Tech Stack:** Rust, `reqwest`, `serde_json`, `thiserror`, `tt-shared`; `httpmock` + `tokio` (dev).

---

### Task 1: crate scaffold + pure pieces (test-first)

**Files:**
- Modify: `Cargo.toml` (workspace member)
- Create: `crates/client/Cargo.toml`
- Create: `crates/client/src/lib.rs`

- [ ] **Step 1: Add the workspace member + crate manifest**

In the root `Cargo.toml` `members`, add (keeping it near the other leaf crates, e.g. after `"crates/tokenize",`):

```toml
    "crates/client",
```

Create `crates/client/Cargo.toml`:

```toml
[package]
name = "tt-client"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Typed Rust client for the TokenTrimmer gateway (OpenAI-compatible chat + cost/savings surfacing)."

[dependencies]
tt-shared.workspace = true
reqwest.workspace = true
serde_json.workspace = true
thiserror.workspace = true

[dev-dependencies]
tokio = { workspace = true }
httpmock.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Create `lib.rs` with the pure pieces + tests**

Create `crates/client/src/lib.rs`:

```rust
//! Typed Rust client for the TokenTrimmer gateway. OpenAI-compatible chat that
//! returns a typed [`CostInfo`] parsed from the `x-tokentrimmer-*` headers, plus
//! the `X-TokenTrimmer-Tag`/`-Route` extensions as first-class builder options.

use reqwest::header::HeaderMap;
use serde_json::{json, Value};

pub use tt_shared::messages::Message;
use tt_shared::messages::MessageContent;

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
) -> Value {
    let mut body = json!({ "model": model, "messages": messages, "stream": false });
    if let Some(mt) = max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_helpers() {
        assert!(matches!(user("hi"), Message::User { content: MessageContent::Text(t), .. } if t == "hi"));
        assert!(matches!(system("s"), Message::System { content: MessageContent::Text(t) } if t == "s"));
        assert!(matches!(assistant("a"), Message::Assistant { content: Some(MessageContent::Text(t)), .. } if t == "a"));
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

    #[test]
    fn build_body_shape() {
        let b = build_body("gpt-4o", &[user("hi")], None, None);
        assert_eq!(b["model"], "gpt-4o");
        assert_eq!(b["stream"], false);
        assert!(b["messages"].is_array());
        assert!(b.get("max_tokens").is_none());
        let b2 = build_body("m", &[], Some(256), Some(0.2));
        assert_eq!(b2["max_tokens"], 256);
        assert_eq!(b2["temperature"], 0.2);
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p tt-client 2>&1 | tail -12`
Expected: PASS (`message_helpers`, `parse_cost_reads_headers`, `parse_cost_ignores_non_numeric`, `build_body_shape`).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/client/Cargo.toml crates/client/src/lib.rs Cargo.lock
git commit -m "feat(tt-client): new crate — cost-header parse + request body + helpers"
```

---

### Task 2: `Client` + `ChatBuilder::send` + `ChatOutcome` + httpmock test

**Files:**
- Modify: `crates/client/src/lib.rs`

- [ ] **Step 1: Add the client, builder, outcome, error**

In `crates/client/src/lib.rs`, add the `ChatCompletionResponse` import to the `use tt_shared::messages::…` line:

```rust
use tt_shared::messages::{ChatCompletionResponse, MessageContent};
```

Add after `build_body` (before `#[cfg(test)]`):

```rust
/// Errors from a [`Client`] call.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request to the gateway failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("gateway returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("failed to decode the gateway response: {0}")]
    Decode(#[source] reqwest::Error),
}

/// Result alias for `tt-client`.
pub type Result<T> = std::result::Result<T, Error>;

/// A typed TokenTrimmer gateway client.
pub struct Client {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl Client {
    /// New client for `base` (e.g. `https://api.tokentrimmer.com`) with `key`.
    #[must_use]
    pub fn new(base: impl Into<String>, key: impl Into<String>) -> Self {
        Self::with_http_client(reqwest::Client::new(), base, key)
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
            route: None,
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
    route: Option<String>,
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
    /// `X-TokenTrimmer-Route` — force a named route.
    #[must_use]
    pub fn route(mut self, route: impl Into<String>) -> Self {
        self.route = Some(route.into());
        self
    }

    /// Send the request and return the typed response + cost.
    pub async fn send(self) -> Result<ChatOutcome> {
        let body = build_body(&self.model, &self.messages, self.max_tokens, self.temperature);
        let mut req = self
            .client
            .http
            .post(format!("{}/v1/chat/completions", self.client.base))
            .bearer_auth(&self.client.key)
            .json(&body);
        if let Some(tag) = &self.tag {
            req = req.header("X-TokenTrimmer-Tag", tag);
        }
        if let Some(route) = &self.route {
            req = req.header("X-TokenTrimmer-Route", route);
        }
        let resp = req.send().await.map_err(Error::Request)?;
        let cost = parse_cost(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
            });
        }
        let response = resp
            .json::<ChatCompletionResponse>()
            .await
            .map_err(Error::Decode)?;
        Ok(ChatOutcome { response, cost })
    }
}

/// A completed chat call: the typed response plus parsed cost/savings.
pub struct ChatOutcome {
    pub response: ChatCompletionResponse,
    pub cost: CostInfo,
}

impl ChatOutcome {
    /// The first choice's assistant text, if any.
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

    /// Savings as a percentage of the baseline, when both are known.
    #[must_use]
    pub fn savings_pct(&self) -> Option<f64> {
        match (self.cost.saved_usd, self.cost.baseline_cost_usd) {
            (Some(s), Some(b)) if b > 0.0 => Some(s / b * 100.0),
            _ => None,
        }
    }
}
```

- [ ] **Step 2: Add the integration + outcome tests**

Add to `lib.rs` `mod tests`:

```rust
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
        assert_eq!(out.savings_pct(), Some(75.0));
        assert_eq!(out.response.model, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn send_surfaces_status_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(429).body("rate limited");
        });
        let client = Client::new(server.base_url(), "k");
        let err = client.chat().model("m").message(user("hi")).send().await.unwrap_err();
        assert!(matches!(err, Error::Status { status: 429, .. }), "{err:?}");
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p tt-client 2>&1 | tail -14`
Expected: PASS (the 4 unit tests + `send_returns_typed_response_and_cost` + `send_surfaces_status_error`).

- [ ] **Step 4: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(tt-client): Client + fluent chat builder + ChatOutcome (httpmock)"
```

---

### Task 3: Gates + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy (workspace)**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings. Commit any fmt diff.

- [ ] **Step 2: Build + test the new crate + a workspace test sanity**

Run: `cargo test -p tt-client 2>&1 | grep -E "test result|error\[" | tail` then `cargo build --workspace 2>&1 | grep -E "^error" | head`
Expected: tt-client tests pass; workspace builds (the new member compiles).

- [ ] **Step 3: cargo-deny**

Run: `cargo deny check advisories 2>&1 | tail -3`
Expected: `advisories ok` (no new external deps — all are existing workspace deps).

- [ ] **Step 4: Doc example sanity (optional)**

Run: `cargo doc -p tt-client --no-deps 2>&1 | grep -E "^error|^warning" | head` (confirms the crate docs build).
Expected: no errors.

- [ ] **Step 5: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** crate scaffold + `parse_cost`/`CostInfo`/`build_body`/message helpers (T1), `Client`/`ChatBuilder`/`ChatOutcome`/`Error`/`send` (T2), gates (T3). All spec items covered.
- **Placeholders:** none — full code throughout.
- **Type consistency:** `parse_cost(&HeaderMap)->CostInfo`, `build_body(&str,&[Message],Option<u32>,Option<f32>)->Value`, `user/system/assistant(impl Into<String>)->Message`, `Client::{new,with_http_client,chat}`, `ChatBuilder::{model,messages,message,max_tokens,temperature,tag,route,send}`, `ChatOutcome::{text->Option<&str>,savings_pct->Option<f64>}`, `Error` (thiserror) + `Result<T>`. Reuses `tt_shared::messages::{Message, MessageContent, ChatCompletionResponse}`.
- **No new deps:** all are existing workspace deps (`tt-shared`, `reqwest`, `serde_json`, `thiserror`, dev `tokio`/`httpmock`), so `cargo deny` is unaffected.
