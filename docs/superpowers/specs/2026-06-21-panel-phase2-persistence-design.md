# Deep Research Panel — Phase 2 (Per-Leg Persistence + Dry-Run) Design Spec

> Status: **APPROVED ARCHITECTURE (locked in master spec), Phase-2 detail.** Date: 2026-06-21.
> Master spec: `2026-06-21-deep-research-panel-design.md` (§4.1 schema, §5 roadmap). Builds on Phase 1 (PR #220).

## 1. Goal

Persist per-leg panel detail to a `panel_legs` child table, emit per-leg telemetry, and add a `/v1/preview` dry-run estimate for panels — without changing the aggregate one-row billing established in Phase 1. Also clears two Phase-1 deferred follow-ups (M1 arbiter credentials, M3 `latency_ms`).

## 2. Scope (what Phase 2 adds)

1. **`panel_legs` migration** (`crates/core/migrations/0026_panel_legs.{up,down}.sql`).
2. **`PanelLegWriter`** persistence trait (tt-telemetry) + `InMemory` + `Postgres` impls, wired into `AppState`.
3. **Per-leg write** in `complete_panel`: after the parent `request_logs` row, write one `panel_legs` row per member leg + the arbiter leg, keyed by the parent row's `request_logs.id`.
4. **Per-leg telemetry**: additive `tokentrimmer.panel.*` span attributes (strategy, leg_count, quorum) on the existing request span. (Full per-leg child spans are out of scope — `panel_legs` + the response body are the per-leg record of truth.)
5. **`/v1/preview` panel dry-run**: when a panel header/`tt_extras.panel` is present, return a side-effect-free per-leg cost estimate (no dispatch) + whether it would pass a supplied budget.
6. **M1 (arbiter credentials)**: thread the arbiter provider's resolved credential into `Synthesize::arbitrate` so cross-provider arbiters work.
7. **M3 (`latency_ms`)**: populate the arbiter leg's real dispatch latency; skipped legs stay 0.

## 3. Key design decisions

### 3.1 `panel_legs` schema — NO enforced FK (match existing convention)

Migration `0001` deliberately omits DB-level foreign keys; the Phase-0 spike flagged `ON DELETE CASCADE` as a *departure needing sign-off*. **Decision: follow the no-FK convention** — `request_log_id` is a plain indexed `UUID` column, not a `REFERENCES`. This avoids the parent-must-commit-before-children ordering constraint on two independent fire-and-forget async writes, and matches the codebase. Retention/purge/export participation for `panel_legs` is an **additive cloud obligation deferred to Phase 3** (the spike's `retention.rs`/`account_purge.rs` lines), tracked explicitly there.

```sql
-- 0026_panel_legs.up.sql
CREATE TABLE panel_legs (
    request_log_id  UUID NOT NULL,            -- = request_logs.id (no enforced FK, per 0001 convention)
    leg_index       INT  NOT NULL,            -- 0..N-1 for member legs; arbiter uses a high sentinel or role
    role            TEXT NOT NULL,            -- 'leg' | 'arbiter'
    provider        TEXT NOT NULL,            -- per-leg provider (unblocks Phase-3 per-provider invoice recon)
    model           TEXT NOT NULL,
    input_tokens    BIGINT,
    output_tokens   BIGINT,
    cached_tokens   BIGINT,
    cost_usd        DOUBLE PRECISION,         -- per-leg cost; NULL = unmetered/unpriced (never coerced to 0)
    latency_ms      BIGINT,
    status          TEXT NOT NULL,            -- 'ok' | 'error' | 'timeout' | 'skipped_no_cred'
    error_class     TEXT,
    PRIMARY KEY (request_log_id, leg_index)
);
CREATE INDEX panel_legs_request_log_id_idx ON panel_legs (request_log_id);
```

`0026_panel_legs.down.sql`: `DROP TABLE IF EXISTS panel_legs;`

### 3.2 Parent id flows to children

The aggregate `request_logs` row's `id` (UUID v7) is minted in `complete_panel` when constructing the parent row. The same id is stamped onto every `panel_legs` row's `request_log_id`. Write the parent row first, then the leg rows (best-effort, retry-wrapped like the parent), in the same spawned task. No FK ⇒ no hard ordering requirement, but parent-first is preferred for read consistency.

### 3.3 `PanelLegWriter` mirrors `RequestLogWriter`

```rust
// tt-telemetry
pub struct PanelLegRow { /* the columns above */ }
#[async_trait] pub trait PanelLegWriter: Send + Sync {
    async fn write_legs(&self, rows: Vec<PanelLegRow>) -> Result<(), RequestLogError>;
}
pub struct InMemoryPanelLegWriter { /* Mutex<Vec<PanelLegRow>> + rows() */ }   // tests assert child rows
#[cfg(feature = "postgres")] pub struct PostgresPanelLegWriter { /* multi-row INSERT */ }
```
`AppState` gains `panel_leg_writer: Arc<dyn PanelLegWriter>` (default a no-op/in-memory). Builder `with_panel_leg_writer(..)`. `complete_panel` builds `Vec<PanelLegRow>` from the `PanelResult.legs` (now carrying real `latency_ms`) and the arbiter leg, and writes them once.

### 3.4 `/v1/preview` panel dry-run

`/v1/preview` is already side-effect-free. When the request carries a panel trigger (header or `tt_extras.panel`), return a `panel` estimate object: `{ strategy, members: [{model, provider, estimated_cost_usd}], arbiter: {...}, total_estimated_cost_usd, within_budget: Option<bool> }` computed via `estimate_panel_cost` — **no dispatch**. `within_budget` compares to `X-TokenTrimmer-Cost-Limit-Usd` if present. Unpriceable member ⇒ `estimated_cost_usd: null` + `total = null` + `within_budget: false` (mirrors the fail-closed gate).

### 3.5 served==rows + cached=false unchanged

Phase 2 adds child rows in a **separate table** — `tt_requests_served_total` and the parent `request_logs` count are untouched (invariant preserved). The parent panel row stays `cached=false`, `provider="panel"`, `cost_usd=aggregate`. No billing change.

## 4. Tasks (TDD)

- **T1 — migration**: `0026_panel_legs.{up,down}.sql` + a migration test (the crate's migration test harness applies it; assert the table exists / round-trips). 
- **T2 — `PanelLegRow` + `PanelLegWriter` trait + `InMemoryPanelLegWriter`** in tt-telemetry; unit test the in-memory writer.
- **T3 — `PostgresPanelLegWriter`** (feature-gated multi-row INSERT) + `AppState` wiring (`with_panel_leg_writer`, default in-memory/no-op).
- **T4 — write legs in `complete_panel`**: build `Vec<PanelLegRow>` from `PanelResult` + arbiter, stamp parent id, write once; integration test (InMemory writer) asserts N+1 child rows with correct role/provider/cost/status, and that the parent stays one row / served +1.
- **T5 — M3 latency + M1 arbiter creds**: `run_panel` records real per-leg `latency_ms` (already has `started.elapsed()`), populate the arbiter leg's latency; thread the arbiter provider credential from `panel_creds` into `Synthesize::arbitrate`. Test: cross-provider arbiter dispatches with its own creds; arbiter leg latency > 0.
- **T6 — `tokentrimmer.panel.*` span attrs** (strategy, leg_count, quorum_met/required) added only on the panel path; test via the gen_ai span-attr test pattern.
- **T7 — `/v1/preview` panel dry-run**: panel trigger on `/v1/preview` ⇒ panel estimate object, no dispatch (assert zero provider calls); unpriceable ⇒ null/within_budget=false.

## 5. Invariants preserved
Off-by-default (no panel ⇒ no `panel_legs` write, no new span attrs); aggregate one-row billing unchanged; `cached=false`; served==rows (child rows are a separate table); fail-closed dry-run estimate.

## 6. Out of scope (later phases)
Cloud read-side (`invoice_recon` reads `panel_legs`, per-leg dashboards, retention/purge inclusion) = Phase 3. best-of-n/majority = Phase 4. Streaming = Phase 5. Transcoders = Phase 6. Entitlement/docs = Phase 7.
