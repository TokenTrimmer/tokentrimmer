# V7b — `tt-client` Streaming Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `ChatBuilder::stream()` to `tt-client`: a `ChatStream` yielding `StreamEvent::{Delta, Usage}`, porting V5a's SSE parsing.

**Architecture:** All in `crates/client/src/lib.rs`. `build_body` gains a `stream` flag; ported `parse_sse_frame`/`drain_frames`; `ChatStream` over `resp.bytes_stream()`.

**Tech Stack:** Rust, `futures` + `bytes` (workspace deps), `reqwest` streaming, the V5a SSE logic.

---

### Task 1: deps + `build_body` stream flag

**Files:**
- Modify: `crates/client/Cargo.toml`
- Modify: `crates/client/src/lib.rs`

- [ ] **Step 1: Add `futures` + `bytes` deps**

In `crates/client/Cargo.toml` `[dependencies]`, after `thiserror.workspace = true`:

```toml
futures.workspace = true
bytes.workspace = true
```

- [ ] **Step 2: Add a `stream` param to `build_body`**

Change `build_body` so `stream` is a parameter (replace the hardcoded `false`):

```rust
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
```

- [ ] **Step 3: Update the `send()` call + the `build_body_shape` test**

In `send()`, change the call to pass `false`:

```rust
        let body = build_body(
            &self.model,
            &self.messages,
            self.max_tokens,
            self.temperature,
            false,
        );
```

In the `build_body_shape` test, update both calls + assert the flag:

```rust
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
```

- [ ] **Step 4: Build + tests**

Run: `cargo test -p tt-client 2>&1 | grep -E "test result|error\[" | tail`
Expected: all pass (build_body_shape updated; send unchanged behaviour).

- [ ] **Step 5: Commit**

```bash
git add crates/client/Cargo.toml crates/client/src/lib.rs Cargo.lock
git commit -m "feat(tt-client): build_body stream flag + futures/bytes deps"
```

---

### Task 2: SSE types + `parse_sse_frame` + `drain_frames` (test-first)

**Files:**
- Modify: `crates/client/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Add to `lib.rs` `mod tests`:

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tt-client parse_sse_frames 2>&1 | tail -8`
Expected: FAIL to compile — `cannot find … Frame` / `parse_sse_frame` / `drain_frames`.

- [ ] **Step 3: Add the SSE types + parsing**

In `lib.rs`, add after `build_body` (before the `Error` enum):

```rust
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
    /// The terminal cost/usage event.
    Usage(StreamUsage),
}

/// Internal parse result for one SSE frame.
enum Frame {
    Delta(String),
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
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p tt-client 2>&1 | grep -E "test result|FAILED" | head`
Expected: PASS (the two new tests + the existing suite).

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(tt-client): SSE frame types + parse_sse_frame + drain_frames (ported)"
```

---

### Task 3: `stream()` + `ChatStream` + httpmock SSE test

**Files:**
- Modify: `crates/client/src/lib.rs`

- [ ] **Step 1: Add the `futures::StreamExt` import**

At the top of `lib.rs` (with the other `use`s):

```rust
use futures::StreamExt as _;
```

- [ ] **Step 2: Add `ChatBuilder::stream` + `ChatStream`**

Add `stream` to `impl ChatBuilder` (after `send`):

```rust
    /// Send the request and stream the response. Yields [`StreamEvent::Delta`]
    /// text chunks then the terminal [`StreamEvent::Usage`] cost event.
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
        let mut req = self
            .client
            .http
            .post(format!("{}/v1/chat/completions", self.client.base))
            .bearer_auth(&self.client.key)
            .json(&body);
        if let Some(tag) = &self.tag {
            req = req.header("X-TokenTrimmer-Tag", tag);
        }
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
```

Add the `ChatStream` type (after `ChatOutcome`):

```rust
/// A live chat stream. Iterate with [`ChatStream::next`].
pub struct ChatStream {
    inner: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
    pending: std::collections::VecDeque<StreamEvent>,
    done: bool,
    header_cost: CostInfo,
}

impl ChatStream {
    /// Header-based cost/trace (`model_used`, `provider`, `trace_id`) — known
    /// before the body streams.
    #[must_use]
    pub fn header_cost(&self) -> &CostInfo {
        &self.header_cost
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
                    for frame in drain_frames(&mut self.buf) {
                        match parse_sse_frame(&frame) {
                            Frame::Delta(t) => self.pending.push_back(StreamEvent::Delta(t)),
                            Frame::Usage(u) => self.pending.push_back(StreamEvent::Usage(u)),
                            Frame::Done => self.done = true,
                            Frame::Ignore => {}
                        }
                    }
                }
                Some(Err(e)) => return Err(Error::Request(e)),
                None => self.done = true,
            }
        }
    }
}
```

- [ ] **Step 3: Add the httpmock SSE integration tests**

Add to `lib.rs` `mod tests` (after the existing `send_*` tests):

```rust
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
        assert_eq!(stream.header_cost().model_used.as_deref(), Some("gpt-4o-mini"));

        let mut text = String::new();
        let mut usage: Option<StreamUsage> = None;
        while let Some(ev) = stream.next().await.unwrap() {
            match ev {
                StreamEvent::Delta(t) => text.push_str(&t),
                StreamEvent::Usage(u) => usage = Some(u),
            }
        }
        assert_eq!(text, "Hello");
        let u = usage.expect("usage event");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 2);
    }

    #[tokio::test]
    async fn stream_surfaces_status_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(500).body("boom");
        });
        let client = Client::new(server.base_url(), "k");
        let err = client
            .chat()
            .model("m")
            .message(user("hi"))
            .stream()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Status { status: 500, .. }), "{err:?}");
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tt-client 2>&1 | grep -E "test result|FAILED|error\[" | head`
Expected: PASS (`stream_yields_deltas_then_usage`, `stream_surfaces_status_error`, + the full suite).

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(tt-client): ChatBuilder::stream + ChatStream (SSE deltas + usage)"
```

---

### Task 4: Gates + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy (workspace)**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings. Commit any fmt diff.

- [ ] **Step 2: Tests + workspace build + deny + doc**

Run: `cargo test -p tt-client 2>&1 | grep -E "test result" | tail` ; `cargo build --workspace 2>&1 | grep -E "^error" | head` ; `cargo deny check advisories 2>&1 | tail -3` ; `cargo doc -p tt-client --no-deps 2>&1 | grep -E "^error|^warning:" | head`
Expected: tests pass; workspace builds; `advisories ok`; docs build.

- [ ] **Step 3: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** `build_body` stream flag (T1), `StreamUsage`/`StreamEvent`/`parse_sse_frame`/`drain_frames` (T2), `stream()` + `ChatStream` + integration (T3), gates (T4). All spec items covered.
- **Placeholders:** none — full code throughout.
- **Type consistency:** `build_body(…, stream: bool)`, `parse_sse_frame(&str)->Frame`, `drain_frames(&mut Vec<u8>)->Vec<String>`, `ChatBuilder::stream(self)->Result<ChatStream>`, `ChatStream::{header_cost->&CostInfo, next->Result<Option<StreamEvent>>}`, `StreamEvent::{Delta,Usage}`, `StreamUsage` fields match the gateway event. Reuses `Error`/`CostInfo`/`parse_cost`.
- **Ported faithfully:** `parse_sse_frame`/`drain_frames` are the proven V5a logic (byte-buffered, UTF-8-safe); the `café`-split regression test comes along. No new external deps (`futures`/`bytes` are workspace deps).
