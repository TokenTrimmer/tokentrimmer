# Deep Research Panel — Phase 7 (Entitlement + Rollout + Docs) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Finalize the campaign's public scope — a configurable `CallerTier` entitlement gate, the agent-loop `record_request_served` fix, and the panel API-reference docs + kill-switch runbook.

**Architecture:** The entitlement gate mirrors the existing `panel_enabled` kill-switch pattern exactly (`TT_PANEL_MIN_TIER` parsed once at AppState build → `AppState.panel_min_tier` + `with_panel_min_tier` builder), checked in `prepare`'s panel-resolution block in the order kill-switch → entitlement → budget. Default `Free` = allow-all (no-op until configured). The served fix is a one-liner. Docs are additive. No billing/engine/transcoder/cloud change.

**Tech Stack:** Rust, `crates/core` (tt-core) + `crates/shared`. Branch `feat/panel-phase7-entitlement` (created; spec committed). Spec: `docs/superpowers/specs/2026-06-23-panel-phase7-entitlement-design.md`.

## Global Constraints
- **Off-by-default / default allow-all:** with `panel_min_tier` unset (= `Free`), the entitlement check passes for everyone — existing panel suites stay green. A non-panel request never reaches the check (it's inside the `panel_from_header` block).
- **No money-path / engine / transcoder / cloud change.** `RouteAction.panel` org config is deferred (out of scope).
- **CI gates (verify locally before each commit):** `cargo fmt --` on changed files + `cargo fmt --check` clean; `cargo clippy -p tt-core --lib --tests` no new warnings; the task's tests + relevant existing suites green. Do NOT use `--all-targets`. Never whole-crate `cargo fmt`. Full local `--lib --tests` stalls on this macOS box (dyld) — CI is authoritative; run targeted `--test <file>` locally.

---

### Task 1: Agent-loop `record_request_served` unify

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` (the `CompletionOutcome::Dispatched` arm, ~854-868)
- Modify: `crates/core/src/metrics.rs` (the `record_request_served` doc comment, ~61-64)
- Test: `crates/core/tests/agent_loop.rs` (or wherever the agent-run loop tests live — find by grepping for `run_loop_core` / `create_run` in `crates/core/tests/`)

**Interfaces:**
- Produces: each dispatched agent-loop turn bumps `tt_requests_served_total{path="agent_run",result="dispatch"}` once.

- [ ] **Step 1: Write the failing test**

Find the agent-run loop test harness (grep `crates/core/tests/` for `create_run`/`run_loop`/`GatewayCompleter`). Add a test that drives an N-turn agent run (e.g. 2 turns via a mock tool loop) and asserts `tt_requests_served_total{path="agent_run",result="dispatch"}` incremented exactly N times. Use the metrics-assertion pattern already in the repo (grep tests for `tt_requests_served_total` or a `metrics_util`/recorder snapshot helper). If no in-test metric recorder is wired, make the gate a focused assertion on the call site instead (and note that in the report) — but prefer the metric assertion if the harness supports it.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --test <agent_loop_test_file> <test_name>`
Expected: FAIL — counter is 0 (the Dispatched arm bumps nothing).

- [ ] **Step 3: Implement**

In `agent_run.rs`, inside the `CompletionOutcome::Dispatched { response, headers } => {` arm (~854), after the `usage`/`msg` are extracted and before `Ok((msg, usage))`:
```rust
crate::metrics::record_request_served("agent_run", "dispatch");
```
In `metrics.rs`, update the doc comment (~61-62) from `` `path` ∈ `chat|sse|embeddings` `` to `` `path` ∈ `chat|sse|embeddings|agent_run` ``.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-core --test <agent_loop_test_file> <test_name>`
Expected: PASS — counter == N.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt -- crates/core/src/routes/agent_run.rs crates/core/src/metrics.rs crates/core/tests/<file>
cargo clippy -p tt-core --lib --tests
git add -A && git commit -m "fix(agent-loop): bump record_request_served per dispatched turn (agent_run path)"
```

---

### Task 2: CallerTier entitlement gate (`TT_PANEL_MIN_TIER`, default allow-all)

**Files:**
- Modify: `crates/core/src/server.rs` (add `panel_min_tier_from_env`, near `panel_enabled_from_env` ~49) + `crates/core/src/lib.rs` (re-export, near the `panel_enabled_from_env` re-export ~51)
- Modify: `crates/core/src/state.rs` (add `panel_min_tier` field ~328, default in constructor ~375, `with_panel_min_tier` builder ~618)
- Modify: `crates/core/src/routes/panel.rs` (add `panel_tier_rank`)
- Modify: `crates/core/src/routes/chat.rs` (the gate in `prepare`'s panel block, between the kill-switch ~2694 and `PanelConfig::resolve` ~2698)
- Test: `crates/core/tests/panel_entitlement.rs` (new)

**Interfaces:**
- Produces: `pub fn panel_min_tier_from_env() -> tt_shared::CallerTier`; `AppState.panel_min_tier: tt_shared::CallerTier` + `AppState::with_panel_min_tier(CallerTier) -> Self`; `pub(crate) fn panel_tier_rank(t: CallerTier) -> u8`.

- [ ] **Step 1: Write the failing tests**

Create `crates/core/tests/panel_entitlement.rs`. Mirror the Phase-6 `responses_panel.rs` / `messages_ingress.rs` mock-panel app harness AND the caller-tier injection pattern from `crates/core/tests/tier_enforcement.rs` (how a test sets `ApiKeyContext.tier`). Tests:
1. **Default allow-all**: `build_panel_app()` with `with_panel_enabled(true)` and NO `with_panel_min_tier` → a panel request from a `Free`/`None`-tier caller returns `200` + renders `tokentrimmer.panel`.
2. **Gate bites**: same app `.with_panel_min_tier(CallerTier::Pro)` → a `Free`/`None`-tier caller's panel request returns `403`; assert via a call-counter mock that ZERO upstream dispatches happened and (if the harness captures rows) zero `request_logs` rows; a `Pro`-tier caller returns `200`.
3. **Order (kill-switch first)**: `with_panel_enabled(false).with_panel_min_tier(Pro)` + `Free` caller → `403` PanelDisabled (the kill-switch error), not the entitlement Forbidden.

Plus unit tests (in `panel.rs` / `server.rs` test modules): `panel_tier_rank` ordering (Free<Pro<Team<Scale); `panel_min_tier_from_env` parse — `"pro"`/`"PRO"`/`"team"`/`"scale"`/unknown/unset → expected `CallerTier` (unknown/unset ⇒ `Free`). (The env-parse unit test sets `TT_PANEL_MIN_TIER` — serialize via the existing env-test convention or test the parse helper with the var set then removed; do NOT hold a lock across `.await`.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tt-core --test panel_entitlement`
Expected: FAIL — `with_panel_min_tier` / `panel_min_tier` / `panel_tier_rank` don't exist; the gate-bites test gets `200` instead of `403`.

- [ ] **Step 3: Add the env parser + state field + builder + rank helper**

`server.rs` (after `panel_enabled_from_env`):
```rust
/// Read `TT_PANEL_MIN_TIER` → the minimum `CallerTier` allowed to use the panel.
/// `"pro"|"team"|"scale"` (case-insensitive) → that tier; absent/unknown → `Free`
/// (allow-all — the default, so the panel works today behind the kill-switch
/// until an operator tightens it or cloud injects real tiers).
pub fn panel_min_tier_from_env() -> tt_shared::CallerTier {
    use tt_shared::CallerTier::*;
    match std::env::var("TT_PANEL_MIN_TIER").map(|v| v.to_ascii_lowercase()).as_deref() {
        Ok("pro") => Pro,
        Ok("team") => Team,
        Ok("scale") => Scale,
        Ok("free") | Err(_) => Free,
        Ok(other) => { tracing::warn!(value = %other, "unknown TT_PANEL_MIN_TIER, defaulting to Free"); Free }
    }
}
```
`lib.rs`: add `panel_min_tier_from_env` to the `pub use server::{...}` line.
`state.rs`: add `pub panel_min_tier: tt_shared::CallerTier,` to `AppState` (with a doc comment mirroring `panel_enabled`); default `panel_min_tier: tt_shared::CallerTier::Free,` in the constructor (~375); and:
```rust
/// Builder: minimum CallerTier allowed to use the panel. `Free` (default) =
/// allow-all. Production wires `panel_min_tier_from_env()`.
#[must_use]
pub fn with_panel_min_tier(mut self, tier: tt_shared::CallerTier) -> Self {
    self.panel_min_tier = tier;
    self
}
```
`panel.rs`:
```rust
/// Entitlement rank for the panel min-tier gate (Free < Pro < Team < Scale).
/// Panel-local (not a global CallerTier Ord — Pro/Team share a TTL band).
pub(crate) fn panel_tier_rank(t: tt_shared::CallerTier) -> u8 {
    use tt_shared::CallerTier::*;
    match t { Free => 0, Pro => 1, Team => 2, Scale => 3 }
}
```

- [ ] **Step 4: Wire the gate in `prepare` (chat.rs)**

In the panel-resolution block, immediately after the kill-switch (`if !state.panel_enabled { return Err(PanelDisabled); }` ~2694) and before `PanelConfig::resolve` (~2698):
```rust
// Entitlement: panel requires `state.panel_min_tier` or higher. `caller_tier`
// is the prepare param (None ⇒ Free fallback). Default min Free ⇒ no-op.
let caller = caller_tier.unwrap_or(tt_shared::CallerTier::Free);
if panel::panel_tier_rank(caller) < panel::panel_tier_rank(state.panel_min_tier) {
    return Err(ApiError::Forbidden(format!(
        "panel: requires {:?} tier or higher",
        state.panel_min_tier
    )));
}
```
Remove any Phase-1 default-allow TODO comment at this site. Wire `panel_min_tier_from_env()` into production AppState construction wherever `with_panel_enabled` is wired (grep for `with_panel_enabled(` / `panel_enabled_from_env(` in `crates/core/src` + `crates/cli` and add the `.with_panel_min_tier(panel_min_tier_from_env())` alongside).

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p tt-core --test panel_entitlement`
Expected: PASS (default allow-all 200; gate-bites 403 + zero dispatch; order = PanelDisabled; rank/env-parse units).

- [ ] **Step 6: Regression + fmt/clippy + commit**

Run: `cargo test -p tt-core --test panel_engine --test panel_dispatch --test responses_panel --test messages_ingress` (existing panel suites unaffected — they don't set `panel_min_tier`, so default Free = allow-all).

```bash
cargo fmt -- crates/core/src/server.rs crates/core/src/lib.rs crates/core/src/state.rs crates/core/src/routes/panel.rs crates/core/src/routes/chat.rs crates/core/tests/panel_entitlement.rs
cargo clippy -p tt-core --lib --tests
git add -A && git commit -m "feat(panel): CallerTier entitlement gate (TT_PANEL_MIN_TIER, default allow-all)"
```

---

### Task 3: API-reference docs + kill-switch runbook

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (add a Deep Research Panel section)
- Create: `.claude/ops/panel-rollout.md` (kill-switch + entitlement rollout runbook)

**Interfaces:** docs only — no code, no test (a markdown link/lint at most).

- [ ] **Step 1: Read the doc structure**

Read `docs/04-gateway-api-reference.md` end-to-end to match its section style (how cache / routing / flex / compression features are documented — headers table, request/response examples, env vars).

- [ ] **Step 2: Add the Deep Research Panel section**

Document, in the established style:
- **Trigger:** `X-TokenTrimmer-Panel: synthesize | best-of-n | majority` header (primary, works on all three ingresses); `tt_extras.panel` config object (`members`, `arbiter`, `quorum`, `max_cost_usd`) accepted on `/v1/chat/completions` + `/v1/responses` (header-only on `/v1/messages`).
- **Behavior:** fan-out to N member legs + an arbiter; `synthesize` (LLM merge), `best-of-n` (LLM judge picks one verbatim), `majority` (embedding-cluster medoid). Legs non-streaming; arbiter streams when `stream:true` (chat completions; `/v1/messages` forwards the events; `/v1/responses` is non-streaming).
- **Response:** `tokentrimmer.panel` object — top-level body key (non-streaming) or trailing SSE event (streaming) — with the per-leg breakdown (`legs[]`: index, role, model, provider, cost, status, tokens), `quorum`, `cost_incomplete`, and `arbiter` (strategy + per-strategy detail). Same shape across all three ingresses.
- **Billing:** ONE `request_logs` row, `provider="panel"`, `cost_usd = Σ legs + arbiter`, `cached=false`; per-leg detail in `panel_legs`; served counted once.
- **Budget:** requires an explicit `X-TokenTrimmer-Cost-Limit-Usd` (or route `max_cost_usd`); over-budget / unpriceable ⇒ `402` before any dispatch (fail-closed).
- **Controls:** kill-switch `TT_PANEL_ENABLED` (off by default ⇒ `403 panel disabled`); entitlement `TT_PANEL_MIN_TIER` (default `Free`/allow-all; below it ⇒ `403`); member cap `TT_PANEL_MAX_MEMBERS` (default 8); `TT_PANEL_MAJORITY_THRESHOLD` (default 0.83).

- [ ] **Step 3: Write the rollout runbook**

Create `.claude/ops/panel-rollout.md`: purpose; pre-checks; enable (`TT_PANEL_ENABLED=1`, choose `TT_PANEL_MIN_TIER`, confirm `TT_PANEL_MAX_MEMBERS`); what to watch (the `tokentrimmer.panel.*` span attrs / `panel_*` metrics + aggregate `cost_usd`); rollback (`TT_PANEL_ENABLED=0` — immediate fail-closed, no in-flight corruption). State clearly that flipping the production env is a **user-gated infra action** (per the infra-writes convention) — the runbook documents the steps, it does not perform them.

- [ ] **Step 4: Verify + commit**

Skim both docs for accuracy against the shipped behavior (Phases 1–6). No code test.
```bash
git add docs/04-gateway-api-reference.md .claude/ops/panel-rollout.md
git commit -m "docs(panel): API-reference panel section + kill-switch/entitlement rollout runbook"
```

---

## Final whole-branch review + finish
After Task 3, dispatch the whole-branch reviewer (superpowers:requesting-code-review) on `feat/panel-phase7-entitlement` vs `main` (attention lens: the Global Constraints — default allow-all / off-by-default, gate ordering kill-switch→entitlement→budget, the served-fix label, no money-path change, docs accuracy). Then `superpowers:finishing-a-development-branch` (push + PR + merge-on-green + sync-main). **Merging this closes the entire deep-research-panel campaign** (Phases 1–7); update the campaign memory + note the deferred `RouteAction.panel` follow-up.

## Self-Review (plan vs spec)
- **Spec coverage:** D1 (env/state/builder) + D2 (panel_tier_rank) + D3 (gate placement/order) → Task 2; D4 (served fix) → Task 1; D5 (off-by-default) → the default-allow + order tests; docs + runbook → Task 3.
- **Placeholder scan:** all new code (env parser, builder, rank, gate snippet, served line) is concrete; the read-at-impl items (the exact agent-run test file; the production AppState construction sites for wiring) are cited grep targets, not TBDs.
- **Type consistency:** `panel_min_tier: CallerTier` (Task 2) is read by the gate via `panel_tier_rank(CallerTier) -> u8`; `with_panel_min_tier(CallerTier)` matches; `panel_min_tier_from_env() -> CallerTier` feeds the builder. `record_request_served(&'static str, &'static str)` (Task 1) matches the existing signature.
