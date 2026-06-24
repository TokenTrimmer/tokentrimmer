# `RouteAction.panel` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Let an org trigger + configure the deep-research panel via a routing rule (`RouteAction.panel`), reusing the shipped panel engine/gates; header wins over a route's panel.

**Architecture:** Add a self-contained `RoutePanel` nested struct + `RouteAction.panel: Option<RoutePanel>` to `tt_routing` ONLY (routing-only lever, like `agentic_budget` — NOT mirrored to plan-core). The gateway carries it through `RouteMatch` → `apply_routing` → the `prepare` panel-resolution block, where (header absent) it maps `RoutePanel` → `PanelExtras` → `PanelConfig::resolve` and runs the same kill-switch/entitlement/budget/engine path. Cloud needs only a git-pin bump (routes are JSONB).

**Tech Stack:** Rust — `crates/routing`, `crates/core` (public); `cloud/Cargo.toml` pin bump (cloud). Branch `feat/routeaction-panel` (public, created; spec committed). Spec: `docs/superpowers/specs/2026-06-23-routeaction-panel-design.md`.

## Global Constraints
- **Off-by-default:** a route without `panel` deserializes to `None` (serde default) and changes nothing; no header + no panel route ⇒ byte-identical single-model path; the existing `X-TokenTrimmer-Panel` header path is unchanged (header wins).
- **Routing-only:** `panel`/`RoutePanel` go in `tt_routing` ONLY — do NOT add to `tt_plan_core::RouteAction` (matches `agentic_budget`; the plan simulator doesn't model panel cost).
- **No dep cycle:** `tt_routing` cannot depend on `tt_core`. Strategy validation in `validate.rs` uses a routing-local literal set; the authoritative `ArbiterStrategyKind::parse` runs at request time in `tt_core`.
- **Same gates/engine:** route-triggered panels reuse `PanelConfig::resolve` + kill-switch + entitlement + budget gate + `complete_panel` unchanged.
- **CI gates (verify locally):** `cargo fmt --` (changed files) + `--check`; `cargo clippy -p tt-routing -p tt-core --lib --tests` no new warnings; the task's tests + relevant existing suites green. No `--all-targets`. Full local `--lib --tests` stalls on macOS dyld — CI authoritative.

---

### Task 1: `tt_routing` — `RoutePanel` + `RouteAction.panel` + validation

**Files:**
- Modify: `crates/routing/src/lib.rs` (add `RoutePanel` near `AgenticBudget` @340; add `panel` field to `RouteAction` after `agentic_budget` @330; export `RoutePanel` + `PANEL_STRATEGY_VALUES` + `validate_panel` from the crate root @30-31)
- Modify: `crates/routing/src/validate.rs` (add `PANEL_STRATEGY_VALUES`, `validate_panel`; extend `validate_route_has_effect` @205)
- Test: `crates/routing/src/validate.rs` (`#[cfg(test)]`) + `crates/routing/src/lib.rs` (`#[cfg(test)]`) inline unit tests

**Interfaces:**
- Produces: `pub struct RoutePanel { strategy: String, members: Vec<String>, arbiter: Option<String>, quorum: Option<usize>, max_cost_usd: Option<f64> }`; `RouteAction.panel: Option<RoutePanel>`; `pub const PANEL_STRATEGY_VALUES: [&str; 4]`; `pub fn validate_panel(&RouteAction) -> Result<(), ValidationError>`.
- Consumes (Task 2): `tt_core` reads `RouteAction.panel` / `RoutePanel` + calls `validate_panel`.

- [ ] **Step 1: Write the failing unit tests**

In `validate.rs` tests: (a) a `RouteAction` with only `panel: Some(RoutePanel{strategy:"synthesize",..})` (no `target_model`) passes `validate_route_has_effect`; (b) `validate_panel` accepts `strategy` ∈ `{synthesize, best-of-n, best_of_n, majority}` and rejects `"bogus"` with a `ValidationError`. In `lib.rs` tests: a `RouteAction { panel: None, .. }` serializes WITHOUT a `panel` key (serde skip), and a JSON string lacking `panel` deserializes to `panel: None`; a `RoutePanel` round-trips.

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p tt-routing validate_panel route_panel`
Expected: FAIL — `RoutePanel` / `panel` / `validate_panel` undefined.

- [ ] **Step 3: Add `RoutePanel` + the field**

In `lib.rs`, after `AgenticBudget` (~`:340+` block), add:
```rust
/// Deep-research panel config for a route-triggered panel (the same panel
/// engine as the X-TokenTrimmer-Panel header). Self-contained (not a re-export
/// of tt_shared PanelExtras) to keep the routing wire contract explicit and
/// avoid a tt_shared coupling in this crate — mirrors the AgenticBudget pattern.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutePanel {
    /// "synthesize" | "best-of-n" (or "best_of_n") | "majority". Validated at
    /// route creation against PANEL_STRATEGY_VALUES; parsed authoritatively at
    /// request time by tt_core's ArbiterStrategyKind::parse.
    pub strategy: String,
    /// Panel member model ids; empty ⇒ gateway env TT_PANEL_DEFAULT_MEMBERS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// Arbiter model id; None ⇒ env TT_PANEL_DEFAULT_ARBITER.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arbiter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quorum: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}
```
In `RouteAction` (after `pub agentic_budget: Option<AgenticBudget>,` @330):
```rust
/// Trigger + configure the deep-research panel for matched requests. None
/// (default) ⇒ no panel. A panel route is typically modifier-only
/// (target_model None); if target_model is also set, the panel governs
/// dispatch and the rewrite is inert (complete_panel branches first).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub panel: Option<RoutePanel>,
```

- [ ] **Step 4: Add `PANEL_STRATEGY_VALUES` + `validate_panel` + extend `has_effect`**

In `validate.rs`:
```rust
/// Accepted RouteAction.panel strategy strings. MUST stay a subset of what
/// tt_core's ArbiterStrategyKind::parse accepts (drift-guarded by a tt-core
/// test). tt_routing cannot depend on tt_core, so this is the routing-local
/// source of truth for route-creation validation.
pub const PANEL_STRATEGY_VALUES: [&str; 4] = ["synthesize", "best-of-n", "best_of_n", "majority"];

/// Reject a route whose panel.strategy is not a recognized strategy.
pub fn validate_panel(then: &RouteAction) -> Result<(), ValidationError> {
    if let Some(p) = &then.panel {
        if !PANEL_STRATEGY_VALUES.contains(&p.strategy.as_str()) {
            return Err(ValidationError::InvalidPanelStrategy(p.strategy.clone()));
        }
    }
    Ok(())
}
```
(Add a `ValidationError::InvalidPanelStrategy(String)` variant mirroring the existing error variants.) In `validate_route_has_effect` (@205), add `|| then.panel.is_some()` to the `has_effect` chain.
Export `RoutePanel`, `PANEL_STRATEGY_VALUES`, `validate_panel` from the crate root (`lib.rs:30-31` `pub use validate::{...}`).

- [ ] **Step 5: Run tests to pass + fmt/clippy**

Run: `cargo test -p tt-routing` ; `cargo fmt -- crates/routing/src/lib.rs crates/routing/src/validate.rs` ; `cargo clippy -p tt-routing --lib --tests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/routing/src/lib.rs crates/routing/src/validate.rs
git commit -m "feat(routing): RouteAction.panel + RoutePanel + validate_panel (routing-only)"
```

---

### Task 2: `tt_core` gateway wiring + drift-guard + integration tests

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (`RouteMatch` @6167 add `panel`; `apply_routing` @6479 populate active arm; paused arm @6366 sets `panel: None`; the `prepare` panel-resolution block @2690 — header-wins merge)
- Modify: `crates/core/src/routes/routes_api.rs` (@88 area — call `validate_panel(&spec.then)` in the create chain)
- Test: `crates/core/tests/route_panel.rs` (new — integration) + a drift-guard unit (in `panel.rs` `#[cfg(test)]` or `route_panel.rs`)

**Interfaces:**
- Consumes: `tt_routing::{RoutePanel, validate_panel, PANEL_STRATEGY_VALUES}` (Task 1); `panel::{PanelConfig, ArbiterStrategyKind, PanelDefaults}`, `tt_shared` `PanelExtras` (existing).

- [ ] **Step 1: Write the failing integration + drift tests**

Create `crates/core/tests/route_panel.rs` (mirror `panel_engine.rs` mock-panel app harness + `route_header.rs` routing-test harness — build an app with a routing store/engine carrying a route whose `then.panel = RoutePanel{strategy:"synthesize", members:[mockA,mockB], arbiter:mockArb}` and conditions matching the request model):
1. **Route triggers panel**: request (NO panel header) matching the panel route ⇒ 200, body has `tokentrimmer.panel` with legs A,B, one `provider='panel'` request_logs row with `matched_route_id` = the route's id.
2. **Header wins**: same request WITH `X-TokenTrimmer-Panel: best-of-n` ⇒ the panel runs `best-of-n` (header), not `synthesize` (route).
3. **Gates apply**: kill-switch off ⇒ 403; below-min tier ⇒ 403; over-budget ⇒ 402 (zero dispatch) — for the route-triggered panel.
4. **Paused route ⇒ no panel**: a paused panel route ⇒ single-model path (no `tokentrimmer.panel`).
5. **Off-by-default**: a route with no `panel` + no header ⇒ unchanged single-model.
Drift-guard unit: `for s in tt_routing::PANEL_STRATEGY_VALUES { assert!(ArbiterStrategyKind::parse(s).is_some()) }`.

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p tt-core --test route_panel`
Expected: FAIL — `RouteMatch` has no `panel`; route doesn't trigger the panel.

- [ ] **Step 3: Plumb `panel` through `RouteMatch` + `apply_routing`**

`RouteMatch` (@6167): add `pub(crate) panel: Option<tt_routing::RoutePanel>`. In `apply_routing`'s **active** arm (@6479) populate `panel: m.then.panel.clone()`. In the **paused** arm (@6366) construct `RouteMatch { ..., panel: None, .. }` (panel is a cost lever — off when paused, like `agentic_budget`).

- [ ] **Step 4: Header-wins merge in `prepare` (@2690)**

Extract `let route_panel = route_match.as_ref().and_then(|m| m.panel.clone());` (near where other route effects are read). Restructure the panel-resolution block so the trigger is header-first, route-fallback:
```rust
// header-wins: explicit X-TokenTrimmer-Panel beats a matched route's panel.
let panel_trigger = match panel::panel_from_header(headers) {
    Some(strategy) => Some(PanelTrigger::Header(strategy)),   // existing path (request tt_extras.panel as extras)
    None => route_panel.map(PanelTrigger::Route),             // NEW
};
let (panel, panel_creds) = if let Some(trigger) = panel_trigger {
    // kill-switch + entitlement gates run identically here (unchanged) ...
    let cfg = match trigger {
        PanelTrigger::Header(strategy) =>
            panel::PanelConfig::resolve(strategy, parse_panel_extras(&req.tt_extras).as_ref(), &defaults)?,
        PanelTrigger::Route(rp) => {
            let strategy = match panel::ArbiterStrategyKind::parse(&rp.strategy) {
                Some(s) => s, None => { /* defensive: skip panel */ return Ok((None, HashMap::new())) }
            };
            let extras = tt_shared::messages::PanelExtras {
                members: rp.members, arbiter_model: rp.arbiter, quorum: rp.quorum, max_cost_usd: rp.max_cost_usd,
            };
            panel::PanelConfig::resolve(strategy, Some(&extras), &defaults)?
        }
    };
    // budget gate + per-member cred resolution run identically (unchanged) ...
    (Some(cfg), creds)
} else { (None, HashMap::new()) };
```
(Define `enum PanelTrigger { Header(panel::ArbiterStrategyKind), Route(tt_routing::RoutePanel) }` locally. Keep the kill-switch/entitlement/budget/cred blocks exactly as today — only the trigger source + the `cfg` construction branch are new. Confirm `PanelExtras` field names against `tt_shared::messages` — `arbiter_model`, not `arbiter`.)

- [ ] **Step 5: Wire `validate_panel` into route creation**

`routes_api.rs` (after `validate_output_shaping` @83, before `validate_route_has_effect` @88): `validate_panel(&spec.then).map_err(|e| ApiError::InvalidRequest(e.to_string()))?;` and add `validate_panel` to the `use tt_routing::{...}` import (@12-13).

- [ ] **Step 6: Run tests to pass**

Run: `cargo test -p tt-core --test route_panel` ; regression `cargo test -p tt-core --test route_header --test panel_engine --test panel_entitlement`
Expected: PASS (route triggers panel; header wins; gates; paused; off-by-default; routing/panel regressions green).

- [ ] **Step 7: fmt + clippy + whole-crate + commit**

```bash
cargo fmt -- crates/core/src/routes/chat.rs crates/core/src/routes/routes_api.rs crates/core/tests/route_panel.rs
cargo clippy -p tt-core --lib --tests
git add -A && git commit -m "feat(panel): route-triggered panels — RouteAction.panel wired through prepare (header-wins) + tests"
```

---

### Task 3: Cloud git-pin bump (SEPARATE PR, AFTER public merges)

> Do this only after the public PR is merged to public `main` and you have the merge SHA. This is the cross-repo step (no schema/handler change — routes are JSONB).

**Files:** `cloud/Cargo.toml` (the 11 public-crate `git … rev = "<SHA>"` pins) + `cloud/Cargo.lock`.

- [ ] **Step 1:** In `cloud/Cargo.toml`, bump ALL 11 public-crate `rev = "<old SHA>"` to the public-main SHA that includes the merged RouteAction.panel feature (they must all be the SAME SHA — the lockstep model). `cargo update` for those crates.
- [ ] **Step 2:** `cd cloud && cargo build --workspace` — confirms the new `tt_routing::RouteAction` (with `panel`) compiles against cloud code (cloud stores `target` as opaque JSONB, so no handler change; this just re-resolves the graph).
- [ ] **Step 3:** Local DB test (Actions-minutes blocker): spin `pgvector/pgvector:pg17`, `TEST_DATABASE_URL=… cargo test -p tt-api -- --include-ignored --test-threads=1` (the routes admin tests stay green — JSONB passthrough unaffected).
- [ ] **Step 4:** Commit (`chore(deps): bump public pin to include RouteAction.panel`), open the cloud PR, and **admin-merge** (per the cloud Actions-minutes ops runbook) once local tests are green. (Surface to the user before admin-merging the private cloud repo if unsure.)

---

## Final whole-branch review + finish
After Task 2 (public), dispatch the whole-branch reviewer on `feat/routeaction-panel` vs `main` (attention lens: off-by-default + header-wins + same-gates + routing-only/no-plan-core-mirror + attribution). Then `superpowers:finishing-a-development-branch` (push + PR + merge-on-green + sync-main). Then Task 3 (cloud pin bump) as its own PR.

## Self-Review (plan vs spec)
- **Spec coverage:** D1 (RoutePanel, routing-only) → Task 1; D2 (header-wins) → Task 2 Step 4; D3 (same gates) → Task 2 (gates unchanged); D4 (target_model inert) → reused complete_once branch (no code); D5 (matched_route_id attribution) → Task 2 Step 1 assertion; D6 (validate self-contained) → Task 1 + drift guard Task 2. Cloud → Task 3.
- **Placeholder scan:** code blocks are concrete; the `prepare` block's exact line shifts with edits but is anchored by `panel_from_header` + `apply_routing`; `PanelExtras` field names flagged for verification.
- **Type consistency:** `RoutePanel` (Task 1) consumed as `tt_routing::RoutePanel` in `RouteMatch`/the merge (Task 2); `arbiter: Option<String>` → `PanelExtras.arbiter_model: Option<String>` → `resolve` lifts to `ModelRef`; `PANEL_STRATEGY_VALUES` defined Task 1, drift-tested Task 2.
