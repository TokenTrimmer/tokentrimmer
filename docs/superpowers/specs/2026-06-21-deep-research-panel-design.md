# Deep Research Panel — Design Spec

> Status: **APPROVED ARCHITECTURE, build in progress.** Date: 2026-06-21.
> Companion findings doc (read-only investigation): `../../../../docs/deep-research-integration-findings.md` (top-level TokenTrimmer tree).
> This spec records the locked architecture, the Phase-0 cloud-billing spike results, the open product decisions (with defaults), the phase roadmap, and the detailed Phase-1 design. Each subsequent phase gets its own spec → plan → PR.

## 1. Goal

Add an **optional, opt-in, expensive "deep research" PANEL** to the TokenTrimmer gateway: one HTTP request fans out to **N model legs in parallel**, then an **arbiter** collapses the N answers into one. The panel must **never fire by surprise** — cost-optimal single-model routing stays the default. We want a dry-run/estimate mode, per-leg cost attribution, retrieve-once-share-across-legs, and pluggable arbiter strategies (synthesize / best-of-n / majority).

## 2. Locked architecture (the four forks, decided)

| Fork | Decision | Consequence |
|---|---|---|
| **Host** | **Chat-handler mode** — fan out inside `complete_once` on `/v1/chat/completions` (`public/crates/core/src/routes/chat.rs`), generalizing the existing 2-arm shadow `tokio::join!` (`chat.rs:1304-1321`) to N legs + arbiter. | Retrieval middleware already substitutes the body once before the handler (`retrieval.rs:147-158`); every leg inherits that context for free. Auto-exposes on `/v1/messages` + `/v1/responses` (transcoder rendering deferred to Phase 6). |
| **Billing / persistence** | **Aggregate** — legs via `measured_single_dispatch`; all leg + arbiter cost **sums into one `cost_usd`** / one `request_logs` row → one billable request; per-leg detail in a new `panel_legs` child table. | Confirmed by Phase-0 spike: **zero cloud billing-code change** needed. See §4. |
| **Failure policy** | **Quorum-based** — proceed and arbitrate over survivors iff quorum met (synthesize/best-of-n: ≥1; majority: > ⌊N/2⌋); else error and bill only what dispatched. Deadline-aborted-but-billed legs are attributed honestly. | See §6.4. |
| **Opt-in surface** | **`X-TokenTrimmer-Panel` header primary** + `tt_extras.panel` for rich config + optional `RouteAction.panel` org config; budget via existing `X-TokenTrimmer-Cost-Limit-Usd`; `CallerTier` entitlement gate. | Header threads uniformly across all three ingresses (`tt_extras` no-ops on `/v1/responses` today — `responses.rs:177`). |

### 2.1 Non-negotiable invariants (every phase preserves these)

1. **Off-by-default, never by surprise.** No panel header and no enabling route config ⇒ the request is **wire-identical** to today's single-model path. Enforced by a golden/snapshot test, not a comment.
2. **Fail-closed budget gate.** A panel dispatches **only if** the trigger is present **and** an explicit budget (`X-TokenTrimmer-Cost-Limit-Usd` or route `max_cost_usd`) covers a **summed-over-(N legs + arbiter), fee-aware** estimate. Any unpriceable leg ⇒ **402 before any upstream call**.
3. **Aggregate billing = one request.** Legs via `measured_single_dispatch`; leg + arbiter cost sums into the single `cost_usd` / one `request_logs` row; the panel counts as **one** request against caps/rpm and **one** invoice line.
4. **`served == rows` integrity.** The panel calls `record_request_served` **exactly once** and writes **exactly one** `request_logs` row, regardless of N. Child `panel_legs` rows live in a separate table and never inflate `tt_requests_served_total` (`metrics.rs:50-67`).
5. **`cached = false` on every panel row.** Overage `COUNT(*) … WHERE NOT cached` (`overage.rs:243-250`) and the gateway month-request accumulator (`budget.rs:289-333`) both skip cached rows; a `cached=true` panel row would silently escape billing. Panels bypass L1/L2 + single-flight (non-deterministic; §6.5).
6. **Retrieve-once is inherited, not re-run.** Legs fan out inside `complete_once` from the already-substituted `req.messages`; legs never re-enter retrieval.

## 3. Open product decisions (defaults taken; flagged for review)

These two were surfaced by the Phase-0 spike and are **business/product** calls, not implementability blockers. Defaults are taken to keep momentum under the "drive autonomously" directive; **override either by editing this section.**

- **(A) Single- vs multi-provider panels → DEFAULT: MULTI-PROVIDER.** A deep-research panel's value is answer diversity across *different* model families, so multi-provider is the product. The `panel_legs` schema therefore carries a **per-leg `provider`** column. Consequence: `invoice_recon` sums `cost_usd` **per provider** from the single `request_logs.provider` column (`invoice_recon.rs:207-219`); a multi-provider panel folds all leg costs under one stamped provider, mis-splitting the *per-provider* invoice attribution (org totals + global metered sums stay correct). **Resolution: Phase 3 teaches `invoice_recon` to read `panel_legs` for provider-accurate splitting (additive cloud change).** Until Phase 3, the parent row is stamped `provider = 'panel'` (sentinel) so it is excluded from per-provider drift checks rather than silently misattributed.
- **(B) Overage revenue for panels → DEFAULT: DEFER pricing to Phase 3; ship honest cost accounting now.** Metered overage is **count-based, flat-per-request** and ignores `cost_usd` (`overage.rs:243-250`, `85-91`), so an expensive panel bills the same overage unit as a trivial request. Cost is still captured honestly via spend-caps + invoice reconciliation (cost-based). v1 keeps the summed `cost_usd` honest and gates panels behind **entitlement (CallerTier) + per-request budget**, so the undercharge is bounded and opt-in. Whether panels need a cost-multiplier overage unit is a Phase-3 business decision.

## 4. Phase-0 spike results (cloud-billing) — load-bearing facts

Full synthesis archived in the workflow transcript; the facts the spec depends on:

- **Shared database.** Gateway (public) and cloud (private) share **one** Neon Postgres; the gateway's `PostgresRequestLogWriter` INSERTs the 39-column `request_logs` row directly (`request_logs.rs:379-418`, `INSERT_BIND_COUNT=39`), cloud reads the same table. No HTTP/queue ingest. ⇒ `panel_legs` lives in that shared DB, gateway-owned (a **public**-repo migration).
- **Billing basis is a mix, all AGGREGATE-compatible:** overage = `COUNT(*) WHERE NOT cached` (count-based, flat rate); spend caps + `invoice_recon` + budget alerts = `SUM(cost_usd)` (cost-based). A 1-row summed-cost panel is correct on every dimension **given the three gates** in §2.1 (4,5 + `cost_usd = SUM`).
- **Caps count per HTTP request, not per upstream call** (`auth.rs:259-351` pre-flight `check_with_limits_keyed`; `chat.rs:1622-1640` settle/record). One panel = one against rpm / monthly_request_cap / monthly_served_cap / spend cap **iff fan-out is internal to one gateway request** (it is). Per-key caps are enforced on the gateway path; cloud `key_budget_caps.rs` is config storage + mirror (no cloud change).
- **`panel_legs` FK target = `request_logs.id` (UUID v7)**, minted in-process at `chat.rs:1789-1790` *before* the async parent write, so panel code can mint the id, write N child rows, then write the parent. Not `trace_id` (nullable/non-unique).
- **`shadow_cost_usd` precedent** (`request_logs.rs:72-85`): "one request, multiple dispatches" with the extra dispatch's cost in its own column. `panel_legs` generalizes to N but **inverts the fold** — it SUMS leg+arbiter into the parent `cost_usd` (one billable request), per-leg detail in the child table for audit.
- **Minimum cloud change for correct billing: NONE.** Per-leg display (logs_admin / export_admin / reports_admin) and per-provider invoice recon are **additive** (Phase 3).
- **Additive cloud obligations** for the child table: retention/anonymization (`retention.rs:286-331`), account purge/export (`account_purge.rs:21-58`) must include `panel_legs` — handled by `ON DELETE CASCADE` (recommended) or explicit lines (Phase 3 verifies).

### 4.1 `panel_legs` schema (Phase-2 migration; defined here for reference)

```sql
CREATE TABLE panel_legs (
    request_log_id   UUID NOT NULL REFERENCES request_logs(id) ON DELETE CASCADE,
    leg_index        INT  NOT NULL,                 -- 0..N-1 for legs
    role             TEXT NOT NULL,                 -- 'leg' | 'arbiter'
    provider         TEXT NOT NULL,                 -- per-leg provider (unblocks multi-provider recon)
    model            TEXT NOT NULL,
    input_tokens     BIGINT,
    output_tokens    BIGINT,
    cached_tokens    BIGINT,
    cost_usd         DOUBLE PRECISION NOT NULL,     -- per-leg cost; parent cost_usd = SUM(these)
    latency_ms       BIGINT,
    status           TEXT NOT NULL,                 -- 'ok' | 'error' | 'timeout' | 'skipped_no_cred'
    error_class      TEXT,
    PRIMARY KEY (request_log_id, leg_index)
);
```

`ON DELETE CASCADE` **departs** from migration `0001`'s deliberate no-FK convention; chosen for clean purge/retention. Flagged for sign-off in the Phase-2 PR.

## 5. Phase roadmap

| Phase | Title | Repo | Depends on | Deliverable |
|---|---|---|---|---|
| **0** | Cloud-billing spike (read-only) | — | — | ✅ DONE — this spec §4 |
| **1** | Core fan-out engine | public | 0 | Header trigger → `complete_panel` (JoinSet of `measured_single_dispatch` + quorum) → `ArbiterStrategy::Synthesize`; fail-closed budget gate; aggregate cost into `cost_usd`; per-leg breakdown in **response body**; cache/single-flight bypass; `panel_legs_total` metric; `AppState` builder + env kill-switch. Off unless header present. **TDD.** |
| **2** | Per-leg persistence + telemetry + dry-run | public | 0,1 | `panel_legs` migration + idempotent child writer; `gen_ai` per-leg span attrs; `/v1/preview` dry-run estimate mode (no dispatch) |
| **3** | Cloud read-side | cloud | 0,2 | `invoice_recon` reads `panel_legs` for per-provider truth (decision A); per-leg display in logs/export/reports; retention/purge/export include `panel_legs`; panel overage-pricing decision (B) |
| **4** | Arbiter strategies | public | 1 | `best-of-n` (ranking) + `majority` (semantic clustering via the existing L2 embedder) |
| **5** | Streaming panel UX | public | 1 | Legs non-streaming; arbiter streams token-by-token to client |
| **6** | Transcoder rendering | public | 1 | Render panel + per-leg attribution on `/v1/messages` + `/v1/responses`; fix `/v1/responses` `tt_extras` passthrough; verify `/v1/messages` |
| **7** | Entitlement + rollout + docs | public(+cloud) | 1,3 | `CallerTier` entitlement gate; `RouteAction.panel` org config; `04-gateway-api-reference` docs; kill-switch ops runbook |

## 6. Phase 1 — detailed design (the first build)

**Scope:** the thinnest end-to-end panel that proves the architecture, fully behind the header gate. Non-streaming, `synthesize` strategy only, legs+arbiter cost aggregated into the existing single `cost_usd`/`request_logs` row, per-leg detail returned in the **response body only** (no DB schema change yet — that's Phase 2). Multi-provider capable.

### 6.1 New module: `public/crates/core/src/routes/panel.rs`

Owns the fan-out. Public surface within the crate:

```rust
pub(crate) async fn complete_panel(
    state: &AppState,
    ctx: &RequestContext,
    prep: Prepared,
    cfg: PanelConfig,
) -> Result<CompletionOutcome, ApiError>;
```

Returns the same `CompletionOutcome::Dispatched { response, headers }` the non-stream arm already assembles, so the handler tail (`chat.rs:2176-2233`) and header emission are reused unchanged.

### 6.2 New types

- **`PanelConfig`** `{ strategy: ArbiterStrategyKind, members: Vec<ModelRef>, arbiter_model: ModelRef, quorum: Option<usize>, max_cost_usd: Option<f64> }`. Parsed from the header (+ `tt_extras.panel` for richer fields); `members`/`arbiter_model` default from config when the header gives only a strategy.
- **`ModelRef`** `{ model: String, provider: Option<String> }` — resolved per leg via the existing `registry.resolve(model)` (`registry.rs:70-74`).
- **`LegResult`** `{ leg_index, role, model, provider, outcome: LegOutcome, cost_usd: Option<f64>, usage: Option<Usage>, latency_ms }` where `LegOutcome = Ok(ChatCompletionResponse) | Err(LegError{class})`. The per-leg attribution unit (returned in the body in Phase 1; persisted to `panel_legs` in Phase 2).
- **`ArbiterStrategyKind`** enum `{ Synthesize, BestOfN, Majority }` (only `Synthesize` implemented in Phase 1; the others return `ApiError::NotImplemented` so the wire shape is stable).

### 6.3 Arbiter seam

```rust
trait ArbiterStrategy {
    async fn arbitrate(
        &self,
        request: &ChatCompletionRequest,
        legs: &[LegResult],
        state: &AppState,
        ctx: &RequestContext,
    ) -> Result<ArbiterOutcome, ApiError>; // ArbiterOutcome { response, cost_usd: Option<f64> }
}
```

`Synthesize`: build an arbiter prompt over the surviving leg answers (template adapted from `agent_run.rs::build_summary_request:1411-1434`) and run **one** `measured_single_dispatch` on `cfg.arbiter_model`. Cost attaches via the same per-leg mechanism.

### 6.4 Data flow (non-streaming)

1. **Parse trigger** — `panel_from_header(&headers)` (new helper in the cluster at `chat.rs:76-133`) → `Option<PanelTrigger>`; merged with `tt_extras.panel` into `PanelConfig` inside `prepare`. Absent ⇒ untouched single-model path.
2. **Entitlement** — if `CallerTier` below the panel minimum, `ApiError::Forbidden` (Phase 7 finalizes the tier; Phase 1 default-allows with a TODO note, since `CallerTier` is fail-open-to-Free today).
3. **Budget gate (fail-closed)** — `estimate_panel_cost(cfg, input_tokens)` sums `estimate_cost_usd` per member + arbiter, each × `fee_multiplier`; **any member with no catalog pricing ⇒ 402** (`ApiError::CostLimitExceeded`). If no explicit budget present ⇒ 402. Runs **before any dispatch** at the existing gate site (`chat.rs:2577-2586`).
4. **Resolve legs** — for each member, `registry.resolve` + per-provider credential resolution (reuse the failover pre-resolution pattern at `chat.rs:2934-2975`); a member with no org credential is recorded as `status='skipped_no_cred'`, not dispatched.
5. **Fan out** — `JoinSet` of `measured_single_dispatch(provider, req.clone(), ctx, deadline)` over resolved legs. Per-leg retry capped (no `with_retry` amplification; one shot per leg, like the shadow path). `req.clone()` per leg (trait takes `req` by value).
6. **Quorum** — count `Ok` legs. Synthesize needs ≥1; else `ApiError::PanelQuorumUnmet` (502-class). Cancelled-but-billed legs are still recorded with their cost.
7. **Arbitrate** — `strategy.arbitrate(...)` over survivors → `ArbiterOutcome`.
8. **Aggregate cost** — `cost_usd = sum_metered(legs.cost_usd) + arbiter.cost_usd` using the `Option<f64>`/`sum_metered` convention (`agent_run.rs:330-340`); any unpriced surviving leg makes the recorded total an honest lower bound flagged in the body.
9. **Assemble response** — arbiter's `ChatCompletionResponse` as the answer; inject `tokentrimmer.panel` into the response body: `{ strategy, legs: [{leg_index, role, model, provider, cost_usd, tokens, status}], arbiter: {...}, total_cost_usd, quorum: {required, met} }`. Single aggregate `x-tokentrimmer-cost-usd` header = the sum.
10. **Record once** — exactly one `record_request_served` + one `request_logs` row written `cached=false` with the aggregate `cost_usd`; provider stamped `'panel'` (decision A sentinel). One settle/record.

### 6.5 Bypass / safety

- Panels **bypass** L1/L2 cache + single-flight (non-deterministic; two same-model legs must not coalesce). Branch before the cache-hit checks in `complete_once`.
- **Kill-switch**: `AppState::with_panel_enabled(bool)` + `TT_PANEL_ENABLED` env (default off in prod until rollout). When disabled, a panel header returns `ApiError::PanelDisabled` (not a silent fallback — explicit, so callers aren't surprised by single-model billing on a panel request).
- **Timeouts**: panel bounded by the per-route `TimeoutLayer` (60s short). Per-leg `deadline` derived from the request budget; the arbiter gets its own deadline. Document that synthesize latency = slowest surviving leg + arbiter.

### 6.6 Metrics

New bounded counter `tt_panel_legs_total{role,status}` and `tt_panel_requests_total{strategy,outcome}` in `metrics.rs`. **`tt_requests_served_total` still increments once per panel** (invariant 4).

### 6.7 Files touched (Phase 1)

- `public/crates/core/src/routes/panel.rs` — **new**: `complete_panel`, `PanelConfig`, `LegResult`, `ArbiterStrategy` + `Synthesize`, `estimate_panel_cost`.
- `public/crates/core/src/routes/chat.rs` — `panel_from_header` helper (`76-133`); read trigger into a local (`2012-2017`); panel-aware fail-closed budget gate (`2577-2586`); branch to `complete_panel` in `complete_once` before cache checks (`1256-1321`); aggregate-cost recording + `cached=false` (`1629-1636`); body injection in the dispatched arm.
- `public/crates/core/src/measurement.rs` — widen `measured_single_dispatch`/`MeasuredDispatch` visibility to `pub(crate)` reachable from `routes::panel` (already `pub(crate)`; confirm path).
- `public/crates/shared/src/messages.rs` — `tt_extras.panel` parsing (mirror `parse_cache_control:54-70`).
- `public/crates/core/src/metrics.rs` — new panel counters.
- `public/crates/core/src/server.rs` / `state.rs` — `with_panel_enabled` builder + `TT_PANEL_ENABLED` env.
- `public/crates/core/src/error.rs` — `PanelQuorumUnmet`, `PanelDisabled` variants + status mapping.

### 6.8 Testing (TDD — write these first)

1. **Off-by-default golden:** a normal request (no panel header) is byte-identical to current single-model output (snapshot). *No header ⇒ no behavior change.*
2. **Happy path:** `X-TokenTrimmer-Panel: synthesize` + sufficient `X-TokenTrimmer-Cost-Limit-Usd` + 2 members ⇒ one arbitrated answer; body carries `panel.legs` (len 2) + `arbiter`; `x-tokentrimmer-cost-usd` == sum.
3. **Fail-closed budget:** over-budget **or** any unpriceable member ⇒ **402 before any provider call** (assert zero upstream dispatches via a mock provider call-counter).
4. **Quorum unmet:** all members error ⇒ `PanelQuorumUnmet`, bills only what dispatched, one `request_logs` row.
5. **served==rows:** a panel request increments `tt_requests_served_total` by exactly 1 and writes exactly 1 row; `tt_panel_legs_total` increments by leg count.
6. **Kill-switch:** `TT_PANEL_ENABLED=false` + panel header ⇒ `PanelDisabled` (no dispatch).
7. **Multi-provider:** members spanning two providers both dispatch; aggregate cost sums across providers; parent row stamped `provider='panel'`.

**Acceptance:** all seven green; `cargo clippy --workspace --all-targets` clean on touched crates; `cargo test --workspace --no-run` compiles all targets (per CI gotcha — field-ripple changes must compile test targets).

## 7. Self-review notes

- **Placeholders:** none — `panel_legs` schema, all type shapes, file:line touch-points, and tests are concrete. Phase 1 deliberately defers the migration to Phase 2 and the cloud read-side to Phase 3; those are scoped, not vague.
- **Consistency:** invariants (§2.1) ↔ Phase-0 gates (§4) ↔ Phase-1 data flow steps 8/10 are aligned (one row, `cached=false`, `cost_usd = SUM`, served once).
- **Scope:** Phase 1 is one mergeable PR; later phases are independently specced. Decisions A/B are explicit, not hidden.
- **Ambiguity:** quorum defaults, the `provider='panel'` sentinel, and the kill-switch's explicit-error (not silent-fallback) behavior are each pinned to one interpretation.
