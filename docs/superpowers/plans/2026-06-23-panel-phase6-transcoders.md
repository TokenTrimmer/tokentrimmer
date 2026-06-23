# Deep Research Panel — Phase 6 (Transcoder Rendering) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Render the `tokentrimmer.panel` attribution on `/v1/messages` + `/v1/responses`, fix `/v1/responses` `tt_extras` passthrough, and forward the `tokentrimmer.*` SSE events through the `/v1/messages` streaming transcode.

**Architecture:** The panel already runs on both endpoints via the `X-TokenTrimmer-Panel` header (passed verbatim to `chat::handler`). Phase 6 is entirely in the two transcoder modules + the `/v1/responses` request translator — no change to billing, the panel engine, `chat::handler`, or `sse.rs`. Non-streaming: extract `tokentrimmer.panel` from the buffered chat body (a `serde_json::Value` parse) and graft it as a top-level key onto the transcoded body (the conversion fns take `&ChatCompletionResponse` → `Value`, so the graft target is their return value). Streaming `/v1/messages`: forward `event: tokentrimmer.*` frames verbatim (the current frame parser only inspects `data:` lines and drops them).

**Tech Stack:** Rust, `crates/core` (tt-core) + `crates/providers/anthropic`. Branch `feat/panel-phase6-transcoders` (created; spec committed). Spec: `docs/superpowers/specs/2026-06-23-panel-phase6-transcoders-design.md`.

## Global Constraints
- **Off-by-default:** a non-panel request on either endpoint produces output unchanged from today — the `tokentrimmer` key is grafted strictly when the pluck is `Some`; the SSE forward triggers only on `event: tokentrimmer.*` frames; the `tt_extras` extraction acts only when an inbound `tt_extras` key is present.
- **No billing / chat / sse / panel-engine change.** Reuse Phases 1–5 as-is. Cost on non-streaming rides the existing `x-tokentrimmer-*` headers (pass through); on streaming `/v1/messages` cost rides the forwarded `tokentrimmer.usage` SSE event (Task 4 is load-bearing for streamed cost).
- **Render shape:** `{ "tokentrimmer": { "panel": <panel_body_json> } }` as a top-level key, identical to `/v1/chat/completions` (one cross-endpoint contract).
- **CI gates (verify locally before each commit):** `cargo fmt --` on changed files only + `cargo fmt --check` clean; `cargo clippy -p tt-core --lib --tests` no new warnings; the task's tests + the existing `messages_ingress` suite green. Do NOT use `--all-targets`. Never whole-crate `cargo fmt`. The whole-crate `--lib --tests` gate stalls on this macOS box (dyld/Spotlight) — CI (Linux) is authoritative; run targeted `--test <file>` locally.

---

### Task 1: `/v1/responses` tt_extras passthrough

**Files:**
- Modify: `crates/core/src/routes/responses.rs` (`ResponsesRequest::into_chat_request`, ~130-179)
- Test: `crates/core/tests/responses_panel.rs` (new) — or extend an existing responses test if present

**Interfaces:**
- Produces: inbound `extra["tt_extras"]` (a JSON object) is moved into `ChatCompletionRequest.tt_extras` (a `HashMap<String, serde_json::Value>`) instead of the current hardcoded empty map; the key is removed from `extra` so it is not double-passed or rejected by the passthrough loop.

- [ ] **Step 1: Write the failing test**

Create `crates/core/tests/responses_panel.rs`. Build the app with mock panel providers (mirror `crates/core/tests/panel_engine.rs`'s app + mock-provider setup) and send a `POST /v1/responses` with header `X-TokenTrimmer-Panel: synthesize` and a body carrying `"tt_extras": { "panel": { "members": [<two mock models>], "arbiter": <mock arbiter> } }`. Assert `200` and that the rendered panel reflects those members (this test will be completed by Task 2's rendering; for Task 1, assert the request is accepted — NOT rejected with "unsupported /v1/responses field: tt_extras"). Minimal Task-1 assertion: a `/v1/responses` body with a top-level `tt_extras` object returns `200` (not `400 unsupported field`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --test responses_panel`
Expected: FAIL — `400` "unsupported /v1/responses field for stateless bridge: tt_extras" (the `extra` loop at responses.rs:134-141 rejects the unknown non-null key).

- [ ] **Step 3: Implement — extract tt_extras before the extra loop**

In `into_chat_request`, the fn takes `self` by value and `self.extra` is a `HashMap<String, Value>`. Before the `for (key, value) in self.extra` loop (line 134), pull out `tt_extras`:

```rust
// Special-case tt_extras (TokenTrimmer panel/lever config) BEFORE the generic
// passthrough loop, mirroring the `metadata` special-case above: move it into the
// typed tt_extras field rather than rejecting it as an unsupported field.
let mut self_extra = self.extra;
let tt_extras: std::collections::HashMap<String, serde_json::Value> =
    match self_extra.remove("tt_extras") {
        Some(serde_json::Value::Object(map)) => map.into_iter().collect(),
        Some(other) if !other.is_null() => {
            tracing::warn!("ignoring non-object tt_extras on /v1/responses");
            std::collections::HashMap::new()
        }
        _ => std::collections::HashMap::new(),
    };
```

Then change the loop to iterate `self_extra` (instead of `self.extra`), and change line 177 from `tt_extras: HashMap::new(),` to `tt_extras,`. (Adjust the `let mut extra = HashMap::new();` block ordering as needed so `self_extra` is consumed by the loop after the removal.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-core --test responses_panel`
Expected: PASS (request accepted; full panel-render assertion lands in Task 2).

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt -- crates/core/src/routes/responses.rs crates/core/tests/responses_panel.rs
cargo clippy -p tt-core --lib --tests
git add crates/core/src/routes/responses.rs crates/core/tests/responses_panel.rs
git commit -m "feat(panel): /v1/responses tt_extras passthrough (enables tt_extras.panel config)"
```

---

### Task 2: Shared panel-graft helper + `/v1/responses` non-streaming render

**Files:**
- Modify: `crates/core/src/routes/mod.rs` (add the shared helper) and `crates/core/src/routes/responses.rs` (`transcode_json_response`, 616-638)
- Test: `crates/core/tests/responses_panel.rs` (extend)

**Interfaces:**
- Produces: `pub(crate) fn graft_tokentrimmer_panel(out: &mut serde_json::Value, chat_body: &[u8])` — parses `chat_body` as `Value`, plucks `["tokentrimmer"]["panel"]`, and if present inserts `out["tokentrimmer"] = { "panel": <plucked> }` (no-op when absent or when `out` is not a JSON object).
- Consumes (Task 3): `/v1/messages` reuses the same helper.

- [ ] **Step 1: Write the failing test**

Extend `responses_panel.rs`: with the Task-1 request (panel header + `tt_extras.panel`), assert the Responses-API body has a top-level `tokentrimmer.panel` object with `legs` (array) and `arbiter.strategy == "synthesize"`, and that the `x-tokentrimmer-cost-usd` response header is present.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --test responses_panel`
Expected: FAIL — no `tokentrimmer` key in the Responses body (transcode drops it).

- [ ] **Step 3: Add the shared helper (`routes/mod.rs`)**

```rust
/// Graft the `tokentrimmer.panel` attribution from a chat-completions response body
/// onto a transcoded target-shape body. The chat handler grafts `tokentrimmer.panel`
/// as a top-level key (chat.rs); the transcoders deserialize into the typed
/// `ChatCompletionResponse` (which drops unknown top-level keys), so we re-extract it
/// from the raw bytes here and re-attach it to the target body. No-op when absent
/// (off-by-default) or when `out` is not a JSON object.
pub(crate) fn graft_tokentrimmer_panel(out: &mut serde_json::Value, chat_body: &[u8]) {
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(chat_body) else { return };
    let Some(panel) = val.get("tokentrimmer").and_then(|t| t.get("panel")).cloned() else { return };
    if let Some(obj) = out.as_object_mut() {
        obj.insert("tokentrimmer".into(), serde_json::json!({ "panel": panel }));
    }
}
```

- [ ] **Step 4: Wire it into `/v1/responses` transcode_json_response**

In `responses.rs:transcode_json_response`, after `let responses = chat_response_to_responses_json(&chat);` (628) and before serialization (629), make `responses` mutable and graft:

```rust
let mut responses = chat_response_to_responses_json(&chat);
crate::routes::graft_tokentrimmer_panel(&mut responses, &bytes);
let new_body = serde_json::to_vec(&responses)
    .map_err(|e| ApiError::Internal(format!("failed to serialize Responses body: {e}")))?;
```

(`bytes` is the buffered chat body already in scope at line 618.)

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p tt-core --test responses_panel`
Expected: PASS — Responses body has `tokentrimmer.panel`, cost header present, members reflect the `tt_extras.panel` config (proving Task 1 + Task 2 together).

- [ ] **Step 6: Off-by-default regression + fmt/clippy + commit**

Run: `cargo test -p tt-core --test responses_panel` and any existing responses ingress test. Add a no-panel `/v1/responses` test asserting NO `tokentrimmer` key (off-by-default).

```bash
cargo fmt -- crates/core/src/routes/mod.rs crates/core/src/routes/responses.rs crates/core/tests/responses_panel.rs
cargo clippy -p tt-core --lib --tests
git add -A && git commit -m "feat(panel): render tokentrimmer.panel on /v1/responses (shared graft helper)"
```

---

### Task 3: `/v1/messages` non-streaming render

**Files:**
- Modify: `crates/core/src/routes/messages.rs` (`transcode_json_response`, 119-147)
- Test: `crates/core/tests/messages_ingress.rs` (extend — it already has the `/v1/messages` app + request harness)

**Interfaces:**
- Consumes: `crate::routes::graft_tokentrimmer_panel` (Task 2).

- [ ] **Step 1: Write the failing test**

Extend `messages_ingress.rs`: build the app with mock panel providers (mirror `panel_engine.rs`), send `POST /v1/messages` with `X-TokenTrimmer-Panel: synthesize`, assert `200` and the Anthropic Messages body (`{type:"message", content:[...], ...}`) has a top-level `tokentrimmer.panel` object with `legs` + `arbiter.strategy`. Also add a no-panel `/v1/messages` test asserting NO `tokentrimmer` key (off-by-default).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --test messages_ingress`
Expected: FAIL — no `tokentrimmer` key in the Anthropic body.

- [ ] **Step 3: Wire the graft into messages transcode_json_response**

In `messages.rs:transcode_json_response`, after `let anthropic = chat_response_to_messages(&chat);` (135), make it mutable and graft before serializing (136):

```rust
let mut anthropic = chat_response_to_messages(&chat);
crate::routes::graft_tokentrimmer_panel(&mut anthropic, &bytes);
let new_body = serde_json::to_vec(&anthropic)
    .map_err(|e| ApiError::Internal(format!("failed to serialize Anthropic response: {e}")))?;
```

(`bytes` is in scope at line 121.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-core --test messages_ingress`
Expected: PASS — Anthropic body has `tokentrimmer.panel`; no-panel case has no `tokentrimmer` key.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt -- crates/core/src/routes/messages.rs crates/core/tests/messages_ingress.rs
cargo clippy -p tt-core --lib --tests
git add crates/core/src/routes/messages.rs crates/core/tests/messages_ingress.rs
git commit -m "feat(panel): render tokentrimmer.panel on /v1/messages"
```

---

### Task 4: `/v1/messages` streaming forward of `tokentrimmer.*` SSE events

**Files:**
- Modify: `crates/core/src/routes/messages.rs` (`process_openai_frame`, 258-284)
- Test: `crates/core/tests/messages_ingress.rs` (extend) — or a focused unit test on `process_openai_frame` if it's reachable

**Interfaces:**
- Produces: `process_openai_frame` forwards a frame whose `event:` line names `tokentrimmer.*` verbatim (returns it as the emitted output) instead of dropping it.

- [ ] **Step 1: Write the failing test**

Add a unit test for `process_openai_frame` (it's a module-private fn; add a `#[cfg(test)] mod` in messages.rs, or test via the streaming integration path). Unit form: feed a frame `b"event: tokentrimmer.panel\ndata: {\"strategy\":\"synthesize\",\"legs\":[]}\n\n"` and a fresh `AnthropicSseEncoder`; assert the returned `Option<String>` is `Some` and contains `tokentrimmer.panel` + the data JSON (forwarded verbatim). A second case: a normal `ChatCompletionChunk` frame still transcodes to Anthropic events (unchanged). A third: an unrelated non-data/non-tt frame still returns `None`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --test messages_ingress` (or the unit test module)
Expected: FAIL — the tokentrimmer frame returns `None` (dropped); the parser only inspects `data:` lines.

- [ ] **Step 3: Implement — detect + forward `event: tokentrimmer.*` verbatim**

At the top of `process_openai_frame`, before the data-line loop (after `let text = ...?;` at 259), add:

```rust
    // Phase 6: forward TokenTrimmer panel/usage SSE events verbatim. The Phase-5
    // streaming panel emits frames like `event: tokentrimmer.panel\ndata: {...}`;
    // the data-line loop below only understands ChatCompletionChunk/error frames and
    // would drop these. A TT-aware client on /v1/messages reads them (and on streams
    // they are the ONLY carrier of panel cost — no x-tokentrimmer-* headers on SSE).
    if text.lines().any(|l| {
        l.trim_end_matches('\r')
            .strip_prefix("event:")
            .map(|s| s.trim().starts_with("tokentrimmer."))
            .unwrap_or(false)
    }) {
        // Emit the frame unchanged, with the blank-line terminator that delimits an
        // SSE frame on the wire.
        let mut out = text.trim_end().to_string();
        out.push_str("\n\n");
        return Some(out);
    }
```

(Confirm the caller — `transcode_sse_response` ~198-245 — writes the returned `String` directly into the output body; match its framing convention. If the caller already appends `\n\n`, drop the manual terminator here.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-core --test messages_ingress`
Expected: PASS — the tokentrimmer frame is forwarded verbatim; ChatCompletionChunk + None cases unchanged.

- [ ] **Step 5: Streaming integration assertion (if feasible) + commit**

If the harness supports a streaming `/v1/messages` panel request, assert the Anthropic SSE output contains an `event: tokentrimmer.panel` line and an `event: tokentrimmer.usage` line alongside the Anthropic content events. (If a full streaming integration test is impractical with the mocks, the `process_openai_frame` unit test is the gate; note that in the report.)

```bash
cargo fmt -- crates/core/src/routes/messages.rs crates/core/tests/messages_ingress.rs
cargo clippy -p tt-core --lib --tests
cargo test -p tt-core --test messages_ingress --test responses_panel
git add -A && git commit -m "feat(panel): forward tokentrimmer.* SSE events through /v1/messages transcode (Phase 6)"
```

---

## Final whole-branch review
After Task 4, dispatch the whole-branch reviewer (superpowers:requesting-code-review) on `feat/panel-phase6-transcoders` vs `main`, attention lens = the Global Constraints (off-by-default byte-parity on non-panel requests; cost-on-streaming via the forwarded usage event; render-shape consistency with chat completions). Then `superpowers:finishing-a-development-branch` (user default: push + PR + merge-on-green + sync-main).

## Self-Review (plan vs spec)
- **Spec coverage:** D3 tt_extras → Task 1; D1/D2 non-streaming render → Tasks 2 (responses) + 3 (messages, shared helper); D4 streaming forward → Task 4; D5 off-by-default → no-panel regression assertions in Tasks 2/3/4. Invariants 1–6 each map to a test.
- **Placeholder scan:** all code blocks are concrete; the one "confirm the caller's framing convention" note in Task 4 is a cited read-at-impl detail (the implementer reads `transcode_sse_response`), not a TBD.
- **Type consistency:** `graft_tokentrimmer_panel(&mut Value, &[u8])` defined in Task 2 is consumed identically in Task 3; both transcoders pass their already-buffered `bytes` and their conversion's returned `Value`.
