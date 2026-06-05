# V7b — `tt-client` Streaming Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V7b (follow-up to V7 #36). Adds streaming to the `tt-client` SDK.
**Depends on:** `tt-client` (V7 #36) merged.

## Goal

`ChatBuilder::stream()` — stream content deltas live and surface the gateway's terminal `tokentrimmer.usage` event as typed cost. Ports the proven V5a SSE machinery into the SDK. Complements the existing non-streaming `send()`.

## Architecture

All in `crates/client/src/lib.rs`. New deps: `futures` + `bytes` (both workspace deps — no new external deps).

### Stream events (public)
- **`pub struct StreamUsage { cost_usd, baseline_cost_usd, saved_usd, input_tokens, output_tokens, cached_tokens }`** (`Deserialize, Debug, Clone`) — the terminal `tokentrimmer.usage` SSE event payload.
- **`#[non_exhaustive] pub enum StreamEvent { Delta(String), Usage(StreamUsage) }`** (`Debug, Clone`) — what the caller iterates. (`Done`/ignored frames are consumed internally and never yielded.)

### SSE parsing (private, ported + tested)
- **`enum Frame { Delta(String), Usage(StreamUsage), Done, Ignore }`** (private).
- **`fn parse_sse_frame(frame: &str) -> Frame`** — `event:`/`data:` lines; `tokentrimmer.usage` → `Usage`; `[DONE]` → `Done`; a `choices[0].delta.content` chunk → `Delta`; else `Ignore`. (Ported verbatim from V5a `chat::parse_sse_frame`.)
- **`fn drain_frames(buf: &mut Vec<u8>) -> Vec<String>`** — byte-buffered, decodes only complete `\n\n`-separated frames so a multi-byte char split across network chunks is never decoded mid-sequence. (Ported verbatim from V5a.)

### `build_body` gains a `stream` flag
- `build_body(model, messages, max_tokens, temperature, stream: bool) -> Value` (the `stream` field is now a param; `send()` passes `false`, `stream()` passes `true`). Public-signature change — fine on the unreleased crate; the `build_body_shape` test updates accordingly.

### `ChatBuilder::stream`
- **`pub async fn stream(self) -> Result<ChatStream>`**: same `MissingModel` pre-flight + tag header + bearer as `send`, but `stream:true`. On non-2xx → `Error::Status { …, cost: Box<CostInfo> }` (parsed from headers). On success → a `ChatStream` wrapping `resp.bytes_stream()`.

### `ChatStream`
```rust
pub struct ChatStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
    pending: std::collections::VecDeque<StreamEvent>,
    done: bool,
    header_cost: CostInfo,
}
impl ChatStream {
    /// Header-based cost/trace (model_used, provider, trace_id) — available
    /// before the body streams.
    pub fn header_cost(&self) -> &CostInfo;
    /// Next event, or `None` at end of stream.
    pub async fn next(&mut self) -> Result<Option<StreamEvent>>;
}
```
`next()` loop: drain queued events; if `done`, return `None`; else pull a chunk (`futures::StreamExt::next`), `buf.extend_from_slice`, `drain_frames`, `parse_sse_frame` each → push `Delta`/`Usage` to `pending`, set `done` on `Frame::Done` or end-of-stream, skip `Ignore`. A chunk error → `Err(Error::Request)`.

## Usage (doc example, httpmock-tested)
```rust
let mut stream = client.chat().model("gpt-4o-mini").messages(vec![tt_client::user("Hi")]).stream().await?;
let mut answer = String::new();
while let Some(ev) = stream.next().await? {
    match ev {
        tt_client::StreamEvent::Delta(t) => { print!("{t}"); answer.push_str(&t); }
        tt_client::StreamEvent::Usage(u) => println!("\n${} · {} tok", u.cost_usd, u.input_tokens + u.output_tokens),
        _ => {}
    }
}
```

## Testing
- **`parse_sse_frame`** (ported tests): a content delta → `Frame::Delta`; the `tokentrimmer.usage` event → `Frame::Usage` with the right tokens; `[DONE]` → `Done`; a role-only/`""` frame → `Ignore`.
- **`drain_frames`**: the `café`-split-across-chunks regression (no frame until complete; then the decoded `café` delta) — ported.
- **build_body**: `stream` flag reflected; `max_tokens`/`temperature` still optional.
- **Integration (httpmock)**: a mock `/v1/chat/completions` returning an SSE body (two content frames, then the `tokentrimmer.usage` event, then `[DONE]`) + cost headers → iterate `next()` and assert the concatenated deltas, the `Usage` event values, and `header_cost().model_used`; a 500 → `Error::Status`.
- `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo deny`; `cargo test -p tt-client`.

## Out of Scope
- Embeddings streaming; tool-calling deltas (`tool_calls` fragments) — later.
- A `futures::Stream` impl on `ChatStream` (the `async fn next()` is the ergonomic surface; a `Stream` impl can be added later without breaking).
- De-duplicating the SSE parsing with `tt-cli`'s `chat` module (the CLI-adopts-`tt-client` refactor is a separate follow-up).
