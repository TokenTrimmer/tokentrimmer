# tt-client streaming tool calls (F5b) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** F5b (SDK half of F5). Surfaces the complete streaming tool calls that F5a (#46/#47) made the gateway emit.
**Depends on:** F5a merged — the gateway emits one chunk whose `delta.tool_calls` carries complete `ToolCall`(s) at finish.

## Problem

`tt-client`'s `ChatStream` drops tool calls. `parse_sse_frame` (`crates/client/src/lib.rs`) inspects only `choices[0].delta.content` → `Frame::Delta`; a chunk carrying `delta.tool_calls` falls through to `Frame::Ignore`. So a streaming caller using tools never sees them.

## Goal

Surface streaming tool calls as a first-class `StreamEvent`. After F5a the calls arrive complete in one chunk, so the SDK just needs to recognize and emit them — no reassembly.

## Architecture

All changes in `crates/client/src/lib.rs`.

### 1. `StreamEvent::ToolCalls`
The enum is already `#[non_exhaustive]`, so adding a variant is non-breaking:
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
`ToolCall` is already re-exported from the crate root.

### 2. `Frame::ToolCalls` (internal)
```rust
enum Frame {
    Delta(String),
    ToolCalls(Vec<ToolCall>),
    Usage(StreamUsage),
    Done,
    Ignore,
}
```

### 3. `parse_sse_frame`
After deserializing the chunk JSON into `v: Value` (the existing step), check tool calls **before** the content check:
```rust
if let Some(tcs) = v["choices"][0]["delta"]["tool_calls"].as_array() {
    if !tcs.is_empty() {
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
```
Tool calls take priority over content within a single frame; F5a separates content and tool calls into distinct frames, so prioritizing here drops nothing. A malformed `tool_calls` array (deserialize fails) falls through to the content/ignore path rather than erroring the stream.

### 4. `ChatStream::drain_into_pending`
Map the new frame to the new event:
```rust
Frame::ToolCalls(t) => self.pending.push_back(StreamEvent::ToolCalls(t)),
```

No SDK-side accumulation: F5a guarantees the calls arrive complete in one chunk.

## Testing (`crates/client/src/lib.rs`)

- **Unit — extend `parse_sse_frames`:** a frame
  `data: {"choices":[{"delta":{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"SF\"}"}}]}}]}`
  → `Frame::ToolCalls(calls)` with `calls[0].id == "call_1"`, `function.name == "get_weather"`, `function.arguments == "{\"city\":\"SF\"}"`. A content frame still parses to `Frame::Delta`.
- **E2E — `stream_yields_tool_calls`** (httpmock, mirroring `stream_yields_deltas_then_usage`): an SSE body with a content delta, then an F5a-shaped tool-calls chunk (`delta.tool_calls:[{id,type,function{name,arguments}}]`, `finish_reason:"tool_calls"`), then a `tokentrimmer.usage` event, then `[DONE]`. Drive the stream and assert it yields `StreamEvent::Delta("…")`, then `StreamEvent::ToolCalls(vec)` (one call, correct name/args), then `StreamEvent::Usage(_)`.

Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p tt-client`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-client --no-deps`.

## Out of scope
- `run_tools` over a stream (the agentic loop stays non-streaming).
- Per-fragment incremental tool-argument events (the gateway reassembles; not surfaced).
- Changing `header_cost`/`Usage` handling.
