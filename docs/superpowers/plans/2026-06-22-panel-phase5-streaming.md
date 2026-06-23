# Deep Research Panel — Phase 5 (Streaming Panel UX) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stream the deep-research panel arbiter's answer to the client over SSE when the client sends `stream: true`, with aggregate billing unchanged.

**Architecture:** Member legs stay non-streaming (quorum needs them buffered); only the arbiter streams. Synthesize streams live upstream tokens; BestOfN/Majority chunk-replay the verbatim selected leg answer via the existing `fake_stream_from_response`. Both converge on one `BoxStream<ChatCompletionChunk>` fed into the existing `sse::stream_response` pipeline with a panel-aware `StreamLogContext`. The `DropGuard` writes one aggregate `request_logs` row (`provider='panel'`, `cost = Σ legs + arbiter`) after the stream drains; a trailing `tokentrimmer.panel` SSE event carries per-leg attribution. An `ArbiterCostPlan` prevents double-counting the replayed leg's tokens.

**Tech Stack:** Rust, `crates/core` (tt-core), axum SSE, tokio, `async-trait`. All work is in the **public** repo on branch `feat/panel-phase5-streaming` (already created; the design spec is committed there).

**Spec:** `docs/superpowers/specs/2026-06-22-panel-phase5-streaming-design.md` (read it; sections referenced as §N below).

## Global Constraints

- **Off-by-default / wire-identical:** absent the `X-TokenTrimmer-Panel` header, `prep.panel.is_none()` and every new `panel` field is `None`; the single-model streaming path must emit **byte-identical** events. Every new struct field is `Option`; every new code path branches on `panel.is_some()`.
- **One served, one row:** a streaming panel bumps `record_request_served` exactly once (already done inside `stream_response` at `sse.rs:1062`); writes exactly one `request_logs` row, `provider='panel'`, `cached=false`, `cost_usd = Σ legs + arbiter`. Do **not** add any extra `record_request_served` call on this path.
- **No double-count (invariant 3):** `ArbiterCostPlan::Known(c)` MUST return `c` and ignore accumulated usage. For BestOfN/Majority the accumulated stream usage is the *replayed chosen-leg tokens, already counted in `Σ legs`*.
- **Fail-closed before 200:** over-budget (already gated in `prepare()`), unpriceable, or quorum-unmet ⇒ `402`/`502` returned synchronously before `stream_response` is called. No half-open stream.
- **No cache:** the panel `StreamLogContext` sets `cache_insert = None`.
- **No cloud change.** No change to quorum, budget-gate math, `panel_legs` schema, or the three selection algorithms. Majority arbiter cost stays `None` (embeddings unmetered).
- **CI gates (verify locally before claiming green):** `cargo fmt --check` (stage only your files), `cargo clippy -p tt-core --lib --tests`, `cargo test -p tt-core --lib --tests`. Do NOT use `--all-targets` for the clippy/test gate. The workspace test job is disk-flaky — rerun once on a transient failure.

---

### Task 1: `ArbiterCostPlan` — the no-double-count cost guard

**Files:**
- Modify: `crates/core/src/routes/panel.rs` (add the enum + impl near `ArbiterOutcome`, ~line 388)
- Test: `crates/core/src/routes/panel.rs` (add a `#[cfg(test)] mod arbiter_cost_plan_tests` at the bottom of the file)

**Interfaces:**
- Produces: `pub enum ArbiterCostPlan { Live, Known(Option<f64>) }` and `impl ArbiterCostPlan { pub fn finalize(&self, streamed_arbiter_cost_usd: Option<f64>) -> Option<f64> }`.
- `finalize` takes the cost the `DropGuard` already computed from the **streamed** answer's accumulated usage (for `Live`, this is the real arbiter cost; for `Known`, it is the replayed-leg cost and MUST be discarded).

- [ ] **Step 1: Write the failing test**

```rust
// at the bottom of crates/core/src/routes/panel.rs
#[cfg(test)]
mod arbiter_cost_plan_tests {
    use super::{ArbiterCostPlan};

    #[test]
    fn known_ignores_streamed_cost() {
        // BestOfN/Majority: the streamed usage is the replayed leg, already in Σ legs.
        let plan = ArbiterCostPlan::Known(Some(0.0021));
        assert_eq!(plan.finalize(Some(999.0)), Some(0.0021)); // streamed cost discarded
        let none = ArbiterCostPlan::Known(None); // Majority: embeddings unmetered
        assert_eq!(none.finalize(Some(999.0)), None);
    }

    #[test]
    fn live_uses_streamed_cost() {
        // Synthesize: fresh arbiter tokens — price what was streamed.
        let plan = ArbiterCostPlan::Live;
        assert_eq!(plan.finalize(Some(0.0042)), Some(0.0042));
        assert_eq!(plan.finalize(None), None); // unpriceable arbiter model
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-core --lib arbiter_cost_plan_tests`
Expected: FAIL — `cannot find type ArbiterCostPlan`.

- [ ] **Step 3: Write minimal implementation**

```rust
// crates/core/src/routes/panel.rs — insert after `ArbiterOutcome` (~line 388)

/// How the aggregate billing path obtains the arbiter's cost at stream-end.
///
/// Streaming panels defer the single `request_logs` row to the `DropGuard`,
/// which sees the *streamed* answer's accumulated usage. For `Synthesize`
/// the streamed tokens are fresh arbiter work and must be priced (`Live`).
/// For `BestOfN`/`Majority` the streamed tokens are a **replay of a member
/// leg's answer already counted in `Σ legs`** — repricing them would
/// double-count, so the cost is fixed up front and the streamed figure is
/// discarded (`Known`). See spec §5.4 (invariant 3).
#[derive(Clone, Debug)]
pub enum ArbiterCostPlan {
    /// Price the streamed answer's accumulated usage (Synthesize live arbiter).
    Live,
    /// Use this pre-computed cost; ignore the streamed usage (replay strategies).
    Known(Option<f64>),
}

impl ArbiterCostPlan {
    /// Resolve the arbiter's contribution to the aggregate.
    ///
    /// `streamed_arbiter_cost_usd` is the cost the `DropGuard` computed from
    /// the streamed answer's accumulated usage. `Known` discards it.
    pub fn finalize(&self, streamed_arbiter_cost_usd: Option<f64>) -> Option<f64> {
        match self {
            ArbiterCostPlan::Live => streamed_arbiter_cost_usd,
            ArbiterCostPlan::Known(c) => *c,
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p tt-core --lib arbiter_cost_plan_tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/panel.rs
git commit -m "feat(panel): ArbiterCostPlan — no-double-count guard for streaming arbiter cost"
```

---

### Task 2: Extract `run_panel_legs_and_quorum` from `run_panel`

**Files:**
- Modify: `crates/core/src/routes/panel.rs` (`run_panel` @1050–1220; extract phases 1–2 into a new helper)
- Test: `crates/core/tests/panel_fanout.rs` + `crates/core/tests/panel_engine.rs` (existing — must stay green unchanged)

**Interfaces:**
- Produces: `pub(crate) async fn run_panel_legs_and_quorum(state, ctx, base_req, creds, cfg, deadline) -> Result<(Vec<LegResult>, Option<f64>), ApiError>` returning the completed member legs and the **leg-only** None-aware cost sum (`leg_cost_total`).
- Consumes (Task 6): `complete_panel_streaming` calls this for the streaming path.
- `run_panel` is refactored to call this helper, then run its existing arbiter + aggregation steps. **Behavior of the non-streaming path is unchanged** (regression-gated).

- [ ] **Step 1: Run the existing panel suite to capture the green baseline**

Run: `cargo test -p tt-core --test panel_fanout --test panel_engine --test panel_dispatch --test panel_arbiter`
Expected: PASS (record the counts; they must be identical after the refactor).

- [ ] **Step 2: Extract the helper (behavior-preserving)**

Add this function immediately above `run_panel` (~line 1049). Move the body of `run_panel` lines **1061–1173** (leg resolve/spawn loop, the `join_next` collect loop, and the quorum check) verbatim into it, then compute `leg_cost_total` with the existing `sum_metered_iter` over the member legs only:

```rust
/// Phases 1–2 of a panel run: fan out member legs concurrently, join them, and
/// enforce quorum. Returns the completed member legs plus the None-aware
/// **leg-only** cost sum. Shared by `run_panel` (non-streaming) and
/// `complete_panel_streaming` (Phase 5). The arbiter step lives in the callers.
pub(crate) async fn run_panel_legs_and_quorum(
    state: &crate::AppState,
    ctx: &tt_shared::RequestContext,
    base_req: &ChatCompletionRequest,
    creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    cfg: &PanelConfig,
    deadline: Duration,
) -> Result<(Vec<LegResult>, Option<f64>), crate::ApiError> {
    use std::time::Instant;
    use tokio::task::JoinSet;

    // <<< MOVE run_panel lines 1061–1173 here verbatim (leg loop, join loop,
    //     quorum check). The `return Err(PanelQuorumUnmet { ... })` stays. >>>

    let leg_cost_total = sum_metered_iter(legs_out.iter().map(|l| l.cost_usd));
    Ok((legs_out, leg_cost_total))
}
```

- [ ] **Step 3: Rewrite `run_panel` to call the helper**

Replace `run_panel`'s body lines 1061–1208 with:

```rust
    let (legs_out, leg_cost_total) =
        run_panel_legs_and_quorum(state, ctx, base_req, creds, cfg, deadline).await?;
    let required = cfg.quorum.unwrap_or(match cfg.strategy {
        ArbiterStrategyKind::Majority => (cfg.members.len() / 2) + 1,
        _ => 1,
    });
    let met = legs_out.iter().filter(|l| matches!(l.status, LegStatus::Ok)).count();
    // (legs_out is already quorum-checked inside the helper; `required`/`met`
    //  are recomputed here only to populate PanelResult.)

    // 3. Arbitrate. (existing lines 1175–1208 unchanged below, but the cost sum
    //    is now leg_cost_total + arb.cost_usd instead of re-summing legs.)
    let strategy = strategy_for(cfg)?;
    let arb_start = std::time::Instant::now();
    let mut legs_out = legs_out;
    let arb = strategy.arbitrate(base_req, &legs_out, state, ctx, creds).await?;
    let arb_latency_ms = arb_start.elapsed().as_millis() as u64;
    // ... (arbiter_provider_id + arbiter_leg construction unchanged: lines 1183–1200) ...
    let total_cost_usd = sum_metered_iter(
        std::iter::once(leg_cost_total).chain(std::iter::once(arb.cost_usd)),
    );
    legs_out.push(arbiter_leg);
    Ok(PanelResult { response: arb.response, legs: legs_out, total_cost_usd,
                     quorum_required: required, quorum_met: met, arbiter_detail: arb.detail })
```

Note: `sum_metered_iter(once(leg_cost_total).chain(once(arb.cost_usd)))` is None-aware and equals the previous `legs.map(cost).chain(once(arb.cost))` sum (the helper already None-aware-summed the legs).

- [ ] **Step 4: Run the suite — assert identical green**

Run: `cargo test -p tt-core --test panel_fanout --test panel_engine --test panel_dispatch --test panel_arbiter --test panel_budget`
Expected: PASS with the **same counts** as Step 1.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt -- crates/core/src/routes/panel.rs && cargo clippy -p tt-core --lib --tests
git add crates/core/src/routes/panel.rs
git commit -m "refactor(panel): extract run_panel_legs_and_quorum (behavior-preserving)"
```

---

### Task 3: `arbitrate_streaming` — trait method, default replay impl, Synthesize live override

**Files:**
- Modify: `crates/core/src/routes/panel.rs` (`ArbiterStrategy` trait @437; `Synthesize` impl @468)
- Test: `crates/core/tests/panel_streaming.rs` (new)

**Interfaces:**
- Produces on the trait:
  ```rust
  async fn arbitrate_streaming(
      &self,
      request: &ChatCompletionRequest,
      legs: &[LegResult],
      state: &AppState,
      ctx: &RequestContext,
      creds: &HashMap<String, ProviderCredentials>,
  ) -> Result<(BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ArbiterCostPlan, ArbiterDetail), ApiError>;
  ```
- Default impl (covers BestOfN/Majority/future): call `self.arbitrate(...)`, then `(fake_stream_from_response(outcome.response), ArbiterCostPlan::Known(outcome.cost_usd), outcome.detail)`.
- `Synthesize` overrides it to dispatch `chat_completion_stream` and return `ArbiterCostPlan::Live`.
- Consumes: `ArbiterCostPlan` (Task 1), `surviving_answers` (existing), `fake_stream_from_response` (`sse.rs:1212`).

- [ ] **Step 1: Write the failing tests**

Create `crates/core/tests/panel_streaming.rs`. Use the existing mock-provider harness pattern from `crates/core/tests/panel_arbiter.rs` (mock providers implementing `Provider`, including `chat_completion_stream`). Two unit-level tests through the public arbiter API:

```rust
// Test A: BestOfN default-impl replay yields a Known plan + the chosen leg verbatim.
// (Set up 3 mock legs already buffered into LegResult; a mock judge provider whose
//  chat_completion returns "2\nmost complete"; call BestOfN::arbitrate_streaming;
//  collect the returned BoxStream into text; assert it equals leg-index-2's answer,
//  assert matches!(plan, ArbiterCostPlan::Known(_)), assert detail.chosen_leg == Some(2).)

// Test B: Synthesize override yields a Live plan + streams the arbiter's chunks.
// (Mock arbiter provider whose chat_completion_stream yields ["Synth", "esized"];
//  call Synthesize::arbitrate_streaming; collect the BoxStream; assert text ==
//  "Synthesized"; assert matches!(plan, ArbiterCostPlan::Live).)
```

(Write both tests in full following `panel_arbiter.rs`'s mock setup — that file already constructs `LegResult`s and mock providers; mirror it. A helper to drain a `BoxStream<ChatCompletionChunk>` into a `String` by concatenating `choices[0].delta.content` belongs in this test file.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-core --test panel_streaming`
Expected: FAIL — `no method named arbitrate_streaming`.

- [ ] **Step 3: Add the trait method with a default impl**

```rust
// in `pub trait ArbiterStrategy` (after `arbitrate`, ~line 453):
/// Streaming variant. Default impl runs the buffered `arbitrate` then replays
/// the chosen response as a chunk stream (`Known` cost — the replayed tokens
/// are already a member leg's, counted in Σ legs; see ArbiterCostPlan). Live
/// strategies (Synthesize) override this. (spec §5.3)
async fn arbitrate_streaming(
    &self,
    request: &ChatCompletionRequest,
    legs: &[LegResult],
    state: &AppState,
    ctx: &RequestContext,
    creds: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
) -> Result<
    (
        futures::stream::BoxStream<'static, Result<ChatCompletionChunk, tt_shared::ProviderError>>,
        ArbiterCostPlan,
        ArbiterDetail,
    ),
    ApiError,
> {
    let outcome = self.arbitrate(request, legs, state, ctx, creds).await?;
    Ok((
        crate::routes::sse::fake_stream_from_response(outcome.response),
        ArbiterCostPlan::Known(outcome.cost_usd),
        outcome.detail,
    ))
}
```

(Add `use` for `ChatCompletionChunk` if not already imported in panel.rs.)

- [ ] **Step 4: Override for `Synthesize` (live stream)**

Add to `impl ArbiterStrategy for Synthesize` an `arbitrate_streaming` that reuses the exact prompt/credential/provider resolution from `Synthesize::arbitrate` (lines 478–548) but builds `arbiter_req` with `stream: true` and dispatches the streaming API:

```rust
async fn arbitrate_streaming(
    &self, request, legs, state, ctx, creds,
) -> Result<(BoxStream<...>, ArbiterCostPlan, ArbiterDetail), ApiError> {
    // <<< reuse lines 478–519 verbatim: surviving_answers guard + synthesis
    //     instruction + messages assembly >>>
    let arbiter_req = ChatCompletionRequest {
        model: self.arbiter_model.model.clone(),
        messages,
        stream: true,              // <-- the only change vs buffered
        max_tokens: Some(4096),
        ..Default::default()
    };
    // <<< reuse lines 531–548 verbatim: provider resolve + arb_ctx credential subst >>>
    let stream = provider
        .chat_completion_stream(arbiter_req, arb_ctx)
        .await
        .map_err(|e| ApiError::ServiceUnavailable(format!("arbiter stream failed: {e}")))?;
    Ok((stream, ArbiterCostPlan::Live, ArbiterDetail::default()))
}
```

Failure posture (spec §D5): a pre-first-chunk establishment failure returns `Err` here (proper non-200, before `stream_response`). The single-model arbiter has no failover candidates — only the provider adapter's own retry applies.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p tt-core --test panel_streaming`
Expected: PASS (Test A + Test B).

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt -- crates/core/src/routes/panel.rs crates/core/tests/panel_streaming.rs
cargo clippy -p tt-core --lib --tests
git add crates/core/src/routes/panel.rs crates/core/tests/panel_streaming.rs
git commit -m "feat(panel): arbitrate_streaming — replay default + Synthesize live override"
```

---

### Task 4: Panel-aware `StreamLogContext` + `DropGuard` aggregate billing

**Files:**
- Modify: `crates/core/src/routes/sse.rs` (`StreamLogContext` struct @382; the `DropGuard` closure @736–883)
- Test: `crates/core/tests/panel_streaming.rs` (extend — assert the DropGuard writes one `provider='panel'` aggregate row via a mock `RequestLogWriter`)

**Interfaces:**
- Produces:
  ```rust
  pub struct PanelStreamLog {
      pub leg_records: Vec<crate::routes::panel::LegResult>,
      pub leg_cost_total: Option<f64>,
      pub strategy: crate::routes::panel::ArbiterStrategyKind,
      pub quorum_required: usize,
      pub quorum_met: usize,
      pub arbiter_detail: crate::routes::panel::ArbiterDetail,
      pub arbiter_cost_plan: crate::routes::panel::ArbiterCostPlan,
      pub arbiter_model: String,
      pub panel_leg_writer: Option<Arc<dyn tt_telemetry::PanelLegWriter>>,
  }
  ```
  and a new field on `StreamLogContext`: `pub panel: Option<Arc<PanelStreamLog>>` (defaults to `None`).
- Consumes: `ArbiterCostPlan`, `LegResult`, `ArbiterDetail` (Tasks 1/3); `PanelLegWriter`, `PanelLegRow` (Phase 2).

- [ ] **Step 1: Write the failing test**

Extend `panel_streaming.rs`: build a `StreamLogContext` with `panel: Some(Arc::new(PanelStreamLog { leg_cost_total: Some(0.010), arbiter_cost_plan: ArbiterCostPlan::Known(Some(0.002)), strategy: BestOfN, ... a mock RequestLogWriter capturing rows, ... }))`, drive a small replay stream through `stream_response`, drain it to completion (drop the body), then assert the captured row has `provider == "panel"`, `cost_usd == 0.012`, `cached == false`, and exactly one row was written. Add a second assertion: with `ArbiterCostPlan::Known`, the replayed-leg cost is NOT added (cost stays `0.012`, not `0.012 + replayed`).

(Reuse the mock-writer pattern from `crates/core/tests/sse_partial_cost.rs` / `streaming_cache_write.rs`, which already exercise the DropGuard with a capturing writer.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-core --test panel_streaming panel_aggregate_row`
Expected: FAIL — `no field panel on StreamLogContext`.

- [ ] **Step 3: Add `PanelStreamLog` + the `panel` field**

Add `PanelStreamLog` (above `StreamLogContext`) and `pub panel: Option<Arc<PanelStreamLog>>` as the last field of `StreamLogContext`. Update every `StreamLogContext { ... }` constructor in the codebase to add `panel: None` (grep: `StreamLogContext {`). This keeps non-panel callers wire-identical.

- [ ] **Step 4: Branch the `DropGuard` on `panel`**

Capture `let panel = ctx.panel.clone();` into the guard closure alongside the existing field moves. After the existing `breakdown`/`cost_usd` computation (line 772), branch:

```rust
// `cost_usd` (computed above) is the STREAMED answer's cost. For panels it is
// the arbiter cost only when Live; for replay it is the chosen leg's cost which
// is already in leg_cost_total — discard it via the cost plan.
let (cost_usd, baseline_cost_usd, row_provider, row_model) = if let Some(p) = panel.as_ref() {
    let arbiter_cost = p.arbiter_cost_plan.finalize(Some(cost_usd));
    let total = sum_metered_iter(
        std::iter::once(p.leg_cost_total).chain(std::iter::once(arbiter_cost)),
    ).unwrap_or(0.0);
    (total, total, "panel".to_string(), p.arbiter_model.clone())
} else {
    (cost_usd, baseline_cost_usd, provider_id_log.clone(), model.clone())
};
```

Use `row_provider`/`row_model` for the `RequestLogRow.provider`/`.model` fields (currently `provider_id_log` / `model.clone()` at lines 834–835), and the panel `cost_usd`/`baseline_cost_usd` for the row + `spend_sink.record`. When `panel.is_some()`: after building/spawning the row, spawn the `panel_legs` rows from `p.leg_records` via `p.panel_leg_writer` (copy the fire-and-forget block from `complete_panel` lines 1484–1518, using `tracker` for the spawn), and fill the panel span attributes (lines 820–824) from `p.strategy`/`leg_records.len()`/`quorum_*` instead of `None`.

`sum_metered_iter` lives in `panel.rs`; re-export or `use crate::routes::panel::sum_metered_iter`.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p tt-core --test panel_streaming panel_aggregate_row`
Expected: PASS — one `provider='panel'` row, `cost_usd == 0.012`.

- [ ] **Step 6: Off-by-default regression + fmt/clippy + commit**

Run: `cargo test -p tt-core --test sse_partial_cost --test streaming_cache_write --test concurrent_sse` (non-panel streaming unchanged)

```bash
cargo fmt -- crates/core/src/routes/sse.rs crates/core/tests/panel_streaming.rs
cargo clippy -p tt-core --lib --tests
git add crates/core/src/routes/sse.rs crates/core/tests/panel_streaming.rs
git commit -m "feat(panel): panel-aware StreamLogContext + DropGuard aggregate billing"
```

---

### Task 5: `tokentrimmer.panel` terminal SSE event + panel-aware `usage_event`

**Files:**
- Modify: `crates/core/src/routes/sse.rs` (`TrackedEventStream` struct @484; its construction @707; terminal queue @593–613; `usage_event` @525–567)
- Test: `crates/core/tests/panel_streaming.rs` (extend — terminal event order + aggregate cost in `tokentrimmer.usage`)

**Interfaces:**
- Produces: a `tokentrimmer.panel` SSE event emitted in `EmitTerminal` **before** `tokentrimmer.usage`; `usage_event`'s `cost_usd` is the aggregate when `panel.is_some()`.
- Consumes: `Arc<PanelStreamLog>` (Task 4) cloned into `TrackedEventStream`; `build_panel_body` shape (mirror `panel.rs:1231`).

- [ ] **Step 1: Write the failing test**

Extend `panel_streaming.rs`: drive a replay stream through `stream_response` with a `panel` context, collect ALL emitted SSE events as strings, and assert the order contains `tokentrimmer.panel` immediately before `tokentrimmer.usage`, both before `[DONE]`; assert the `tokentrimmer.panel` JSON has `legs` + `arbiter.strategy`; assert the `tokentrimmer.usage` event's `cost_usd` equals the aggregate (`0.012`), not the arbiter-only cost.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-core --test panel_streaming panel_terminal_event`
Expected: FAIL — no `tokentrimmer.panel` event in the stream.

- [ ] **Step 3: Thread `panel` into `TrackedEventStream`**

Add `panel: Option<Arc<PanelStreamLog>>` to `TrackedEventStream`; clone it from `log_ctx.panel` at construction (~line 707). Add a `panel_event(&self) -> Option<Event>` method that, when `panel.is_some()`, builds the `build_panel_body`-shaped JSON from `panel.leg_records` + `panel.arbiter_detail` + `panel.strategy` + quorum, and serializes it as `Event::default().event("tokentrimmer.panel").data(json)`. Fail-soft: on serialization error return `None` (drop only the metadata; mirror the non-streaming fallback at `chat.rs:2269`). Factor the body builder so it shares shape with `panel.rs::build_panel_body` (extract a `pub(crate) fn panel_body_json(...)` in panel.rs that both call, to keep one source of truth).

- [ ] **Step 4: Insert into the terminal queue + make `usage_event` panel-aware**

In the terminal queue (lines 600–606), insert the panel event between `include_usage_chunk_event` and `usage_event`:

```rust
if let Some(ev) = self.include_usage_chunk_event() { queue.push_back(ev); }
if let Some(ev) = self.panel_event() { queue.push_back(ev); }     // <-- new
if let Some(ev) = self.usage_event() { queue.push_back(ev); }
queue.push_back(Event::default().data("[DONE]"));
```

In `usage_event` (525–567): when `self.panel.is_some()`, replace the streamed `cost_usd` with the aggregate (`panel.arbiter_cost_plan.finalize(streamed_cost)` + `panel.leg_cost_total`, via `sum_metered_iter`) so the streamed `tokentrimmer.usage` matches the row written by the DropGuard. Leave the non-panel path byte-identical.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p tt-core --test panel_streaming panel_terminal_event`
Expected: PASS — order `… → tokentrimmer.panel → tokentrimmer.usage → [DONE]`, aggregate cost.

- [ ] **Step 6: Off-by-default regression + fmt/clippy + commit**

Run: `cargo test -p tt-core --test concurrent_sse --test sse_partial_cost` (non-panel event bytes unchanged)

```bash
cargo fmt -- crates/core/src/routes/sse.rs crates/core/src/routes/panel.rs crates/core/tests/panel_streaming.rs
cargo clippy -p tt-core --lib --tests
git add -A && git commit -m "feat(panel): tokentrimmer.panel terminal SSE event + aggregate usage_event"
```

---

### Task 6: `complete_panel_streaming` orchestrator + routing wire-up (end-to-end)

**Files:**
- Modify: `crates/core/src/routes/panel.rs` (add `complete_panel_streaming` near `complete_panel` @1367)
- Modify: `crates/core/src/routes/chat.rs` (the gate @2206)
- Test: `crates/core/tests/panel_streaming.rs` (extend — router-level end-to-end)

**Interfaces:**
- Produces: `pub(crate) async fn complete_panel_streaming(state, ctx, prep, cfg) -> Result<Response, ApiError>`.
- Consumes: `run_panel_legs_and_quorum` (Task 2), `arbitrate_streaming` (Task 3), `PanelStreamLog`/panel `StreamLogContext` (Task 4), `sse::stream_response`.

- [ ] **Step 1: Write the failing end-to-end tests**

Extend `panel_streaming.rs` with router-level tests (build the app like `panel_engine.rs` does, send `POST /v1/chat/completions` with `stream: true` + `X-TokenTrimmer-Panel: synthesize|best-of-n|majority`):
1. **Synthesize live** ⇒ `200`, `content-type: text/event-stream`; body contains the arbiter chunks then `tokentrimmer.panel` then `tokentrimmer.usage` then `[DONE]`; the captured `request_logs` row is one `provider='panel'` row, `cost = Σ legs + arbiter`, `cached=false`; `record_request_served` bumped once.
2. **BestOfN** ⇒ streamed chunks reconstruct the chosen leg verbatim; aggregate cost = `Σ legs + judge` (assert NOT `+ replayed-leg`).
3. **Quorum-unmet** (mock legs all error) ⇒ `502`, **zero** `request_logs` rows, no stream.
4. **Off-by-default**: `stream:true` with no panel header ⇒ byte-identical single-model stream (regression vs `concurrent_sse.rs`).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-core --test panel_streaming streaming_e2e`
Expected: FAIL — panel `stream:true` currently returns a buffered JSON body (gate forces non-streaming).

- [ ] **Step 3: Implement `complete_panel_streaming`**

Mirror `complete_panel` (1367–1578) but defer the row/spend to the DropGuard:

```rust
pub(crate) async fn complete_panel_streaming(
    state: &AppState, ctx: &RequestContext, prep: Prepared, cfg: PanelConfig,
) -> Result<Response, ApiError> {
    let deadline = prep.request_timeout.unwrap_or_else(|| Duration::from_secs(120));
    // Budget gate already ran in prepare(); do NOT re-gate. Legs + quorum:
    let (legs, leg_cost_total) =
        match run_panel_legs_and_quorum(state, ctx, &prep.req, &prep.panel_creds, &cfg, deadline).await {
            Ok(v) => v,
            Err(e) => {  // 502 quorum-unmet etc. — before any stream, zero rows.
                let outcome = match &e { ApiError::PanelQuorumUnmet { .. } => "quorum_unmet", _ => "error" };
                crate::metrics::record_panel_request(cfg.strategy.as_str(), outcome);
                return Err(e);
            }
        };
    let required = cfg.quorum.unwrap_or(match cfg.strategy { ArbiterStrategyKind::Majority => (cfg.members.len()/2)+1, _ => 1 });
    let met = legs.iter().filter(|l| matches!(l.status, LegStatus::Ok)).count();

    // Arbiter → (stream, cost plan, detail). Establishment errors return non-200 here.
    let strategy = strategy_for(&cfg)?;
    let (arbiter_stream, arbiter_cost_plan, arbiter_detail) =
        strategy.arbitrate_streaming(&prep.req, &legs, state, ctx, &cfg.panel_creds_or(ctx)).await?;
    crate::metrics::record_panel_request(cfg.strategy.as_str(), "success");

    let arbiter_provider = state.registry.resolve(&cfg.arbiter_model.model)
        .ok_or_else(|| ApiError::ModelNotFound { model: cfg.arbiter_model.model.clone() })?;

    // Build the arbiter-leg record so panel_legs persistence matches the non-stream path.
    // (Append a LegRole::Arbiter LegResult to `legs` with cost from the plan's Known value
    //  or None for Live — the Live arbiter cost is finalized in the DropGuard. Mirror
    //  run_panel lines 1189–1199; usage None is acceptable for the deferred path.)

    let panel = Arc::new(crate::routes::sse::PanelStreamLog {
        leg_records: legs, leg_cost_total, strategy: cfg.strategy,
        quorum_required: required, quorum_met: met, arbiter_detail, arbiter_cost_plan,
        arbiter_model: cfg.arbiter_model.model.clone(),
        panel_leg_writer: state.panel_leg_writer.clone(),
    });

    // Panel-aware StreamLogContext: arbiter provider pricing (Live finalize uses it),
    // spend_sink/writer/tracker from state, cache_insert: None, span_ctx with panel attrs,
    // panel: Some(panel). (Build mirroring handle_streaming's StreamLogContext @3593–3648,
    //  but pricing = arbiter model pricing and panel = Some(...).)
    let log_ctx = /* StreamLogContext { ... panel: Some(panel), cache_insert: None, ... } */;

    Ok(crate::routes::sse::stream_response(arbiter_stream, &arbiter_provider, ctx.trace_id, Some(log_ctx)))
}
```

(`panel_creds_or(ctx)` denotes passing `prep.panel_creds` — thread it as `complete_panel` does. The arbiter-leg append + the exact `StreamLogContext` field list are filled by reading `complete_panel` 1421–1556 and `handle_streaming` 3593–3648; no new fields beyond `panel`/`cache_insert: None`.)

- [ ] **Step 4: Change the gate (`chat.rs:2206`)**

```rust
if prep.req.stream {
    if let Some(cfg) = prep.panel.take() {
        return crate::routes::panel::complete_panel_streaming(&state, &ctx, prep, cfg)
            .await
            .map_err(Into::into);   // ApiError -> Response via IntoResponse, matching handle_streaming
    }
    return handle_streaming(&state, &ctx, prep).await;
}
```

Remove the obsolete "Streaming arbiter UX is Phase 5" comment (lines 2203–2205). The non-stream panel path (`complete_once` → `complete_panel`) is untouched.

- [ ] **Step 5: Run the end-to-end tests**

Run: `cargo test -p tt-core --test panel_streaming`
Expected: PASS (all e2e cases: Synthesize live, BestOfN, quorum-unmet 502, off-by-default).

- [ ] **Step 6: Full gate + commit**

```bash
cargo fmt -- crates/core/src/routes/panel.rs crates/core/src/routes/chat.rs crates/core/tests/panel_streaming.rs
cargo clippy -p tt-core --lib --tests
cargo test -p tt-core --lib --tests   # whole-crate; rerun once if the disk-flaky job trips
git add -A && git commit -m "feat(panel): complete_panel_streaming + gate wire-up — stream the arbiter over SSE (Phase 5)"
```

---

## Final whole-branch review

After Task 6, dispatch the whole-branch reviewer (superpowers:requesting-code-review) on `feat/panel-phase5-streaming` against `main`, with the global constraints above as the attention lens (money-path invariants 2/3, off-by-default invariant 1, fail-closed invariant 4). Then `superpowers:finishing-a-development-branch` (per the user's standing default: push + PR + sync-main).

## Self-Review (plan vs spec)

- **Spec coverage:** D1 (gate, Task 6) · D2 (Task 3 default replay + Synthesize live) · D3 (Task 5 terminal event + order) · D4 (Task 4 aggregate row + Task 1 cost plan) · D5 (Task 3 establishment-error non-200, Task 6 quorum-502) · D6 (Task 4 `cache_insert: None`). Invariants 1–7 each map to a test in Tasks 1/2/4/5/6.
- **Placeholder scan:** the `<<MOVE …>>` / `/* … */` markers are precise *move-this-range* or *fill-from-cited-lines* instructions for an implementer reading the named code, not vague TODOs — each cites exact source lines. New types (ArbiterCostPlan, PanelStreamLog) and the gate change are shown complete.
- **Type consistency:** `arbitrate_streaming` returns `(BoxStream, ArbiterCostPlan, ArbiterDetail)` in Task 3 and is consumed with that exact shape in Task 6; `PanelStreamLog` fields defined in Task 4 are populated identically in Task 6; `finalize(Option<f64>) -> Option<f64>` (Task 1) is called the same way in Tasks 4 and 5.
