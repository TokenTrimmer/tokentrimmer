# `impl futures::Stream for ChatStream` (G-A) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** G-A (`ttc-chatstream-stream-impl`, public repo). Add a `futures::Stream` (+ `FusedStream`) impl to `tt_client::ChatStream` so callers can use `StreamExt` combinators, not only the inherent `next()`.

## Goal

`ChatStream` (`crates/client/src/lib.rs`) today exposes only an inherent `async fn next() -> Result<Option<StreamEvent>>`. Callers therefore cannot use the `futures::StreamExt` combinator ecosystem (`map`/`filter`/`collect`/`for_each`/`try_collect`) or `select!`. This slice implements `futures::Stream` and `futures::stream::FusedStream` for `ChatStream`, reusing the existing frame-draining loop, with no behavior change to current `next()` callers.

## Background (current state)

- `ChatStream` fields (`:480-486`): `inner: Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>`, `buf: Vec<u8>`, `pending: VecDeque<StreamEvent>`, `done: bool`, `header_cost: CostInfo`. Every field is `Unpin`, so `ChatStream: Unpin`.
- `drain_into_pending()` (`:510-520`) parses every complete frame in `buf` into `pending` (`Delta`/`ToolCalls`/`Usage`/sets `done` on `Done`/ignores `Ignore`).
- `next()` (`:526-553`) loops: pop `pending`; if `done` → `Ok(None)`; else `self.inner.next().await`:
  - `Some(Ok(chunk))` → extend `buf`, `drain_into_pending()`;
  - `Some(Err(e))` → `Err(Error::Request(e))`;
  - `None` (EOF) → if `buf` non-empty, append synthetic `b"\n\n"` + drain (so a terminal frame without a trailing blank line isn't dropped), then set `done`.
- `futures` is already a dependency (`use futures::StreamExt as _;` at `:5`); `Result` is the crate's `Result<T, Error>` alias.

## Architecture

All changes in `crates/client/src/lib.rs`. No new dependencies.

### 1. `impl futures::Stream for ChatStream`
`type Item = Result<StreamEvent>`. A manual `poll_next` that mirrors `next()` in `Poll` terms (since `ChatStream: Unpin`, take `let this = self.get_mut();`):

```rust
impl futures::Stream for ChatStream {
    type Item = Result<StreamEvent>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
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
                    // Mirror next(): EOF can terminate a final event without a
                    // trailing blank line — flush the residual frame.
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
```
`this.inner.as_mut().poll_next(cx)` is valid: `inner` is `Pin<Box<dyn Stream + Send>>`, `.as_mut()` yields `Pin<&mut (dyn Stream + Send)>`, and `Stream::poll_next` takes `Pin<&mut Self>`.

### 2. `impl futures::stream::FusedStream for ChatStream`
```rust
impl futures::stream::FusedStream for ChatStream {
    fn is_terminated(&self) -> bool {
        self.done && self.pending.is_empty()
    }
}
```
Correct because once `done` is set and `pending` drains, `poll_next` only ever returns `None`.

### 3. Keep the inherent `next()`
Unchanged — back-compat and the ergonomic `Result<Option<StreamEvent>>` shape. Rust's inherent-method resolution means `stream.next()` still binds to the inherent method even when `StreamExt` is in scope (benign shadow; current behavior preserved). Add a one-line doc note on `ChatStream` pointing combinator users at the `Stream` impl.

## Data flow / semantics

Identical to `next()`: same frame parsing, same per-error single-`Err` item, same EOF residual flush. The only addition is the `Poll`-based entry point and the fused-termination signal. Combinators (`collect`, `for_each`, `try_collect`, …) operate purely through `poll_next`, so they inherit these semantics.

## Testing (httpmock SSE, mirroring the existing `stream` tests)

1. `StreamExt::collect::<Vec<Result<StreamEvent>>>()` over a deltas→usage→`[DONE]` mock yields the same event sequence the `next()` test asserts, then terminates.
2. A transport/`500` error mid-stream surfaces as exactly one `Err` item from the combinator path.
3. EOF without a trailing `\n\n` still flushes the terminal `usage`/delta frame via `poll_next` (combinator path).
4. `is_terminated()` is `false` before exhaustion and `true` after the stream has returned `None`.

Gates: `cargo test -p tt-client`; `cargo clippy -p tt-client --all-targets -- -D warnings`; `cargo doc -p tt-client` (the crate's doc gate — keep intra-doc links valid).

## Out of scope
- Changing `StreamEvent` or the inherent `next()` signature.
- A `TryStream`-specific helper (the `Result` item already enables `try_*` combinators).
- CLI adopting the `Stream` impl (`tt chat` keeps calling `next()`).
- Items B (`gw-warnings-header`) and C (`gw-traceparent-ingest`) — separate slices.
