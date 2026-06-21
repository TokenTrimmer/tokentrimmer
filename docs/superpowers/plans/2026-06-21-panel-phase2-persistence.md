# Deep Research Panel — Phase 2 (Per-Leg Persistence + Dry-Run) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]` checkboxes.

**Goal:** Persist per-leg panel detail to a new `panel_legs` table, add per-leg telemetry + a `/v1/preview` panel dry-run, and clear the Phase-1 deferred follow-ups (arbiter credentials, real `latency_ms`) — all without changing Phase-1's aggregate one-row billing.

**Architecture:** A `panel_legs` child table (no enforced FK, per the `0001` convention), a `PanelLegWriter` trait mirroring the existing `RequestLogWriter`, written once from `complete_panel` keyed on the parent `request_logs.id`. Spec: `docs/superpowers/specs/2026-06-21-panel-phase2-persistence-design.md`.

**Tech stack:** Rust, sqlx (migrations + Postgres writer), tt-telemetry, tt-core.

## Global Constraints

- **No billing change.** The aggregate parent row stays exactly one `request_logs` row, `cached=false`, `provider="panel"`, `cost_usd=aggregate`. `tt_requests_served_total` still +1 per panel. `panel_legs` rows are a SEPARATE table and must NEVER inflate served==rows.
- **Off-by-default.** No panel ⇒ no `panel_legs` write, no new span attrs, no preview panel object. Non-panel paths byte-identical.
- **`panel_legs` has NO enforced FK** (match migration `0001`); `request_log_id` is an indexed `UUID`. PK `(request_log_id, leg_index)`.
- **Per-leg `cost_usd` is `Option`/NULL when unpriced** — never coerce to 0 (mirrors `MeasuredDispatch.cost_usd`).
- **CI hygiene:** never whole-crate `cargo fmt`; stage only touched files; verify `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test -p tt-core --no-run`. DB tests are `#[ignore]`+`TEST_DATABASE_URL`-gated (won't run in CI) — make the migration + writer logic unit-testable via the in-memory writer; gate Postgres behind `#[cfg(feature="postgres")]`.
- Builds are slow (minutes) — be patient.

---

### Task 1: `panel_legs` migration

**Files:** Create `crates/core/migrations/0026_panel_legs.up.sql` + `0026_panel_legs.down.sql`. Test: the crate's existing migration test (find it: `grep -rln "migrate\|MIGRATOR\|sqlx::migrate" crates/core` — likely `tests/migrations.rs`).

- [ ] **Step 1:** Read an existing recent migration pair (e.g. `0024_batch_jobs.up.sql`) to match SQL style/quoting.
- [ ] **Step 2:** Write `0026_panel_legs.up.sql` exactly per spec §3.1 (table + `panel_legs_request_log_id_idx`). Write `0026_panel_legs.down.sql` = `DROP TABLE IF EXISTS panel_legs;`.
- [ ] **Step 3:** If `tests/migrations.rs` exists and runs the migrator against a test DB (likely `#[ignore]`), confirm the new migration is picked up (sqlx embeds the dir). If migrations are compile-time-checked (`sqlx::migrate!`), `cargo build -p tt-core` validates the SQL parses. Run `cargo build -p tt-core`.
- [ ] **Step 4:** Commit: `feat(panel): 0026 panel_legs child table migration`.

---

### Task 2: `PanelLegRow` + `PanelLegWriter` trait + in-memory writer

**Files:** Modify `crates/telemetry/src/request_logs.rs` (or a sibling `panel_legs.rs` module in tt-telemetry — prefer a new module `crates/telemetry/src/panel_legs.rs` + `pub mod panel_legs;` in lib.rs for one-responsibility-per-file). Test: in-module `#[cfg(test)]`.

**Interfaces (Task 4 depends on these):**
```rust
pub struct PanelLegRow { pub request_log_id: Uuid, pub leg_index: i32, pub role: String, pub provider: String, pub model: String, pub input_tokens: Option<i64>, pub output_tokens: Option<i64>, pub cached_tokens: Option<i64>, pub cost_usd: Option<f64>, pub latency_ms: Option<i64>, pub status: String, pub error_class: Option<String> }
#[async_trait] pub trait PanelLegWriter: Send + Sync { async fn write_legs(&self, rows: Vec<PanelLegRow>) -> Result<(), RequestLogError>; }
pub struct InMemoryPanelLegWriter { /* Mutex<Vec<PanelLegRow>> */ }
impl InMemoryPanelLegWriter { pub fn new() -> Self; pub fn rows(&self) -> Vec<PanelLegRow>; }
pub struct NoopPanelLegWriter; // default; drops rows
```
- [ ] **Step 1 (RED):** unit test: `InMemoryPanelLegWriter::new()`, `write_legs(vec![row1,row2]).await`, assert `rows().len()==2` and a field round-trips. Run → fail.
- [ ] **Step 2 (GREEN):** implement `PanelLegRow` (mirror `RequestLogRow` derives — `Debug, Clone`), the trait, `InMemoryPanelLegWriter` (mirror `InMemoryRequestLogWriter` at request_logs.rs:319-345), and `NoopPanelLegWriter`. Reuse `RequestLogError`.
- [ ] **Step 3:** `cargo test -p tt-telemetry` green; clippy clean.
- [ ] **Step 4:** Commit: `feat(panel): PanelLegRow + PanelLegWriter trait + in-memory writer`.

---

### Task 3: `PostgresPanelLegWriter` + `AppState` wiring

**Files:** Modify `crates/telemetry/src/panel_legs.rs` (Postgres impl, `#[cfg(feature="postgres")]`), `crates/core/src/state.rs` (AppState field + builder).

**Interfaces:** `AppState.panel_leg_writer: Arc<dyn PanelLegWriter>` (default `Arc::new(NoopPanelLegWriter)`); `AppState::with_panel_leg_writer(self, Arc<dyn PanelLegWriter>) -> Self`.
- [ ] **Step 1:** Implement `PostgresPanelLegWriter` mirroring `PostgresRequestLogWriter::write` (request_logs.rs:421-525) — a single multi-row INSERT into `panel_legs` (build a `QueryBuilder` or batched bind). Match the bind-count-guard test pattern if one exists for request_logs.
- [ ] **Step 2:** Wire `AppState`: add field defaulting to `NoopPanelLegWriter`, add `with_panel_leg_writer`. Confirm the non-panel path is unaffected (the writer is only called from `complete_panel`).
- [ ] **Step 3:** `cargo build -p tt-core --features postgres` (if that's how it's built) + default build clean; clippy clean.
- [ ] **Step 4:** Commit: `feat(panel): PostgresPanelLegWriter + AppState wiring`.

---

### Task 4: Write legs from `complete_panel`

**Files:** Modify `crates/core/src/routes/panel.rs` (`complete_panel`), `crates/core/src/routes/chat.rs` if the parent row id needs surfacing. Test: `crates/core/tests/panel_legs_persist.rs` (new), using `InMemoryRequestLogWriter` + `InMemoryPanelLegWriter`.

**Behavior:** in `complete_panel`, after constructing the aggregate parent `request_logs` row (and knowing its `id`), build a `Vec<PanelLegRow>` from `PanelResult.legs` (member legs) + the arbiter leg — each carrying `request_log_id = parent.id`, `leg_index`, `role`, `provider`, `model`, token counts (from `LegResult.usage`), `cost_usd` (`LegResult.cost_usd`, Option), `latency_ms`, `status` (`LegStatus::as_str`), `error_class` (None for now). Write them once via `state.panel_leg_writer.write_legs(...)` in the same spawned task as the parent row write (parent first).
- [ ] **Step 1 (RED):** integration test: panel-enabled router with InMemory writers; a happy 2-member panel ⇒ after `drain`, `request_log_writer.rows().len()==1` AND `panel_leg_writer.rows().len()==3` (2 legs + arbiter), with roles `['leg','leg','arbiter']`, the parent `id` matching all child `request_log_id`s, and per-leg `provider`/`cost_usd` populated. Run → fail.
- [ ] **Step 2 (GREEN):** implement the leg-row build + write in `complete_panel`. Mint/obtain the parent row id once; stamp it on the children.
- [ ] **Step 3:** test green; the existing panel suite still green (`cargo test -p tt-core --test panel_dispatch --test panel_engine`); clippy clean.
- [ ] **Step 4:** Commit: `feat(panel): persist per-leg panel_legs rows from complete_panel`.

---

### Task 5: Real `latency_ms` + arbiter credentials (clears Phase-1 M1, M3)

**Files:** Modify `crates/core/src/routes/panel.rs` (`run_panel`, `Synthesize::arbitrate`, `ArbiterStrategy` if the signature must carry creds). Test: extend `panel_fanout.rs`.

**Behavior:**
- M3: the arbiter leg currently has `latency_ms: 0`. Measure the arbiter dispatch (`Instant::now()` around `measured_single_dispatch`) and set the arbiter leg's `latency_ms`. (Member legs already record real latency.)
- M1: `Synthesize::arbitrate` currently dispatches with `ctx` directly; a cross-provider arbiter then uses the wrong credentials. Thread the arbiter provider's credential (resolved into `panel_creds` in `prepare`) into the arbiter dispatch — pass the `creds` map (or the arbiter's resolved `ProviderCredentials`) into `arbitrate` and substitute it into the arbiter `ctx` like member legs do.
- [ ] **Step 1 (RED):** test: a panel whose arbiter is on a DIFFERENT mock provider than the members + source bearer ⇒ arbiter dispatches successfully (its mock counter increments) and the arbiter leg's `latency_ms > 0`. Run → fail (today the arbiter would fail closed on missing creds).
- [ ] **Step 2 (GREEN):** thread arbiter creds + measure arbiter latency. If the `ArbiterStrategy::arbitrate` signature must change to carry creds, update the trait + `Synthesize` + `run_panel` call site consistently.
- [ ] **Step 3:** test green; full panel suite green; clippy clean.
- [ ] **Step 4:** Commit: `fix(panel): arbiter cross-provider credentials + real arbiter latency`.

---

### Task 6: `tokentrimmer.panel.*` span attributes

**Files:** Modify `crates/core/src/routes/panel.rs` (or `crates/telemetry/src/gen_ai.rs` if attrs are recorded there). Test: mirror `crates/core/tests/gen_ai_span_attrs.rs`.

**Behavior:** on the panel path only, record additive span attrs: `tokentrimmer.panel.strategy`, `tokentrimmer.panel.leg_count`, `tokentrimmer.panel.quorum_required`, `tokentrimmer.panel.quorum_met`. Follow the existing `tokentrimmer.shadow_model` additive-attr precedent (`gen_ai.rs:77-85`) — set only when a panel ran; absent otherwise.
- [ ] **Step 1 (RED):** test (mirroring gen_ai_span_attrs.rs) capturing the request span for a panel request asserts the 4 attrs present with correct values; a non-panel request has none. Run → fail.
- [ ] **Step 2 (GREEN):** record the attrs in `complete_panel`.
- [ ] **Step 3:** test green; clippy clean.
- [ ] **Step 4:** Commit: `feat(panel): tokentrimmer.panel.* span attributes`.

---

### Task 7: `/v1/preview` panel dry-run

**Files:** Modify `crates/core/src/routes/preview.rs` + `crates/preview/src/lib.rs` (+ types). Test: `crates/core/tests/panel_preview.rs` (new).

**Behavior:** when `/v1/preview` receives a panel trigger (`panel_from_header` or `tt_extras.panel`), return a `panel` estimate object: `{ strategy, members: [{model, provider, estimated_cost_usd: Option}], arbiter: {model, provider, estimated_cost_usd: Option}, total_estimated_cost_usd: Option, within_budget: Option<bool> }` computed via `estimate_panel_cost` / per-member `estimate_cost_usd × fee_multiplier`. NO dispatch. Unpriceable member ⇒ that member's `estimated_cost_usd: null`, `total: null`, `within_budget: false`. `within_budget` compares total to `X-TokenTrimmer-Cost-Limit-Usd` when present (else null).
- [ ] **Step 1 (RED):** test: `/v1/preview` with `X-TokenTrimmer-Panel: synthesize` + 2 members ⇒ 200 with a `panel` object: members length 2 + arbiter + numeric total; assert ZERO provider dispatch calls (atomic counter). A second test: a member with a bogus model ⇒ `total` null + `within_budget` false. Run → fail.
- [ ] **Step 2 (GREEN):** implement the preview panel branch (reuse `estimate_panel_cost`/`PanelConfig::resolve`).
- [ ] **Step 3:** test green; existing preview tests still green; clippy clean.
- [ ] **Step 4:** Commit: `feat(panel): /v1/preview panel dry-run estimate (no dispatch)`.

---

## Self-Review
- Spec coverage: §2.1→T1, §3.3→T2/T3, §2.3/§3.2→T4, M3+M1→T5, §2.4→T6, §2.5/§3.4→T7. All covered.
- No billing change in any task (T4 writes a separate table; parent row untouched).
- No placeholders; each task has a concrete TDD test intent + interfaces. Exact signatures (preview route shape, gen_ai recording site, sqlx INSERT) are cited seams for the implementer to read.
- Type consistency: `PanelLegRow`/`PanelLegWriter`/`InMemoryPanelLegWriter`/`with_panel_leg_writer` consistent across T2–T4.
