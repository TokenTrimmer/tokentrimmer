# Deep Research Panel — Phase 5 (streaming panel UX) Design Spec

> Status: IMPLEMENTED (Phase 5 — branch feat/panel-phase5-streaming). Date: 2026-06-22. Repo: public. Branch: `feat/panel-phase5-streaming`.
> Builds on Phases 1–4. Master spec: `2026-06-21-deep-research-panel-design.md` (roadmap row 5: "Legs non-streaming; arbiter streams token-by-token to client").
> Hardened against a 4-lens adversarial design review (billing / lifecycle / contract / surface) — the money-path and feasibility blockers it surfaced are resolved inline below.

## 1. Goal

Honor `stream: true` for panel requests. Today a panel request with `stream: true` is silently forced through the buffered path — the gate at `chat.rs:2206` is `if prep.req.stream && prep.panel.is_none()`, with the comment "Streaming arbiter UX is Phase 5." The client asked for an SSE stream and got a JSON blob. Phase 5 closes that gap: the arbiter's answer streams as Server-Sent Events, panel attribution arrives as a trailing SSE event, and **aggregate billing is unchanged** (one `request_logs` row, `cost_usd = Σ legs + arbiter`, `cached = false`, exactly one served increment).

Member legs remain non-streaming — quorum requires every leg fully buffered before arbitration begins (established constraint, unchanged). Only the **arbiter output** streams.

## 2. Key facts the design rests on (verified in code)

- **Real upstream streaming exists end-to-end.** `Provider::chat_completion_stream(req, ctx) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>` (`crates/shared/src/provider.rs:72`); every adapter implements it. `sse::stream_response(stream, provider, trace_id, Option<StreamLogContext>) -> Response` (`sse.rs:659`) accepts **any** provider `BoxStream` unchanged → `UsageTrackingStream` → `TrackedEventStream` → axum `Sse`. **No adapter changes.**
- **`fake_stream_from_response(ChatCompletionResponse) -> BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>`** (`sse.rs:1212`) converts a buffered response into the *exact same chunk-stream type*. (It is also the L1-cache-hit replay mechanism — but see §5.3: panel replay uses it with a **`Some(ctx)`** log context, the opposite of the cache-hit path's `None`.)
- **Served is already bumped by the stream pipeline.** `stream_response` calls `record_request_served("sse", "dispatch")` exactly once, synchronously, in its live-dispatch arm (`sse.rs:1062`), before handing back the `Response`. The cache-hit fake-stream is a *separate* path that logs its own row. **Routing a panel stream through `stream_response` therefore self-accounts for `served` — no extra call, and the handler tail's `record_request_served` (chat.rs:2240) must NOT also run for this path** (it doesn't: the handler returns the streaming `Response` directly, bypassing the non-streaming match arm).
- **Billing finalizes in `DropGuard`** (`sse.rs:736–1048`), which fires when the response body is dropped. It already holds (via `StreamLogContext`, `sse.rs:382`): `writer` (`RequestLogWriter`), `spend_sink`, `pricing`/`baseline_pricing`, `tracker`, `cache_insert`, `span_ctx`. It computes cost from the accumulated `UsageTrackingStream` usage and writes the row. It has **no `&AppState`** — anything it needs (e.g. the `PanelLegWriter`) must be captured into the context up front.
- **The budget gate + credential pre-resolution run in `prepare()`** (`panel_budget_gate` at `chat.rs:2698`, inside `prepare` fn @2339), which the handler calls at `chat.rs:2174` — *before* the stream/non-stream branch @2206. So the gate has already passed by the time we branch; the streaming path does **not** re-gate.
- **Terminal-event machinery already emits TT metadata.** `TrackedEventStream` (`sse.rs:484–627`), `EmitTerminal` phase queues: optional OpenAI-native usage chunk (if `stream_options.include_usage`) → `tokentrimmer.usage` event (`sse.rs:525–567`) → `[DONE]`.
- **Panels bypass cache + single-flight** (`complete_once` branches to panel *before* any cache check, `chat.rs:1053`). Non-deterministic → must never be cached.
- **Only `Synthesize` produces fresh arbiter tokens.** `BestOfN` dispatches a judge call that only *selects*, returns the chosen leg's **original buffered response** verbatim (`panel.rs:582`). `Majority` does embedding clustering with **no dispatch**, returns the medoid leg's **buffered response** verbatim and `cost_usd: None` (`panel.rs:750`, `:915` — embeddings unmetered today).

## 3. Decisions (approved)

- **D1 — Trigger: auto-stream, no new header.** `stream: true` + panel opted-in (`prep.panel.is_some()`) ⇒ the arbiter streams. No `X-TokenTrimmer-Panel-Streaming` flag. Honoring the existing `stream` field is the least-surprising contract.
- **D2 — All three strategies honor `stream: true`; only Synthesize streams *live*.** Synthesize dispatches the arbiter with `stream: true` and pipes live upstream tokens. BestOfN/Majority do their (non-streaming) selection, then **chunk-replay the verbatim selected leg answer** via `fake_stream_from_response`. One uniform client contract — `stream: true` ⇒ SSE, always. (Alt considered: only Synthesize streams, others fall back to JSON — rejected: a strategy-dependent response *medium* is a leaky contract.)
- **D3 — Panel attribution as a trailing `tokentrimmer.panel` SSE event**, same JSON shape as `build_panel_body` (`panel.rs:1231`), emitted **before** `tokentrimmer.usage`, before `[DONE]`. Mirrors the non-streaming body-merge order (attribution, then cost rollup) and the existing `tokentrimmer.usage` precedent.
- **D4 — Aggregate billing unchanged, no double-count.** Exactly one `request_logs` row (`provider='panel'`, `model=arbiter`, `cached=false`, `cost_usd = Σ legs + arbiter`), written in the `DropGuard` after the stream drains. `panel_legs` rows fire-and-forget. `served` bumped once at `sse.rs:1062`. The arbiter cost is finalized via the **`ArbiterCostPlan`** (§5.4): `Live` ⇒ price the freshly-accumulated arbiter usage; `Known(c)` ⇒ return `c` and **ignore** the accumulated usage (for BestOfN/Majority the accumulated usage is the *replayed chosen-leg tokens, already counted in `Σ legs`* — repricing them would double-count). The terminal `tokentrimmer.usage` event reports the **aggregate**. The streamed top-level `usage` is whatever the streamed answer carries (arbiter usage for Synthesize; chosen-leg usage for replay) — exactly as the non-streaming panel response's `usage` field, with per-leg detail living in `panel_legs` + `tokentrimmer.panel`.
- **D5 — Failure posture mirrors the existing streaming path; no new fallback.**
  - Budget gate + quorum run *before* any arbiter dispatch and before the 200 ⇒ `402`/`502` returned synchronously (fail-closed preserved). The budget gate already ran in `prepare()`; quorum runs inside `run_panel_legs_and_quorum` and returns `Err` before `stream_response` is called.
  - Synthesize live arbiter establishment failure (pre-first-chunk): reuse only the **retry loop** (`RetryPolicy`, chat.rs:3428-3431) — the arbiter is a single model, so no fallback-provider logic applies. A permanent establishment failure propagates as `Err(ApiError)` → proper non-200, before any stream starts.
  - Failure *after* the first chunk: surfaces as the existing in-stream JSON error event (`sse.rs:620`); the row is marked `truncated`. No buffered-retry/degraded-leg fallback (cannot un-send chunks).
  - Hang safety: the streaming route cap is `STREAMING_TIMEOUT_SECS` (600s), not the 60s cap that the buffered path uses. The arbiter *establishment* call (`provider.chat_completion_stream(...)`) is **additionally** bounded by a per-call timeout (`arb_ctx.deadline` or 120s default), mirroring the buffered `arbitrate` path — a hang before the first byte surfaces as a non-200 before any stream starts. A mid-stream stall (after the first chunk) is shed only by the 600s route cap — acceptable because bytes have already been sent to the client. A timed-out or aborted stream drops the body → `DropGuard` fires → the row is written (marked `truncated`). Panel streams respect the same timeout contract as single-model streams.
- **D6 — No caching for panel streams.** The panel-aware `StreamLogContext` sets `cache_insert = None`, mirroring the non-streaming panel's cache bypass.

## 4. Architecture

```
request (stream:true + panel header)  ──  budget gate + creds already resolved in prepare() (chat.rs:2174/2698)
  └─ gate (chat.rs:2206): if stream && panel.is_some() → complete_panel_streaming  ← CHANGED (returns Result<Response,ApiError>)
       └─ complete_panel_streaming(state, ctx, prep, cfg):
            1. run_panel_legs_and_quorum(...)  ──  legs fan-out + join + quorum   (extracted from run_panel; non-stream path unchanged)
                 → (Vec<LegResult> leg_records, Option<f64> leg_cost_total)  |  Err(502) on quorum-unmet, before 200
            2. arbiter → (BoxStream, ArbiterCostPlan, ArbiterDetail):
                 • Synthesize: build synthesis req → chat_completion_stream(stream:true) → live BoxStream
                               ArbiterCostPlan::Live   (priced at stream-end from accumulated usage via ctx.pricing = arbiter pricing)
                 • BestOfN:    judge select (measured_single_dispatch, non-stream) → fake_stream_from_response(chosen.response)
                               ArbiterCostPlan::Known(Some(judge_cost))
                 • Majority:   embed+cluster (no dispatch) → fake_stream_from_response(medoid.response)
                               ArbiterCostPlan::Known(None)   (embeddings unmetered today)
            3. build StreamLogContext { ...arbiter-provider pricing/spend/writer..., cache_insert: None,
                                        panel: Some(PanelStreamLog { leg_records, leg_cost_total, strategy, quorum,
                                                                     arbiter_detail, arbiter_cost_plan, panel_leg_writer }) }
            4. Ok(sse::stream_response(arbiter_stream, arbiter_provider, trace_id, Some(ctx)))   ← served bumped here (sse.rs:1062)
                 • UsageTrackingStream accumulates the streamed answer's usage
                 • TrackedEventStream terminal: [include_usage chunk?] → tokentrimmer.panel → tokentrimmer.usage(AGGREGATE) → [DONE]
                 • DropGuard: arbiter_cost = plan.finalize(accumulated_usage, ctx.pricing);
                              total = none_aware_add(leg_cost_total, arbiter_cost);  cost_incomplete = any None;
                              write ONE request_logs row (provider='panel', cost=total, cached=false);
                              spawn panel_legs writes via panel_leg_writer; NO cache insert
```

Off-by-default holds: absent the header, `prep.panel.is_none()` and the request takes the single-model streaming path untouched (no panel field is read).

## 5. Components & seams

### 5.1 Routing (`chat.rs:2206`)
```
if prep.req.stream {
    if let Some(cfg) = prep.panel.take() {
        return complete_panel_streaming(&state, &ctx, prep, cfg).await;   // Result<Response, ApiError>, `?`-free: it's already a Response
    }
    return handle_streaming(&state, &ctx, prep).await;
}
// non-stream panel still flows through complete_once → complete_panel (unchanged)
```

### 5.2 `complete_panel_streaming` (new, `panel.rs`) — `async fn(...) -> Result<Response, ApiError>`
Mirrors `complete_panel` (`panel.rs:1367`) but for a streaming arbiter:
- Does **not** re-run the budget gate (already passed in `prepare()`).
- Calls `run_panel_legs_and_quorum(...)` (§5.3a) → `(leg_records, leg_cost_total)`; on quorum-unmet returns `Err(ApiError)` (`502`) **before** any stream — fail-closed.
- Builds the arbiter `BoxStream` + `ArbiterCostPlan` + `ArbiterDetail` via `arbitrate_streaming` (§5.3b).
- Captures `state.panel_leg_writer.clone()` and the arbiter provider's `pricing` into the context (the `DropGuard` has no `&AppState`).
- Constructs the panel-aware `StreamLogContext` (§5.5), then `Ok(sse::stream_response(arbiter_stream, &arbiter_provider, trace_id, Some(ctx)))`. `served` is bumped inside `stream_response` (`sse.rs:1062`) — do not bump it here.

### 5.3 Leg/quorum extraction + streaming arbiter (`panel.rs`)
**(a) Extract `run_panel_legs_and_quorum(state, ctx, base_req, creds, cfg, deadline) -> Result<(Vec<LegResult>, Option<f64>), ApiError>`** — phases 2–3 of today's `run_panel` (`panel.rs:1061–1173`: fan out member legs, join, quorum check) plus the member-leg cost sum, returned as `leg_cost_total`. `run_panel` is refactored to call this helper then do its arbiter step; `complete_panel` (non-streaming) is **byte-for-byte unchanged in behavior** (regression-gated by the existing panel suite). `complete_panel_streaming` calls the same helper, then branches to the streaming arbiter.

**(b) `ArbiterStrategy::arbitrate_streaming(...) -> Result<(BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ArbiterCostPlan, ArbiterDetail), ApiError>`** — a new trait method with a **default impl** that calls the existing buffered `arbitrate`, then wraps `outcome.response` in `fake_stream_from_response` and returns `ArbiterCostPlan::Known(outcome.cost_usd)` + `outcome.detail`. This covers `BestOfN`, `Majority`, and any future strategy for free. `Synthesize` **overrides** it: reuse its existing synthesis-prompt construction + arbiter provider/credential resolution (`panel.rs:505–555`) but dispatch `provider.chat_completion_stream(arbiter_req, &arb_ctx)` (with the retry loop, no fallback — §D5) and return the live `BoxStream` + `ArbiterCostPlan::Live` + detail.
- The default impl passes its stream to `stream_response` with **`Some(panel_ctx)`** (via §5.2), NOT `None`. (The L1-cache-hit path uses `None` and settles inline; panel replay must use `Some` so the `DropGuard` fires and writes the aggregate row.)

### 5.4 `ArbiterCostPlan` (new, `panel.rs`) — the no-double-count guard
```
enum ArbiterCostPlan { Live, Known(Option<f64>) }
impl ArbiterCostPlan {
    fn finalize(&self, accumulated: &Usage, arbiter_pricing: Option<&ModelPricing>) -> Option<f64> {
        match self {
            ArbiterCostPlan::Live      => arbiter_pricing.map(|p| compute_cost(accumulated, p)),  // fresh arbiter tokens
            ArbiterCostPlan::Known(c)  => *c,   // IGNORE `accumulated` — for replay it is the chosen leg's tokens, already in Σ legs
        }
    }
}
```
`Live` reuses the arbiter provider's `pricing` already captured in `StreamLogContext` (the arbiter *is* the streamed provider). The aggregate is `none_aware_add(leg_cost_total, finalize(...))`, matching the non-streaming `sum_metered_iter` fold (`panel.rs:1202–1208`): if either side is `None`, the total is the other side and `cost_incomplete = true`.

### 5.5 Panel-aware `StreamLogContext` (`sse.rs:382`) + `DropGuard` (`sse.rs:736`)
Add one field: `pub panel: Option<Arc<PanelStreamLog>>`, where
```
struct PanelStreamLog {
    leg_records: Vec<LegResult>, leg_cost_total: Option<f64>,
    strategy: ArbiterStrategyKind, quorum_required: usize, quorum_met: usize,
    arbiter_detail: ArbiterDetail, arbiter_cost_plan: ArbiterCostPlan,
    panel_leg_writer: Option<Arc<dyn PanelLegWriter>>,
}
```
`Arc` so the **single source of truth** is shared (not duplicated) between the `DropGuard` closure and `TrackedEventStream` (§5.6). When `panel` is `Some`, the `DropGuard`:
- `arbiter_cost = panel.arbiter_cost_plan.finalize(&accumulated, ctx.pricing.as_ref())`,
- `total = none_aware_add(panel.leg_cost_total, arbiter_cost)`; `cost_incomplete = panel.leg_cost_total.is_none() || arbiter_cost.is_none()`,
- `spend_sink.record(total)` + `settle(cached=false)` (existing sink, deferred from the synchronous non-streaming call),
- writes ONE `request_logs` row with `provider="panel"`, `model=arbiter`, `cost_usd=total`, `cached=false`,
- spawns `panel_legs` writes via `panel.panel_leg_writer` (reuse the Phase-2 `PanelLegWriter` path + `tracker`),
- records the Phase-2 `tokentrimmer.panel.*` span attributes,
- `cache_insert` is `None` so no L1/L2 insert runs.
When `panel` is `None`, the `DropGuard` is byte-for-byte its current single-model behavior (invariant 1).

### 5.6 `tokentrimmer.panel` terminal SSE event (`TrackedEventStream`, `sse.rs:484–606`)
`TrackedEventStream` gains `panel: Option<Arc<PanelStreamLog>>` (cloned `Arc` from `StreamLogContext` at construction `sse.rs:707`). In `EmitTerminal`, when `panel.is_some()`, build the `build_panel_body`-shaped object from `panel.leg_records` + `panel.arbiter_detail` + the arbiter cost via the **same** `arbiter_cost_plan.finalize(...)` (so the event and the row agree), and queue it **before** `usage_event()`. Final order: `[OpenAI usage chunk if include_usage] → tokentrimmer.panel → tokentrimmer.usage → [DONE]`. The `tokentrimmer.usage` event is made panel-aware: when `panel.is_some()`, its `cost_usd` is the **aggregate** `total`, not the arbiter-only cost. Serialization is fail-soft: if the panel object fails to serialize, drop only the metadata event, never the answer or the usage event (mirrors the non-streaming fallback at **`chat.rs:2269`**).

## 6. Invariants (targeted by tests)
1. **Off-by-default / wire-identical.** No panel header ⇒ byte-identical single-model streaming (the `panel` field is `None` everywhere; no new event bytes).
2. **One served, one row.** A streaming panel bumps `served` exactly once (`sse.rs:1062`) and writes exactly one `request_logs` row (`provider='panel'`, `cached=false`, `cost_usd = Σ legs + arbiter`).
3. **No double-count.** For BestOfN/Majority the replayed chosen-leg tokens are **not** repriced as arbiter cost (`ArbiterCostPlan::Known` ignores accumulated usage); aggregate = `Σ legs + judge|None`.
4. **Fail-closed before 200.** Over-budget / unpriceable / quorum-unmet ⇒ `402`/`502` with zero arbiter dispatch and zero rows — never a half-open stream.
5. **No cache.** Streaming panels never insert L1/L2.
6. **Uniform contract.** `stream:true` ⇒ SSE for all three strategies; the client sees only the arbiter answer + `tokentrimmer.panel` + `tokentrimmer.usage` + `[DONE]`, never member-leg tokens.
7. **Aggregate parity.** Streamed aggregate `cost_usd` == non-streaming `cost_usd` for the same inputs (exact for BestOfN/Majority replay; within arbiter nondeterminism for Synthesize).

## 7. Testing (TDD)
- **`ArbiterCostPlan::finalize` unit test:** `Known(Some(x)).finalize(any_usage, any_pricing) == Some(x)` (ignores usage); `Known(None) == None`; `Live` prices the usage. Directly guards invariant 3.
- **Synthesize live stream** (mock provider whose `chat_completion_stream` yields N chunks): client receives arbiter chunks in order → `tokentrimmer.panel` → `tokentrimmer.usage` (aggregate) → `[DONE]`; exactly one `request_logs` row, `provider='panel'`, `cost = Σ legs + arbiter`, `cached=false`; `served` bumped once.
- **BestOfN replay** (mock judge `"2\n…"`): streamed chunks reconstruct leg-2's verbatim answer; `tokentrimmer.panel.arbiter.chosen_leg == 2`; aggregate cost = `Σ legs + judge` (NOT `+ replayed-leg-tokens` — the no-double-count assertion).
- **Majority replay** (mock embedder, 3-cluster): streamed chunks == medoid leg's answer; `winning_cluster_size` correct; aggregate cost = `Σ legs` (arbiter None).
- **Fail-closed before 200**: over-budget ⇒ `402`, zero upstream dispatch, zero rows (call-counter); quorum-unmet ⇒ `502`, zero rows.
- **No cache**: after a streaming panel completes, assert no L1/L2 entry inserted.
- **Off-by-default regression**: existing single-model streaming tests unchanged + green; `stream:true` with no panel header ⇒ byte-identical event sequence.
- **Terminal ordering**: assert `[openai usage chunk if requested] → tokentrimmer.panel → tokentrimmer.usage → [DONE]`.
- **Unpriceable leg**: one unpriceable leg ⇒ `tokentrimmer.panel.cost_incomplete = true`, event still emitted.
- **Non-streaming regression**: the full existing panel suite passes unchanged after the `run_panel_legs_and_quorum` extraction.

## 8. Out of scope (later phases)
- `/v1/messages` + `/v1/responses` transcoder rendering of streamed panels (Phase 6).
- `CallerTier` entitlement gate + `RouteAction.panel` org config + gateway-API-reference docs + the agent-loop `record_request_served` unify (Phase 7).
- No change to quorum, budget-gate math, `panel_legs` schema, the cloud read-side, the three arbiter selection algorithms, or embedding metering (Majority stays `cost_usd: None`).

## 9. Self-review
- **Placeholders:** none — every seam cites a verified file:line; the `ArbiterCostPlan` and `PanelStreamLog` shapes are given concretely; the one plan-time choice (trait method vs. match — settled here as the default-impl trait method) is explicit.
- **Consistency:** one `BoxStream` type serves both live and replayed arbiters → one pipeline (`stream_response`). Billing/failure/no-cache each reuse the non-streaming panel behavior; the only new money-path logic is the deferred write boundary + the `ArbiterCostPlan` double-count guard (invariant 3), both explicitly tested.
- **Scope:** one subsystem (chat/sse/panel, public repo), one plan, no cloud change.
- **Ambiguity:** strategy coverage (D2), metadata delivery + ordering (D3/5.6), cost finalization (5.4), served accounting (§2 + invariant 2), failure posture (D5), and caching (D6) are each pinned to exactly one behavior, with rejected alternatives noted.
- **Review hardening:** the design review's three blockers — replay double-counting, the streaming `served` increment, and `DropGuard` lacking state for pricing/leg-writer — are resolved by §5.4 (Known ignores accumulated usage), §2 + §5.2 (served self-accounted at `sse.rs:1062`), and §5.5 (`panel_leg_writer` + arbiter pricing captured into the context), respectively.
