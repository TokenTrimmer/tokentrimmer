# Streaming tool-call fidelity (compat adapter) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the OpenAI-compatible streaming parser reassemble tool-call fragments by `index` and emit one complete `ToolCall` chunk at finish, fixing streaming tool calls for all five compat providers.

**Architecture:** Replace the direct-to-`ChatCompletionChunk` SSE deserialization in `crates/providers/compat/src/stream.rs` with lenient raw types, then accumulate tool-call fragments across chunks (`ToolAccum`) inside the existing `build_sse_stream` loop and drain them into a complete chunk on `finish_reason`/stream-end. Mirrors the Anthropic/Gemini adapters; canonical chunk contract unchanged.

**Tech Stack:** Rust, async-stream, serde, httpmock + insta (tests).

---

### Task 1: Rewrite the acceptance test for real fragments (RED)

**Files:**
- Modify: `crates/providers/openai/tests/streaming.rs:387-445` (the `stream_tool_call_delta` test)
- Delete: `crates/providers/openai/tests/snapshots/streaming__stream_tool_call_chunks.snap`

- [ ] **Step 1: Replace the test body**

Replace the entire `stream_tool_call_delta` test (lines 390-445) with a version that feeds **real** OpenAI fragments (continuations omit `id`/`type`/`name`) and asserts a single reassembled chunk. Note: the snapshot assertion is dropped in favor of explicit field assertions (clearer, no accept step):

```rust
#[tokio::test]
async fn stream_tool_call_delta() {
    let server = MockServer::start();

    // Real OpenAI shape: first frag carries id/type/name + empty args; the
    // continuations carry ONLY index + an `arguments` fragment (no id/name).
    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,",
        "\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":",
        "[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,",
        "\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":",
        "[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,",
        "\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":",
        "[{\"index\":0,\"function\":{\"arguments\":\"\\\"NYC\\\"}\"}}]},",
        "\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    // Fragments are accumulated and emitted as ONE complete tool-call chunk.
    let tool_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| {
            c.choices
                .first()
                .is_some_and(|ch| !ch.delta.tool_calls.is_empty())
        })
        .collect();
    assert_eq!(tool_chunks.len(), 1, "expected one reassembled tool-call chunk");

    let tc = &tool_chunks[0].choices[0].delta.tool_calls;
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0].id, "call_abc");
    assert_eq!(tc[0].r#type, "function");
    assert_eq!(tc[0].function.name, "get_weather");
    assert_eq!(tc[0].function.arguments, "{\"city\":\"NYC\"}");
    assert_eq!(
        tool_chunks[0].choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
}
```

- [ ] **Step 2: Delete the stale snapshot**

```bash
rm crates/providers/openai/tests/snapshots/streaming__stream_tool_call_chunks.snap
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p tt-provider-openai --test streaming stream_tool_call_delta 2>&1 | tail -25`
Expected: FAIL — the current parser yields `Err(ProviderError::Deserialize)` for the continuation fragments (missing `id`/`type`/`name`), so `result.expect("no stream error")` panics (or the tool-call count is wrong).

- [ ] **Step 4: Commit the failing test**

```bash
git add crates/providers/openai/tests/streaming.rs crates/providers/openai/tests/snapshots/
git commit -m "test(compat): expect reassembled streaming tool calls (RED)"
```

---

### Task 2: Implement fragment reassembly in compat (GREEN)

**Files:**
- Modify: `crates/providers/compat/src/stream.rs` (imports ~30-32; `build_sse_stream` ~136-199; `parse_sse_event` ~218-268; add new types/helpers; add unit tests in the `#[cfg(test)]` module)

- [ ] **Step 1: Extend imports**

Replace the `use tt_shared::{...}` block (lines ~30-32) with:

```rust
use tt_shared::{
    filter_extra_headers,
    messages::{ChunkChoice, ChunkDelta, ToolCall, ToolCallFunction},
    ChatCompletionChunk, ChatCompletionRequest, ProviderError, RequestContext, Usage,
};
```

Add near the top of the file (after the existing `use` lines):

```rust
use std::collections::BTreeMap;
```

- [ ] **Step 2: Add lenient raw types + conversions**

Add these above `parse_sse_event` (e.g. just before the `SseEvent` enum):

```rust
// ── Lenient raw chunk shapes ────────────────────────────────────────────────
// OpenAI streams tool-call deltas as fragments: the first carries id/type/name,
// continuations carry only `index` + an `arguments` fragment. The canonical
// `ToolCall` requires all fields, so we deserialize into these lenient shapes
// and reassemble (see `ToolAccum`).

#[derive(Debug, Deserialize)]
struct RawChunk {
    id: String,
    object: String,
    created: i64,
    model: String,
    #[serde(default)]
    choices: Vec<RawChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct RawChoice {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: RawDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCallDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct RawToolCallDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    r#type: Option<String>,
    #[serde(default)]
    function: Option<RawFnDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFnDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl RawChunk {
    /// Map a non-tool-call chunk (content/role/usage) to the canonical shape,
    /// leaving `tool_calls` empty — tool calls flow through `ToolAccum`.
    fn into_canonical(self) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.id,
            object: self.object,
            created: self.created,
            model: self.model,
            choices: self
                .choices
                .into_iter()
                .map(|c| ChunkChoice {
                    index: c.index,
                    delta: ChunkDelta {
                        role: c.delta.role,
                        content: c.delta.content,
                        tool_calls: Vec::new(),
                    },
                    finish_reason: c.finish_reason,
                })
                .collect(),
            usage: self.usage,
        }
    }
}

// ── Tool-call accumulator ───────────────────────────────────────────────────

#[derive(Default)]
struct PartialToolCall {
    id: String,
    r#type: String,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    fn into_tool_call(self) -> ToolCall {
        ToolCall {
            id: self.id,
            r#type: if self.r#type.is_empty() {
                "function".to_string()
            } else {
                self.r#type
            },
            function: ToolCallFunction {
                name: self.name,
                arguments: self.arguments,
            },
        }
    }
}

#[derive(Clone)]
struct ChunkMeta {
    id: String,
    object: String,
    created: i64,
    model: String,
}

/// Accumulates streaming tool-call fragments (keyed by OpenAI `index`) until the
/// call is complete, then drains them into one canonical chunk.
#[derive(Default)]
struct ToolAccum {
    calls: BTreeMap<u32, PartialToolCall>,
    meta: Option<ChunkMeta>,
}

impl ToolAccum {
    fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Merge every tool-call fragment in `raw` into the accumulator.
    fn merge(&mut self, raw: &RawChunk) {
        let mut saw_fragment = false;
        for choice in &raw.choices {
            for tc in &choice.delta.tool_calls {
                saw_fragment = true;
                let e = self.calls.entry(tc.index).or_default();
                if let Some(id) = &tc.id {
                    if !id.is_empty() {
                        e.id = id.clone();
                    }
                }
                if let Some(t) = &tc.r#type {
                    if !t.is_empty() {
                        e.r#type = t.clone();
                    }
                }
                if let Some(f) = &tc.function {
                    if let Some(n) = &f.name {
                        if !n.is_empty() {
                            e.name = n.clone();
                        }
                    }
                    if let Some(a) = &f.arguments {
                        e.arguments.push_str(a);
                    }
                }
            }
        }
        if saw_fragment {
            self.meta = Some(ChunkMeta {
                id: raw.id.clone(),
                object: raw.object.clone(),
                created: raw.created,
                model: raw.model.clone(),
            });
        }
    }

    /// Drain the accumulated calls into one chunk (index-ordered via BTreeMap).
    /// Returns `None` when nothing is accumulated.
    fn drain(&mut self, finish_reason: Option<String>, usage: Option<Usage>) -> Option<ChatCompletionChunk> {
        if self.calls.is_empty() {
            return None;
        }
        let meta = self.meta.take()?;
        let tool_calls: Vec<ToolCall> = std::mem::take(&mut self.calls)
            .into_values()
            .map(PartialToolCall::into_tool_call)
            .collect();
        Some(ChatCompletionChunk {
            id: meta.id,
            object: meta.object,
            created: meta.created,
            model: meta.model,
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls,
                },
                finish_reason,
            }],
            usage,
        })
    }
}

/// Process one raw chunk against the accumulator, returning a canonical chunk to
/// forward (or `None` when the chunk is a mid-accumulation tool fragment that is
/// swallowed until the call completes).
fn handle_raw_chunk(acc: &mut ToolAccum, raw: RawChunk) -> Option<ChatCompletionChunk> {
    let has_tool_frag = raw
        .choices
        .iter()
        .any(|c| !c.delta.tool_calls.is_empty());
    let finish_reason = raw.choices.iter().find_map(|c| c.finish_reason.clone());

    if has_tool_frag {
        acc.merge(&raw);
        if finish_reason.is_some() {
            return acc.drain(finish_reason, raw.usage);
        }
        return None; // swallow until complete
    }
    // A finish_reason may arrive on a separate chunk after the fragments.
    if finish_reason.is_some() && !acc.is_empty() {
        return acc.drain(finish_reason, raw.usage);
    }
    Some(raw.into_canonical())
}
```

- [ ] **Step 3: Switch `parse_sse_event` to `RawChunk`**

In the `SseEvent` enum, change the `Chunk` variant:

```rust
enum SseEvent {
    /// A successfully deserialized (lenient) chunk.
    Chunk(RawChunk),
    Done,
    Err(ProviderError),
    Skip,
}
```

In `parse_sse_event`, change the deserialization target:

```rust
            match serde_json::from_str::<RawChunk>(data) {
                Ok(chunk) => results.push(SseEvent::Chunk(chunk)),
                Err(e) => results.push(SseEvent::Err(ProviderError::Deserialize(format!(
                    "failed to parse SSE chunk: {e}"
                )))),
            }
```

- [ ] **Step 4: Rewrite the `build_sse_stream` loop to thread the accumulator**

Replace the `async_stream::stream! { ... }` body (lines ~142-198) with:

```rust
    async_stream::stream! {
        let mut buffer: Vec<u8> = Vec::new();
        let mut acc = ToolAccum::default();
        futures::pin_mut!(bytes_stream);

        loop {
            let next_item: Option<Result<Bytes, reqwest::Error>> = bytes_stream.next().await;
            match next_item {
                Some(Ok(chunk)) => {
                    buffer.extend_from_slice(&chunk);
                    while let Some((event_end, sep_len)) = find_event_boundary(&buffer) {
                        let event_bytes = buffer.drain(..event_end + sep_len).collect::<Vec<_>>();
                        let mut done = false;
                        for event in parse_sse_event(&event_bytes) {
                            match event {
                                SseEvent::Done => {
                                    // Flush any tool call that never got an explicit finish.
                                    if let Some(c) = acc.drain(Some("tool_calls".to_string()), None) {
                                        yield Ok(c);
                                    }
                                    done = true;
                                    break;
                                }
                                SseEvent::Chunk(raw) => {
                                    if let Some(c) = handle_raw_chunk(&mut acc, raw) {
                                        yield Ok(c);
                                    }
                                }
                                SseEvent::Err(e) => {
                                    yield Err(e);
                                }
                                SseEvent::Skip => {}
                            }
                        }
                        if done {
                            return;
                        }
                    }
                }
                Some(Err(e)) => {
                    yield Err(map_reqwest_error(e));
                    return;
                }
                None => {
                    // Upstream closed without [DONE] — flush remaining buffer then acc.
                    if !buffer.is_empty() {
                        for event in parse_sse_event(&buffer) {
                            match event {
                                SseEvent::Chunk(raw) => {
                                    if let Some(c) = handle_raw_chunk(&mut acc, raw) {
                                        yield Ok(c);
                                    }
                                }
                                SseEvent::Err(e) => yield Err(e),
                                SseEvent::Done | SseEvent::Skip => {}
                            }
                        }
                    }
                    if let Some(c) = acc.drain(Some("tool_calls".to_string()), None) {
                        yield Ok(c);
                    }
                    return;
                }
            }
        }
    }
```

- [ ] **Step 5: Update + add compat unit tests**

The existing `parse_sse_event_*` tests still compile (they match `SseEvent::Chunk(c) if c.id == "..."` — `RawChunk` keeps `id`). Add these tests to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn parse_sse_event_tool_call_fragment_no_id() {
        // A continuation fragment (only index + arguments) must now deserialize
        // (previously failed because canonical ToolCall required id/type/name).
        let data = r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]},"finish_reason":null}]}"#;
        let event = format!("data: {data}\n\n");
        let results = parse_sse_event(event.as_bytes());
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], SseEvent::Chunk(c) if c.id == "c"));
    }

    fn frag(index: u32, id: Option<&str>, name: Option<&str>, args: &str) -> RawToolCallDelta {
        RawToolCallDelta {
            index,
            id: id.map(String::from),
            r#type: id.map(|_| "function".to_string()),
            function: Some(RawFnDelta {
                name: name.map(String::from),
                arguments: Some(args.to_string()),
            }),
        }
    }

    fn raw_chunk(tool_calls: Vec<RawToolCallDelta>, finish_reason: Option<&str>) -> RawChunk {
        RawChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            created: 1,
            model: "gpt-4o".into(),
            choices: vec![RawChoice {
                index: 0,
                delta: RawDelta {
                    role: None,
                    content: None,
                    tool_calls,
                },
                finish_reason: finish_reason.map(String::from),
            }],
            usage: None,
        }
    }

    #[test]
    fn handle_reassembles_single_tool_call() {
        let mut acc = ToolAccum::default();
        // frag 1: id+name, empty args → swallowed
        assert!(handle_raw_chunk(&mut acc, raw_chunk(vec![frag(0, Some("call_1"), Some("f"), "")], None)).is_none());
        // frag 2: args fragment, no id → swallowed
        assert!(handle_raw_chunk(&mut acc, raw_chunk(vec![frag(0, None, None, "{\"a\":")], None)).is_none());
        // frag 3: closing args + finish → drained
        let out = handle_raw_chunk(&mut acc, raw_chunk(vec![frag(0, None, None, "1}")], Some("tool_calls")))
            .expect("complete chunk");
        let tc = &out.choices[0].delta.tool_calls;
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_1");
        assert_eq!(tc[0].r#type, "function");
        assert_eq!(tc[0].function.name, "f");
        assert_eq!(tc[0].function.arguments, "{\"a\":1}");
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        assert!(acc.is_empty());
    }

    #[test]
    fn handle_reassembles_two_tool_calls_by_index() {
        let mut acc = ToolAccum::default();
        handle_raw_chunk(&mut acc, raw_chunk(vec![frag(0, Some("a"), Some("fa"), "{}")], None));
        handle_raw_chunk(&mut acc, raw_chunk(vec![frag(1, Some("b"), Some("fb"), "{}")], None));
        let out = handle_raw_chunk(&mut acc, raw_chunk(vec![], Some("tool_calls")))
            .expect("complete chunk");
        let tc = &out.choices[0].delta.tool_calls;
        assert_eq!(tc.len(), 2);
        assert_eq!(tc[0].id, "a"); // index 0 first
        assert_eq!(tc[1].id, "b");
    }

    #[test]
    fn handle_forwards_content_chunk() {
        let mut acc = ToolAccum::default();
        let raw = RawChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            created: 1,
            model: "gpt-4o".into(),
            choices: vec![RawChoice {
                index: 0,
                delta: RawDelta {
                    role: None,
                    content: Some("Hi".into()),
                    tool_calls: vec![],
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let out = handle_raw_chunk(&mut acc, raw).expect("forwarded");
        assert_eq!(out.choices[0].delta.content.as_deref(), Some("Hi"));
        assert!(out.choices[0].delta.tool_calls.is_empty());
    }
```

- [ ] **Step 6: Run compat + openai tests**

Run: `cargo test -p tt-provider-compat -p tt-provider-openai 2>&1 | tail -30`
Expected: all pass, including the rewritten `stream_tool_call_delta` and the new compat unit tests.

- [ ] **Step 7: Commit**

```bash
git add crates/providers/compat/src/stream.rs
git commit -m "feat(compat): reassemble streaming tool-call fragments (GREEN)"
```

---

### Task 3: Extra end-to-end coverage

**Files:**
- Modify: `crates/providers/openai/tests/streaming.rs` (add two tests after `stream_tool_call_delta`)

- [ ] **Step 1: Add the two tests**

```rust
#[tokio::test]
async fn stream_two_tool_calls_by_index() {
    let server = MockServer::start();
    let sse_body = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"fa\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"fb\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200).header("Content-Type", "text/event-stream").body(sse_body);
    });
    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("stream");
    let mut tool_calls = Vec::new();
    while let Some(r) = stream.next().await {
        let c = r.expect("no error");
        if let Some(ch) = c.choices.first() {
            tool_calls.extend(ch.delta.tool_calls.clone());
        }
    }
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0].id, "call_a"); // index 0 first
    assert_eq!(tool_calls[1].id, "call_b");
}

#[tokio::test]
async fn stream_content_then_tool_call() {
    let server = MockServer::start();
    let sse_body = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me check\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200).header("Content-Type", "text/event-stream").body(sse_body);
    });
    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("stream");
    let mut content = String::new();
    let mut tool_call_count = 0;
    while let Some(r) = stream.next().await {
        let c = r.expect("no error");
        if let Some(ch) = c.choices.first() {
            if let Some(t) = &ch.delta.content {
                content.push_str(t);
            }
            tool_call_count += ch.delta.tool_calls.len();
        }
    }
    assert_eq!(content, "Let me check", "content chunk still streams");
    assert_eq!(tool_call_count, 1, "tool call reassembled");
}
```

- [ ] **Step 2: Run**

Run: `cargo test -p tt-provider-openai --test streaming 2>&1 | tail -20`
Expected: all streaming tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/providers/openai/tests/streaming.rs
git commit -m "test(compat): cover multi-call + content-then-toolcall streaming"
```

---

### Task 4: Gates + finish

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt`
Then: `git diff --quiet || (git add -A && git commit -m "style: cargo fmt")`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: no warnings. Fix any, then re-run.

- [ ] **Step 3: Test the affected crates**

Run: `cargo test -p tt-provider-compat -p tt-provider-openai 2>&1 | grep -E "test result:" | tail`
Expected: all pass.

- [ ] **Step 4: Doc gate**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-provider-compat --no-deps 2>&1 | tail -10`
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
