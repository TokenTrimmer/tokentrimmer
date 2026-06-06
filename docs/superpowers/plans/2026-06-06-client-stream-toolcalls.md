# tt-client streaming tool calls (F5b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface streaming tool calls from `tt-client`'s `ChatStream` as a `StreamEvent::ToolCalls(Vec<ToolCall>)`.

**Architecture:** Add `ToolCalls` variants to the public `StreamEvent` and internal `Frame` enums, recognize `delta.tool_calls` in `parse_sse_frame` (priority over content), and map the frame to the event in `drain_into_pending`. No reassembly — F5a delivers complete calls in one chunk. All in `crates/client/src/lib.rs`.

**Tech Stack:** Rust, serde_json, httpmock (tests).

---

### Task 1: Recognize and emit tool-call frames

**Files:**
- Modify: `crates/client/src/lib.rs` (`StreamEvent` ~157-165; `Frame` ~167-173; `parse_sse_frame` ~201-208; `drain_into_pending` ~498-507; unit test `parse_sse_frames` ~946-963)

- [ ] **Step 1: Add the enum variants + drain mapping**

In `StreamEvent` (`#[non_exhaustive]`), add the `ToolCalls` variant between `Delta` and `Usage`:

```rust
pub enum StreamEvent {
    /// A chunk of assistant text.
    Delta(String),
    /// Complete tool call(s) the model requested (emitted at finish).
    ToolCalls(Vec<ToolCall>),
    /// The terminal cost/usage event.
    Usage(StreamUsage),
}
```

In the internal `Frame` enum, add the matching variant:

```rust
enum Frame {
    Delta(String),
    ToolCalls(Vec<ToolCall>),
    Usage(StreamUsage),
    Done,
    Ignore,
}
```

In `ChatStream::drain_into_pending`, add the arm that maps the frame to the event (alongside the existing `Frame::Delta`/`Frame::Usage` arms):

```rust
                Frame::ToolCalls(t) => self.pending.push_back(StreamEvent::ToolCalls(t)),
```

(`ToolCall` is already re-exported at the crate root — no new import.)

- [ ] **Step 2: Extend the `parse_sse_frames` unit test (RED)**

In the existing `#[test] fn parse_sse_frames()` add, after the existing `Frame::Delta` assertion:

```rust
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
```

Run: `cargo test -p tt-client parse_sse_frames 2>&1 | tail -15`
Expected: FAIL — `parse_sse_frame` currently returns `Frame::Ignore` for a tool-calls frame (no content), so the `matches!` is false.

- [ ] **Step 3: Implement the tool-calls branch in `parse_sse_frame` (GREEN)**

Replace the final content `match` (lines ~205-208) with a tool-calls check first, then the content fallback:

```rust
    if let Some(tcs) = v["choices"][0]["delta"]["tool_calls"].as_array() {
        if !tcs.is_empty() {
            if let Ok(calls) =
                serde_json::from_value::<Vec<ToolCall>>(Value::Array(tcs.clone()))
            {
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
```

Run: `cargo test -p tt-client parse_sse_frames 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(tt-client): surface streaming tool calls as StreamEvent::ToolCalls"
```

---

### Task 2: End-to-end stream test

**Files:**
- Modify: `crates/client/src/lib.rs` (add a `#[tokio::test]` in the `#[cfg(test)] mod tests` block, next to `stream_yields_deltas_then_usage`)

- [ ] **Step 1: Write the e2e test**

```rust
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
```

- [ ] **Step 2: Run**

Run: `cargo test -p tt-client stream_yields_tool_calls 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "test(tt-client): e2e stream yields ToolCalls then Usage"
```

---

### Task 3: Gates + finish

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt`
Then: `git diff --quiet || (git add -A && git commit -m "style: cargo fmt")`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: no warnings. Fix any, re-run.

- [ ] **Step 3: Test the crate**

Run: `cargo test -p tt-client 2>&1 | grep -E "test result:" | tail`
Expected: all pass.

- [ ] **Step 4: Doc gate**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-client --no-deps 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 5: Advisories**

Run: `cargo deny check advisories 2>&1 | tail -5`
Expected: ok.

- [ ] **Step 6: Commit any residual gate fixes**

```bash
git status --porcelain
# commit anything outstanding with a descriptive message
```
```
