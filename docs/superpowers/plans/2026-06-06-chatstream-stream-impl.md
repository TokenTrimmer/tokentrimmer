# `impl futures::Stream for ChatStream` (G-A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement `futures::Stream` and `futures::stream::FusedStream` for `tt_client::ChatStream` so callers can use `StreamExt` combinators, with no behavior change to the inherent `next()`.

**Architecture:** Add two trait impls in `crates/client/src/lib.rs` after the inherent `impl ChatStream` block. `poll_next` mirrors the existing `next()` loop in `Poll` terms (`ChatStream: Unpin`, so `self.get_mut()`); `is_terminated` reports `done && pending.is_empty()`. Keep `next()` unchanged. Tests use the existing httpmock SSE harness in the in-file `mod tests`.

**Tech Stack:** Rust, `futures` (already a dep), `httpmock` + `tokio::test` (existing test harness).

---

### Task 1: `impl futures::Stream for ChatStream` (driven by a combinator test)

**Files:**
- Modify: `crates/client/src/lib.rs` (add impl after the inherent `impl ChatStream { … }` block ending at `:554`; add test in `mod tests`)

- [ ] **Step 1: Write the failing combinator test**

In the `mod tests` block, add a test that drives the stream through a `StreamExt` combinator (not the inherent `next()`). Add `use futures::StreamExt;` at the top of the test that needs it (the module-level `use futures::StreamExt as _;` is anonymous and not callable by name from the test module):

```rust
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
```

- [ ] **Step 2: Run it — expect a COMPILE failure**

Run: `cargo test -p tt-client stream_impl_collects_deltas_via_combinators 2>&1 | tail -20`
Expected: FAIL — compile error, no method named `filter_map`/`collect` found for `ChatStream` (no `Stream` impl yet).

- [ ] **Step 3: Add the `Stream` impl**

In `crates/client/src/lib.rs`, immediately after the inherent `impl ChatStream { … }` block (closing brace at `:554`), add:

```rust
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
```

Note: `this.inner.as_mut().poll_next(cx)` requires `futures::Stream` in scope for the `poll_next` method — the module-level `use futures::StreamExt as _;` does NOT provide `Stream::poll_next` (that's the base trait, not the ext trait). Add `use futures::Stream as _;` to the module-level imports near `:5` (anonymous import keeps it call-only):

```rust
use futures::Stream as _;
```

- [ ] **Step 4: Run the test — expect PASS**

Run: `cargo test -p tt-client stream_impl_collects_deltas_via_combinators 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(client): impl futures::Stream for ChatStream"
```

---

### Task 2: `FusedStream` impl + EOF-flush test + doc note

**Files:**
- Modify: `crates/client/src/lib.rs` (add `FusedStream` impl after the `Stream` impl; add two tests; tweak the `ChatStream` doc at `:479`)

- [ ] **Step 1: Write the failing `is_terminated` test**

In `mod tests`, add:

```rust
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
        let mut stream = client.chat().model("m").message(user("hi")).stream().await.unwrap();
        assert!(!stream.is_terminated(), "fresh stream is not terminated");
        // Drain via the inherent next() until end.
        while stream.next().await.unwrap().is_some() {}
        assert!(stream.is_terminated(), "stream is terminated after None");
    }
```

- [ ] **Step 2: Run it — expect a COMPILE failure**

Run: `cargo test -p tt-client stream_is_terminated_after_exhaustion 2>&1 | tail -20`
Expected: FAIL — no method `is_terminated` for `ChatStream` (no `FusedStream` impl yet).

- [ ] **Step 3: Add the `FusedStream` impl**

Immediately after the `impl futures::Stream for ChatStream { … }` block, add:

```rust
impl futures::stream::FusedStream for ChatStream {
    fn is_terminated(&self) -> bool {
        // Once `done` is set and the queue drains, poll_next only returns None.
        self.done && self.pending.is_empty()
    }
}
```

- [ ] **Step 4: Run it — expect PASS**

Run: `cargo test -p tt-client stream_is_terminated_after_exhaustion 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Add the EOF-flush combinator test**

This proves the `Poll::Ready(None)` residual-flush arm: the SSE body ends WITHOUT a trailing `\n\n` and WITHOUT `[DONE]`, so the terminal delta is only recoverable via the flush.

```rust
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
        let stream = client.chat().model("m").message(user("hi")).stream().await.unwrap();
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
```

- [ ] **Step 6: Run it — expect PASS**

Run: `cargo test -p tt-client stream_impl_flushes_terminal_frame_on_eof 2>&1 | tail -15`
Expected: PASS (the `Poll::Ready(None)` arm appends `b"\n\n"` and drains the residual frame).

- [ ] **Step 7: Doc note on `ChatStream`**

Update the `ChatStream` doc comment (`:479`) from:
```rust
/// A live chat stream. Iterate with [`ChatStream::next`].
```
to:
```rust
/// A live chat stream. Iterate with [`ChatStream::next`], or use
/// `futures::StreamExt` combinators via the [`futures::Stream`] impl
/// (`Item = Result<StreamEvent>`); it is also a [`futures::stream::FusedStream`].
```

- [ ] **Step 8: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(client): impl FusedStream for ChatStream + EOF-flush test + doc"
```

---

### Task 3: Gates + finish

**Files:** none (verification only)

- [ ] **Step 1: Full crate tests**

Run: `cargo test -p tt-client 2>&1 | grep -E "test result:|FAILED" | tail`
Expected: all pass (the 3 new tests + the existing suite, incl. `stream_sends_cost_limit_header` which still uses the inherent `next()` — the inherent method shadows `StreamExt::next`, so its behavior is unchanged).

- [ ] **Step 2: Clippy (the `-D warnings` gate)**

Run: `cargo clippy -p tt-client --all-targets -- -D warnings 2>&1 | tail -15`
Expected: no warnings.

- [ ] **Step 3: Doc gate (intra-doc links)**

Run: `cargo doc -p tt-client --no-deps 2>&1 | tail -15`
Expected: builds with no broken-link warnings (the new doc note references `futures::Stream`/`futures::stream::FusedStream`/`ChatStream::next`, all resolvable).

- [ ] **Step 4: Confirm no stray changes**

Run: `git status --porcelain`
Expected: empty.

Run: `git diff main --stat`
Expected: only `crates/client/src/lib.rs` (+ the spec/plan docs) changed.

**Coverage note (no silent cap):** `poll_next`'s `Poll::Ready(Some(Err(e)))` arm (a mid-stream transport error) has no dedicated test — httpmock cannot inject a transport failure after a 200 + partial body, and `reqwest::Error` has no public constructor to fabricate one. That arm is byte-for-byte identical to the inherent `next()`'s `Some(Err(e))` arm; it is covered structurally, not by a new test.
