# Slice 1a-0: `complete_once` extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the chat handler's non-streaming completion pipeline into a reusable in-process `complete_once(state, ctx, req) -> CompletionOutcome` core that the future server-side agent loop calls per turn — **behavior-preserving**, no new feature.

**Architecture:** The 5000-line `crates/core/src/routes/chat.rs::handler` interweaves a shared setup (route → redaction → compression → agentic-budget → cache-behavior) with a branch (streaming | non-streaming). This refactor carves the route→dispatch→response pipeline into a callable core, leaving `handler` as the axum-glue wrapper (extractor parse, sandbox short-circuit, streaming branch unchanged, final Response/header assembly). The hard contract: **every existing `crates/core` test stays green** — this is a pure refactor on the money + circuit-breaker hot path (the ARCH-1 stuck-breaker bug lived here).

**Tech Stack:** Rust, axum, `tt-shared` message types, the existing `crate::failover` + provider dispatch.

**Spec:** `docs/superpowers/specs/2026-06-17-server-side-agentic-loop-slice1a-design.md` (slice 1a-0 section).

**Why contract-style, not line-by-line:** a behavior-preserving move of ~900 interwoven lines cannot be pre-written as bite-sized code blocks without being instantly stale; the existing test suite **is** the spec. Each task below defines a boundary + an acceptance gate; the implementer carves against the real code and iterates until the gate is green.

**Reference — verified handler structure (`crates/core/src/routes/chat.rs`):**
- `pub async fn handler(State<AppState>, Extension<TraceId>, Option<Extension<ApiKeyContext>>, Option<Extension<RetrievalTelemetry>>, HeaderMap, Json<ChatCompletionRequest>) -> ApiResult<Response>` (~853).
- Phases in order: provider resolve (865–875) · sandbox short-circuit for `tt_test_*` (877–903) · auth/ctx build → `RequestContext` (905–990) · `apply_routing` rewrites `req.model` (992–1022) · route-action capture incl. `route_agentic_budget`, `route_fallbacks` (1034–1101) · redaction/compression (1435–1552) · agentic-budget `plan()` (1567–1586) · cache-behavior + body-capture gating (1678–1733) · **`if req.stream` branch (1735)** · non-streaming block: L1/L2 lookup, single-flight, dispatch (`provider.chat_completion` ~2297 / `crate::failover::dispatch_with_failover` ~2322), output shaping, cost, L1/L2 insert, `request_logs` write, response (2096–3016) · final `Json(response).into_response()` + `attach_cost_headers`/`attach_warnings` (~2950–3015).
- Types (`crates/shared/src/messages.rs`): `ChatCompletionRequest` (119–181), `Message` (183–206; `Assistant { content, tool_calls: Vec<ToolCall>, name }`, `Tool { content, tool_call_id }`), `ChatCompletionResponse { id, object, created, model, choices: Vec<Choice>, usage: Usage }`, `Choice { index, message, finish_reason }`, `Usage { prompt_tokens, completion_tokens, .. }`, `ToolCall { id, r#type, function: ToolCallFunction { name, arguments } }`.
- `RequestContext` (`crates/shared/src/context.rs`): `{ trace_id, org_id, api_key_id, credentials, tag, deadline }`.
- Tests: inline `#[cfg(test)]` in chat.rs (~15 modules) + `crates/core/tests/` (~61 files, ~309 cases): failover, cache (L1/L2/negative/single-flight), routing (rewrite/flex/batch/minify/compression/diff/format-switch/pauses), auth/tier, streaming/SSE, telemetry/request_logs.

**Cloud-free / public:** all changes in `crates/core`. Public CI gates it.

---

### Task 1: Define the `CompletionOutcome` contract + a no-op skeleton

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add the struct + skeleton near `handler`)

- [ ] **Step 1: Add the outcome struct + skeleton fn (compiles, unused).**

Add a struct that bundles everything the handler needs *after* a non-streaming completion to build the HTTP `Response` (so the extraction can return it instead of inlining response assembly), and a skeleton `complete_once` that is not yet called:

```rust
/// Everything the HTTP wrapper needs to assemble the client `Response` after a
/// single non-streaming completion. Returned by [`complete_once`] so the agent
/// loop (which wants the typed response, not an HTTP `Response`) and the chat
/// handler share one completion pipeline.
pub(crate) struct CompletionOutcome {
    /// The provider/cache response to return (or replay) to the caller.
    pub response: ChatCompletionResponse,
    /// Cost/route/cache/warning metadata the wrapper turns into
    /// `x-tokentrimmer-*` headers (exact fields mirror what the current
    /// non-streaming tail reads when calling `attach_cost_headers` /
    /// `attach_warnings`). Implementer populates from the moved code.
    pub headers: CompletionHeaders,
}
```

The `CompletionHeaders` fields are whatever the current `attach_cost_headers` + `attach_warnings` + the OTel span attrs read at the non-streaming tail (cost_usd, baseline_cost_usd, cache layer, matched route, savings, warnings, served model, token counts, …). The implementer reads the tail (~2660–3015) and defines `CompletionHeaders` to carry exactly those values — no more.

- [ ] **Step 2: Build.** Run `cargo build -p tt-core`. Expected: PASS (skeleton unused → an `#[allow(dead_code)]` on the skeleton until Task 2 wires it is acceptable).

- [ ] **Step 3: Commit.**
```bash
git add crates/core/src/routes/chat.rs
git commit -m "refactor(core): CompletionOutcome contract for complete_once extraction (slice 1a-0)"
```

---

### Task 2: Carve the non-streaming pipeline into `complete_once`

**Files:**
- Modify: `crates/core/src/routes/chat.rs`

**Boundary contract (the key judgment, made against real code):**
- `complete_once(state: &AppState, ctx: &RequestContext, mut req: ChatCompletionRequest, /* the setup inputs */) -> ApiResult<CompletionOutcome>` performs the **non-streaming** completion: it must reproduce, byte-for-byte in behavior, the current `else` (non-streaming) arm — L1/L2 lookup, single-flight coalesce, provider dispatch (single + `dispatch_with_failover`), output shaping, cost compute, L1/L2 insert, `request_logs` write — and return the response + header meta instead of building the HTTP `Response`.
- **Shared setup (routing → route-action capture → redaction → compression → agentic-budget → cache-behavior → body-capture gating, phases at 992–1733) must be reachable per call** so the agent loop re-routes/redacts each turn. Two acceptable shapes — implementer picks whichever keeps the streaming path *untouched* and the diff smallest:
  - **(a)** `complete_once` takes `(state, ctx, req)` and runs the shared setup internally, then the non-streaming pipeline. The handler's non-streaming arm becomes `complete_once(...)`; the **streaming arm keeps its current inline setup + streaming dispatch unchanged**. (Setup logic is then shared via a small `prepare(...)` helper both arms call, OR duplicated only if extraction proves riskier than duplication — but prefer the shared helper.)
  - **(b)** If sharing the setup cleanly is too invasive, `complete_once` takes the already-prepared setup locals as inputs (a `Prepared` bundle the handler builds before the branch), and the loop builds the same `Prepared` per turn via a public `prepare(...)`. 
- The **streaming arm's behavior must not change** under either shape. The **sandbox short-circuit + auth/ctx build (phases 1–3) stay in `handler`** (they parse extractors/headers — not part of a reusable completion core).
- Preserve **all early returns** in the non-streaming block (e.g. an L1/L2 cache hit returns early): inside `complete_once` they become `return Ok(CompletionOutcome { .. })`.

- [ ] **Step 1: Carve.** Move the non-streaming pipeline body into `complete_once`, rewire the handler's non-streaming arm to call it and assemble the `Response` from the returned `CompletionOutcome` (using the existing `attach_cost_headers`/`attach_warnings` against `outcome.headers`). Make the shared-setup decision per the contract above. Do NOT touch the streaming arm's behavior.

- [ ] **Step 2: Build + clippy.** Run `cargo build -p tt-core` then `cargo clippy -p tt-core --all-targets -- -D warnings`. Expected: PASS (remove the Task-1 `#[allow(dead_code)]`).

- [ ] **Step 3: THE GATE — full suite green (behavior preservation).** Run `cargo test -p tt-core`. Expected: ALL pass (the same ~309+ cases that passed before this PR). This is the behavior-preservation proof. If ANY test regresses, the carve changed behavior — fix until green; do NOT alter a test to match new behavior.

- [ ] **Step 4: Confirm no behavior drift on the streaming path specifically.** Run the streaming/SSE tests explicitly: `cargo test -p tt-core --test concurrent_sse --test streaming_cache_write` (adjust to the real streaming test file names found in `crates/core/tests/`). Expected: PASS.

- [ ] **Step 5: Commit.**
```bash
git add crates/core/src/routes/chat.rs
git commit -m "refactor(core): extract complete_once non-streaming pipeline; handler is the wrapper (slice 1a-0)"
```

---

### Task 3: Expose the seam the loop needs + document it

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (visibility + doc)

- [ ] **Step 1: Set visibility.** Ensure `complete_once` (and `CompletionOutcome`, `CompletionHeaders`, and `prepare`/`Prepared` if shape (a)/(b) introduced them) are `pub(crate)` so the future `agent_run` module in `crates/core/src/routes/` can call them. Add a doc comment on `complete_once` stating its contract: "one routed, metered, cached non-streaming completion; the agent loop calls this per turn." Do not over-expose (keep it `pub(crate)`, not `pub`).

- [ ] **Step 2: Build + the gate again.** `cargo build -p tt-core && cargo test -p tt-core`. Expected: PASS (unchanged behavior).

- [ ] **Step 3: Commit.**
```bash
git add crates/core/src/routes/chat.rs
git commit -m "refactor(core): expose complete_once seam (pub(crate)) for the agent loop (slice 1a-0)"
```

---

## Final verification (mirror the required public CI checks)

- [ ] Run the gate the way CI will:
```bash
cargo fmt -p tt-core -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p tt-core                       # behavior-preservation gate (~309+)
cargo test --workspace --no-run             # all test targets compile (CI memory: signature ripples)
cargo run -q -p tt-cli -- inspect .         # tt inspect . required check
cargo test -p tt-plan-core                  # determinism goldens untouched (this PR doesn't touch plan-core)
```
Expected: all green. The plan-replay determinism goldens MUST be unchanged (this refactor is gateway-only).

> Per the CI memory: a signature ripple can pass `cargo build` but fail test targets — always run `clippy --workspace --all-targets` + `test --workspace --no-run`. `cli_spawn_smoke` may time out locally but passes in CI.

---

## Self-Review (against the spec)

**Spec coverage (slice 1a-0):** extract `complete_once` reusable core (Task 2) · `handler` becomes the axum-glue wrapper with the streaming branch unchanged (Task 2 boundary contract) · behavior-preserving, gated by the full `tt-core` suite (Task 2 Step 3 + final verification) · seam exposed `pub(crate)` for the loop (Task 3). ✓ No new feature, no `/v1/chat/completions` behavior change. ✓

**Placeholder note:** the moved-code "steps" are intentionally contract+gate, not pre-written 900-line blocks — the right granularity for a behavior-preserving refactor where the existing tests are the spec. The `CompletionOutcome`/`CompletionHeaders` field sets are defined by what the current response-assembly tail reads (Task 1 Step 1), not invented.

**Type consistency:** `complete_once(state, ctx, req) -> ApiResult<CompletionOutcome>`, `CompletionOutcome { response: ChatCompletionResponse, headers: CompletionHeaders }` are referenced consistently across Tasks 1–3 and are what slice 1a's loop will call through a `TurnCompleter` seam.

**Risk:** highest-risk change of the sweep (hot-path refactor). Mitigation: isolated PR, no feature, the ~309-case suite as a hard behavior gate, streaming path explicitly untouched + separately tested, and the `prepare` vs `Prepared` shape chosen to minimize the diff. If the carve proves intractable behavior-preservingly, the implementer reports BLOCKED with the specific entanglement (e.g. a local mutated across the branch) rather than altering a test.
