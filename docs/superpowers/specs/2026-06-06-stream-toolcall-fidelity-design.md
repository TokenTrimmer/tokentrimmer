# Streaming tool-call fidelity (compat adapter) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** F5a (gateway-side half of F5). Makes the OpenAI-compatible adapter faithfully convey streaming tool calls. F5b (tt-client SDK surfacing) builds on this.

## Problem

The OpenAI-compatible streaming parser (`crates/providers/compat/src/stream.rs`) deserializes every SSE `data:` line **directly** into the canonical `ChatCompletionChunk` (`parse_sse_event`, line ~253). The canonical `ChunkDelta.tool_calls` is `Vec<ToolCall>` where every field is required (`id`, `type`, `function.name`, `function.arguments`) and there is **no `index`**.

Real OpenAI streams tool calls as *fragments*: the first delta for a call carries `index`+`id`+`type`+`name`+`arguments:""`; **continuation deltas omit `id`/`type`/`name`** and carry only `index` + an `arguments` fragment. Those continuation chunks therefore **fail to deserialize** and are emitted as `Err(ProviderError::Deserialize)` — the argument fragments are lost, so the gateway cannot stream a usable tool call to clients.

This affects all five OpenAI-compatible providers that share `compat::stream` (openai, groq, mistral, together, openrouter). The Anthropic and Gemini adapters are unaffected — they already reassemble tool calls internally (`anthropic/src/stream.rs` `PartialToolCall`; `gemini/src/stream.rs`) and emit complete `ToolCall`s.

The existing test only passes because its fixture sends full `id`/`name` on every chunk (`openai/tests/streaming.rs:398`) — not real provider behavior.

## Goal

Bring the compat adapter in line with Anthropic/Gemini: **accumulate OpenAI tool-call fragments by `index` and emit a single chunk carrying the complete `ToolCall`(s)** when the tool-call stream finishes. Content/text deltas keep streaming incrementally as today.

This keeps the canonical `ChunkDelta.tool_calls: Vec<ToolCall>` contract unchanged — **blast radius is `compat/src/stream.rs` only** (plus the now-realistic test + snapshot). No tt-shared, sse.rs, anthropic, or gemini changes.

## Architecture

All changes in `crates/providers/compat/src/stream.rs`.

### 1. Lenient raw deserialization types (private to the module)

The direct-to-`ChatCompletionChunk` parse is what rejects fragments. Deserialize into a lenient shape instead, then convert:

```rust
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

#[derive(Debug, Deserialize)]
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
```

`RawChunk::into_canonical(self) -> ChatCompletionChunk` maps role/content/finish_reason/usage and leaves `tool_calls` **empty** (tool calls flow exclusively through the accumulator path, never through this conversion). Multiple `choices` are preserved (mapped element-wise) for non-tool chunks; tool accumulation assumes the OpenAI single-choice (`index 0`) streaming convention.

### 2. `parse_sse_event` returns `RawChunk`

`SseEvent::Chunk(RawChunk)` instead of `ChatCompletionChunk`. `Done`/`Err`/`Skip` unchanged. The existing `parse_sse_event_*` unit tests still hold (they assert `c.id == "c1"`, and `RawChunk` keeps `id`).

### 3. Accumulator in the `build_sse_stream` loop

The `async_stream` loop owns the cross-chunk state:

```rust
let mut acc: std::collections::BTreeMap<u32, PartialToolCall> = BTreeMap::new();
```
```rust
#[derive(Default)]
struct PartialToolCall { id: String, r#type: String, name: String, arguments: String }
```

Per `SseEvent::Chunk(raw)` (let `choice = raw.choices.first()`):

1. **Merge fragments** — for each `tc` in `choice.delta.tool_calls`: `let e = acc.entry(tc.index).or_default();` set `e.id`/`e.type`/`e.name` from `tc` when present and non-empty; append `tc.function.arguments` to `e.arguments`.
2. **Decide output:**
   - **Drain & emit** — if the choice has a `finish_reason` **and** `acc` is non-empty: build `Vec<ToolCall>` from `acc` (BTreeMap iteration is index-sorted) with `r#type` defaulting to `"function"` when empty, clear `acc`, and `yield Ok` a `ChatCompletionChunk` whose single `ChunkChoice` has `delta.tool_calls = <drained>`, the original `finish_reason`, and `raw.usage`.
   - **Swallow** — else if `choice.delta.tool_calls` is non-empty (mid-accumulation, no finish_reason yet): yield nothing.
   - **Forward** — else: `yield Ok(raw.into_canonical())` (content/role/usage chunks stream as today).
3. **No choices** (e.g. usage-only terminal chunk): `yield Ok(raw.into_canonical())`.

### 4. Flush on stream end

If the stream reaches `[DONE]` **or** upstream EOF with a non-empty `acc` (a tool call that never got an explicit `finish_reason`), drain `acc` into one final `ChatCompletionChunk` (`finish_reason: Some("tool_calls")`, `usage: None`) and yield it before closing. Defensive — guarantees no accumulated tool call is dropped.

## Behavior change (intended)

- A tool-call stream now yields **one** chunk with the complete call(s) at the end, instead of N fragment chunks (the fragments were broken anyway). Text/content still streams incrementally chunk-by-chunk.
- Identical to how Anthropic and Gemini already behave. Incremental tool-argument "typing" is not surfaced (it isn't for the other two providers either) — out of scope.

## Testing

`crates/providers/openai/tests/streaming.rs` (end-to-end via httpmock, real compat path):
- **Rewrite `stream_tool_call_delta`** to feed *real* OpenAI fragments:
  - chunk 1: `tool_calls:[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]`, `finish_reason:null`
  - chunk 2: `tool_calls:[{"index":0,"function":{"arguments":"{\"city\":"}}]`, `finish_reason:null` (no id/type/name)
  - chunk 3: `tool_calls:[{"index":0,"function":{"arguments":"\"NYC\"}"}}]`, `finish_reason:"tool_calls"`
  - Assert: exactly **one** chunk carrying tool calls is emitted; `tool_calls.len()==1`; `id=="call_abc"`; `function.name=="get_weather"`; `function.arguments=="{\"city\":\"NYC\"}"`; `finish_reason==Some("tool_calls")`.
- **New `stream_two_tool_calls_by_index`**: fragments with `index:0` and `index:1` interleaved across chunks → assert two complete calls, index-ordered, each with its full arguments.
- **New `stream_content_then_tool_call`**: a content chunk (`delta.content:"Let me check"`) followed by tool-call fragments → assert the content chunk forwards (streams) AND a final reassembled tool-call chunk is emitted.
- Replace the `stream_tool_call_chunks` insta snapshot with the reassembled shape (delete the stale `.snap`; `cargo insta` regenerates / accept).
- Keep happy-path, heartbeat, error, CRLF, no-space, `[DONE]` tests green.

`crates/providers/compat/src/stream.rs` unit tests:
- Update `parse_sse_event_*` assertions if needed for `RawChunk` (only the matched type changes; `.id` access stays).
- Add `parse_sse_event_tool_call_fragment_no_id` — a continuation fragment (only `index`+`arguments`) deserializes to a `RawChunk` (previously an `Err`).

Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p tt-provider-compat -p tt-provider-openai`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-provider-compat --no-deps`.

## Out of scope

- Changing the canonical `ChunkDelta`/`ToolCall` types or surfacing per-fragment incremental tool arguments (Approach B, rejected).
- The tt-client SDK side (F5b) — surfacing the now-complete tool calls as stream events.
- Anthropic/Gemini adapters (already correct).
- Multi-choice (`n>1`) tool-call streaming — OpenAI streams a single choice; non-tool multi-choice chunks pass through unchanged.
