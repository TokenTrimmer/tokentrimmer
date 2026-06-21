# Deep Research Panel — Phase 1 (Core Fan-Out Engine) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in, header-gated, non-streaming, fixed-strategy (`synthesize`) multi-model panel that fans out one `/v1/chat/completions` request to N legs in parallel, arbitrates to one answer, and bills it as one aggregate request — fully off unless `X-TokenTrimmer-Panel` is present.

**Architecture:** A new `routes/panel.rs` module owns fan-out (`JoinSet` of `measured_single_dispatch`) + a pluggable `ArbiterStrategy` (only `Synthesize` in Phase 1). `complete_once` branches to `complete_panel` when a `PanelConfig` is present, *before* the cache-hit checks. Cost from all legs + arbiter sums into the single existing `cost_usd`/`request_logs` row; per-leg detail rides the response body. Spec: `docs/superpowers/specs/2026-06-21-deep-research-panel-design.md`.

**Tech Stack:** Rust, axum, tokio (`JoinSet`), the existing `Provider` trait + `measured_single_dispatch`, `tt_shared` types.

## Global Constraints

- **Off-by-default is sacred.** No `X-TokenTrimmer-Panel` header (and panel disabled or absent route config) ⇒ the request path is wire-identical to today. Proven by an explicit snapshot/golden test (Task 7.1).
- **One row, one served, `cached=false`, `cost_usd = SUM(legs+arbiter)`.** Invariants §2.1 of the spec. The panel must `record_request_served` exactly once and write exactly one `request_logs` row.
- **Fail-closed budget.** Any unpriceable leg or absent/insufficient budget ⇒ `ApiError::CostLimitExceeded` (402) *before any dispatch*.
- **Bypass cache + single-flight** for panel requests (non-deterministic).
- **CI gotchas (memory):** never run whole-crate `cargo fmt`; stage only files you touched. Verify with `cargo clippy --workspace --all-targets` and `cargo test --workspace --no-run` (field-ripple changes must compile test targets) before claiming done. Workspace tests can be disk-flaky → rerun once on spurious failure.
- **Provider trait takes `req` by value** ⇒ `req.clone()` per leg.
- Phase 1 writes **no DB migration** (that is Phase 2); per-leg detail is response-body only.

---

### Task 1: Error variants, metrics, and kill-switch scaffolding

**Files:**
- Modify: `public/crates/core/src/error.rs` (enum + `into_response` match, around `:16-66` and `:85-172`)
- Modify: `public/crates/core/src/metrics.rs` (register new counters near the existing `tt_requests_served_total` registration, `:50-103`)
- Modify: `public/crates/core/src/state.rs` (add `panel_enabled: bool` to `AppState` + builder)
- Modify: `public/crates/core/src/server.rs` (read `TT_PANEL_ENABLED` env into the builder)
- Test: `public/crates/core/tests/panel_engine.rs` (new file; first test stub here)

**Interfaces:**
- Produces: `ApiError::PanelDisabled`, `ApiError::PanelQuorumUnmet { required: usize, met: usize }`, `ApiError::PanelStrategyUnsupported { strategy: String }`; `AppState::with_panel_enabled(bool)` + `AppState.panel_enabled`; metrics `tt_panel_requests_total{strategy,outcome}`, `tt_panel_legs_total{role,status}`.

- [ ] **Step 1: Add the three error variants.** In `error.rs`, add to `enum ApiError`:

```rust
    /// The panel feature is disabled (kill-switch). Explicit — never a silent
    /// fallback to single-model, so a panel caller is not surprised by
    /// single-model billing.
    #[error("deep-research panel is disabled")]
    PanelDisabled,

    /// Too few legs survived to arbitrate.
    #[error("panel quorum unmet: {met} of {required} legs succeeded")]
    PanelQuorumUnmet { required: usize, met: usize },

    /// A panel strategy requested but not implemented in this build.
    #[error("panel strategy not supported: {strategy}")]
    PanelStrategyUnsupported { strategy: String },
```

- [ ] **Step 2: Map them in `into_response`.** Add arms to the match in `impl IntoResponse for ApiError`:

```rust
            ApiError::PanelDisabled => (
                StatusCode::FORBIDDEN,
                "permission_error",
                "panel_disabled",
                "The deep-research panel is not enabled on this gateway.".into(),
            ),
            ApiError::PanelQuorumUnmet { required, met } => (
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "panel_quorum_unmet",
                format!("Deep-research panel could not reach quorum: {met} of {required} legs succeeded."),
            ),
            ApiError::PanelStrategyUnsupported { strategy } => (
                StatusCode::NOT_IMPLEMENTED,
                "invalid_request_error",
                "panel_strategy_unsupported",
                format!("Deep-research panel strategy '{strategy}' is not supported yet."),
            ),
```

- [ ] **Step 3: Register metrics.** In `metrics.rs`, mirroring the existing counter registration pattern, add label-bounded counters `tt_panel_requests_total` (labels `strategy`, `outcome`) and `tt_panel_legs_total` (labels `role`, `status`). Expose increment helpers `record_panel_request(strategy, outcome)` and `record_panel_leg(role, status)` next to the existing `record_request_served` helper.

- [ ] **Step 4: Add the kill-switch.** In `state.rs`, add `pub panel_enabled: bool` to `AppState` (default `false`), a builder `pub fn with_panel_enabled(mut self, on: bool) -> Self`. In `server.rs`, where the `AppState` is built, read `std::env::var("TT_PANEL_ENABLED").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)` and pass to `.with_panel_enabled(...)`.

- [ ] **Step 5: Compile + commit.**

Run: `cargo build -p tt-core` → Expected: builds clean.
```bash
git add crates/core/src/error.rs crates/core/src/metrics.rs crates/core/src/state.rs crates/core/src/server.rs
git commit -m "feat(panel): error variants, metrics, and TT_PANEL_ENABLED kill-switch"
```

---

### Task 2: `PanelConfig` + header / `tt_extras` parsing

**Files:**
- Create: `public/crates/core/src/routes/panel.rs` (module skeleton + config types)
- Modify: `public/crates/core/src/routes/mod.rs` (add `pub(crate) mod panel;`)
- Modify: `public/crates/core/src/routes/chat.rs` (add `panel_from_header` in the helper cluster `:76-133`)
- Modify: `public/crates/shared/src/messages.rs` (parse `tt_extras.panel`, mirroring `parse_cache_control:54-70`)
- Test: `public/crates/core/tests/panel_config.rs` (new)

**Interfaces:**
- Produces:
  ```rust
  pub(crate) enum ArbiterStrategyKind { Synthesize, BestOfN, Majority }
  pub(crate) struct ModelRef { pub model: String, pub provider: Option<String> }
  pub(crate) struct PanelConfig {
      pub strategy: ArbiterStrategyKind,
      pub members: Vec<ModelRef>,
      pub arbiter_model: ModelRef,
      pub quorum: Option<usize>,
      pub max_cost_usd: Option<f64>,
  }
  pub(crate) fn panel_from_header(headers: &HeaderMap) -> Option<ArbiterStrategyKind>;
  impl PanelConfig { pub(crate) fn resolve(strategy: ArbiterStrategyKind, extras: Option<&PanelExtras>, defaults: &PanelDefaults) -> Result<PanelConfig, ApiError> }
  ```
- Consumes: `tt_shared::messages` `tt_extras` bag.

- [ ] **Step 1: Write the failing test (header parse).** In `tests/panel_config.rs`:

```rust
use tt_core::routes::panel::{panel_from_header, ArbiterStrategyKind};
use axum::http::HeaderMap;

#[test]
fn header_absent_is_none() {
    assert!(panel_from_header(&HeaderMap::new()).is_none());
}

#[test]
fn header_synthesize_parses() {
    let mut h = HeaderMap::new();
    h.insert("x-tokentrimmer-panel", "synthesize".parse().unwrap());
    assert!(matches!(panel_from_header(&h), Some(ArbiterStrategyKind::Synthesize)));
}

#[test]
fn header_unknown_strategy_is_none() {
    let mut h = HeaderMap::new();
    h.insert("x-tokentrimmer-panel", "bogus".parse().unwrap());
    assert!(panel_from_header(&h).is_none());
}
```

(Make `routes::panel` test-visible: ensure `pub(crate)` items used in tests are re-exported under a `#[doc(hidden)] pub mod` test surface, or mark these specific items `pub`. Follow whatever the crate already does for `tests/` access — e.g. `provider_override_from_header` is reached via the crate's test exports; mirror that.)

- [ ] **Step 2: Run, verify fail.** `cargo test -p tt-core --test panel_config` → Expected: FAIL (unresolved import).

- [ ] **Step 3: Implement config types + `panel_from_header`** in `routes/panel.rs`:

```rust
use axum::http::HeaderMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArbiterStrategyKind { Synthesize, BestOfN, Majority }

impl ArbiterStrategyKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self { Self::Synthesize => "synthesize", Self::BestOfN => "best-of-n", Self::Majority => "majority" }
    }
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "synthesize" => Some(Self::Synthesize),
            "best-of-n" | "best_of_n" => Some(Self::BestOfN),
            "majority" => Some(Self::Majority),
            _ => None,
        }
    }
}

pub(crate) fn panel_from_header(headers: &HeaderMap) -> Option<ArbiterStrategyKind> {
    headers.get("x-tokentrimmer-panel")
        .and_then(|v| v.to_str().ok())
        .and_then(ArbiterStrategyKind::parse)
}

#[derive(Clone, Debug)]
pub(crate) struct ModelRef { pub model: String, pub provider: Option<String> }

#[derive(Clone, Debug)]
pub(crate) struct PanelConfig {
    pub strategy: ArbiterStrategyKind,
    pub members: Vec<ModelRef>,
    pub arbiter_model: ModelRef,
    pub quorum: Option<usize>,
    pub max_cost_usd: Option<f64>,
}
```

Add `pub(crate) mod panel;` to `routes/mod.rs`. Add the crate test-surface re-export the crate already uses so `tests/` can import these.

- [ ] **Step 4: Run, verify pass.** `cargo test -p tt-core --test panel_config` → PASS.

- [ ] **Step 5: Add `tt_extras.panel` parsing.** In `messages.rs`, next to `parse_cache_control` (`:54-70`), add a `PanelExtras { members: Vec<...>, arbiter_model: Option<String>, quorum: Option<usize>, max_cost_usd: Option<f64> }` deserialized from the `tt_extras["panel"]` value; tolerate absence. Add a unit test in `messages.rs`'s `#[cfg(test)]` asserting a body with `tt_extras.panel.members=["a","b"]` parses and a body without `panel` yields `None`.

- [ ] **Step 6: Implement `PanelConfig::resolve`.** Merge header strategy + optional `PanelExtras` + gateway `PanelDefaults` (a small config struct with default member list + default arbiter model, sourced from env/config — `TT_PANEL_DEFAULT_MEMBERS`, `TT_PANEL_DEFAULT_ARBITER`). If members are empty after merge ⇒ `ApiError::InvalidRequest("panel requires at least one member model")`. Unit-test the merge precedence (header-only → defaults; extras override defaults).

- [ ] **Step 7: Commit.**
```bash
git add crates/core/src/routes/panel.rs crates/core/src/routes/mod.rs crates/core/src/routes/chat.rs crates/shared/src/messages.rs crates/core/tests/panel_config.rs
git commit -m "feat(panel): PanelConfig + X-TokenTrimmer-Panel header and tt_extras.panel parsing"
```

---

### Task 3: `LegResult`, `ArbiterStrategy` trait, and `Synthesize`

**Files:**
- Modify: `public/crates/core/src/routes/panel.rs`
- Test: `public/crates/core/tests/panel_arbiter.rs` (new)

**Interfaces:**
- Produces:
  ```rust
  pub(crate) enum LegRole { Leg, Arbiter }
  pub(crate) enum LegStatus { Ok, Error, Timeout, SkippedNoCred }
  pub(crate) struct LegResult {
      pub leg_index: usize, pub role: LegRole, pub model: String, pub provider: String,
      pub status: LegStatus, pub response: Option<tt_shared::ChatCompletionResponse>,
      pub cost_usd: Option<f64>, pub usage: Option<tt_shared::Usage>, pub latency_ms: u64,
  }
  pub(crate) struct ArbiterOutcome { pub response: tt_shared::ChatCompletionResponse, pub cost_usd: Option<f64> }
  #[async_trait] pub(crate) trait ArbiterStrategy {
      async fn arbitrate(&self, request: &ChatCompletionRequest, legs: &[LegResult], state: &AppState, ctx: &RequestContext) -> Result<ArbiterOutcome, ApiError>;
  }
  pub(crate) struct Synthesize { pub arbiter_model: ModelRef }
  pub(crate) fn strategy_for(cfg: &PanelConfig) -> Result<Box<dyn ArbiterStrategy + Send + Sync>, ApiError>;
  ```

- [ ] **Step 1: Failing test for `strategy_for`.** In `tests/panel_arbiter.rs`: assert `strategy_for` returns a `Synthesize` for `Synthesize`, and `Err(ApiError::PanelStrategyUnsupported{..})` for `BestOfN` and `Majority`.

- [ ] **Step 2: Run, verify fail.** `cargo test -p tt-core --test panel_arbiter` → FAIL.

- [ ] **Step 3: Implement the types + trait + `Synthesize::arbitrate` + `strategy_for`.** `Synthesize::arbitrate` builds an arbiter prompt over the `LegStatus::Ok` leg answers (adapt the template at `agent_run.rs:1411-1434::build_summary_request` — a system instruction "synthesize the following N candidate answers into one best answer" + each leg's content), then runs ONE `crate::measurement::measured_single_dispatch(provider, arbiter_req, ctx, deadline)` on the resolved `arbiter_model` (resolve via `state.registry.resolve`). Map its `MeasuredDispatch { response, cost_usd }` into `ArbiterOutcome`. `strategy_for` returns `Box::new(Synthesize{..})` for `Synthesize`, else `Err(PanelStrategyUnsupported)`.

- [ ] **Step 4: Run, verify pass.** `cargo test -p tt-core --test panel_arbiter` → PASS.

- [ ] **Step 5: Commit.**
```bash
git add crates/core/src/routes/panel.rs crates/core/tests/panel_arbiter.rs
git commit -m "feat(panel): LegResult, ArbiterStrategy trait, and Synthesize strategy"
```

---

### Task 4: `estimate_panel_cost` + fail-closed budget gate

**Files:**
- Modify: `public/crates/core/src/routes/panel.rs`
- Test: `public/crates/core/tests/panel_budget.rs` (new)

**Interfaces:**
- Produces: `pub(crate) fn estimate_panel_cost(state: &AppState, cfg: &PanelConfig, input_tokens: u32, max_tokens: Option<u32>) -> Option<f64>` (returns `None` if ANY member or the arbiter is unpriceable — fail-closed signal); `pub(crate) fn panel_budget_gate(state, cfg, input_tokens, max_tokens, ceiling: Option<f64>) -> Result<(), ApiError>`.
- Consumes: `chat::estimate_cost_usd` (`chat.rs:65-74`), `Provider::pricing`, `Provider::fee_multiplier`.

- [ ] **Step 1: Failing tests.** In `tests/panel_budget.rs`:
  - Sum: two priced members + priced arbiter ⇒ `estimate_panel_cost` ≈ sum of each `estimate_cost_usd × fee_multiplier`.
  - Fail-closed unpriceable: one member with no catalog pricing ⇒ `estimate_panel_cost` returns `None`.
  - Gate over budget: estimate > ceiling ⇒ `Err(CostLimitExceeded{..})`.
  - Gate unpriceable: `None` estimate ⇒ `Err(CostLimitExceeded{..})` (treat unpriceable as over-ceiling).
  - Gate no ceiling: `ceiling=None` ⇒ `Err(CostLimitExceeded{..})` (a panel REQUIRES an explicit budget).

- [ ] **Step 2: Run, verify fail.** `cargo test -p tt-core --test panel_budget` → FAIL.

- [ ] **Step 3: Implement.**

```rust
pub(crate) fn estimate_panel_cost(state: &AppState, cfg: &PanelConfig, input_tokens: u32, max_tokens: Option<u32>) -> Option<f64> {
    let mut total = 0.0_f64;
    for m in cfg.members.iter().chain(std::iter::once(&cfg.arbiter_model)) {
        let provider = state.registry.resolve(&m.model).ok()?;        // unknown model => fail-closed
        let pricing = provider.pricing(&m.model)?;                    // unpriceable => fail-closed (None)
        total += crate::routes::chat::estimate_cost_usd(&pricing, input_tokens, max_tokens) * provider.fee_multiplier();
    }
    Some(total)
}

pub(crate) fn panel_budget_gate(state: &AppState, cfg: &PanelConfig, input_tokens: u32, max_tokens: Option<u32>, ceiling: Option<f64>) -> Result<(), ApiError> {
    let ceiling = ceiling.or(cfg.max_cost_usd)
        .ok_or(ApiError::CostLimitExceeded { estimated_usd: f64::INFINITY, ceiling_usd: 0.0 })?;
    let est = estimate_panel_cost(state, cfg, input_tokens, max_tokens)
        .ok_or(ApiError::CostLimitExceeded { estimated_usd: f64::INFINITY, ceiling_usd: ceiling })?;
    if est > ceiling {
        return Err(ApiError::CostLimitExceeded { estimated_usd: est, ceiling_usd: ceiling });
    }
    Ok(())
}
```

(Confirm `state.registry.resolve` returns `Result<Arc<dyn Provider>, _>` — adapt to the actual signature at `registry.rs:70-74`. Use the same input-token estimate source the existing single-model ceiling uses at `chat.rs:2577-2586`.)

- [ ] **Step 4: Run, verify pass.** PASS.

- [ ] **Step 5: Commit.**
```bash
git add crates/core/src/routes/panel.rs crates/core/tests/panel_budget.rs
git commit -m "feat(panel): fail-closed, fee-aware summed-over-legs budget estimator + gate"
```

---

### Task 5: `complete_panel` — fan-out, quorum, cost aggregation

**Files:**
- Modify: `public/crates/core/src/routes/panel.rs`
- Modify: `public/crates/core/src/measurement.rs` (confirm `measured_single_dispatch`/`MeasuredDispatch` reachable from `routes::panel`; they are `pub(crate)` — same crate, so OK)
- Test: `public/crates/core/tests/panel_fanout.rs` (new; uses the crate's mock `Provider` test harness — find it via the existing `tests/cross_provider.rs` / `tests/failover.rs` mock providers and reuse that fixture)

**Interfaces:**
- Produces:
  ```rust
  pub(crate) struct PanelResult {
      pub response: tt_shared::ChatCompletionResponse, // the arbiter's answer
      pub legs: Vec<LegResult>,                        // legs + the arbiter leg (role=Arbiter)
      pub total_cost_usd: Option<f64>,                 // SUM(leg + arbiter), None-aware
      pub quorum_required: usize,
      pub quorum_met: usize,
  }
  pub(crate) async fn run_panel(state: &AppState, ctx: &RequestContext, base_req: &ChatCompletionRequest, creds: &HashMap<String, ProviderCredentials>, cfg: &PanelConfig, deadline: Duration) -> Result<PanelResult, ApiError>;
  ```
- Consumes: `measured_single_dispatch` (`measurement.rs:55-77`), the failover per-provider credential map shape (`Prepared.failover_creds`).

- [ ] **Step 1: Failing tests** (with mock providers): (a) 2 mock legs both return ⇒ `PanelResult.legs` len 3 (2 legs + arbiter), `total_cost_usd` = sum of the three mock costs, `quorum_met == 2`. (b) all legs error ⇒ `Err(PanelQuorumUnmet{required:1, met:0})`. (c) a member missing from `creds` ⇒ that leg `status == SkippedNoCred`, not dispatched, and does not count toward quorum.

- [ ] **Step 2: Run, verify fail.** `cargo test -p tt-core --test panel_fanout` → FAIL.

- [ ] **Step 3: Implement `run_panel`:**

```rust
use tokio::task::JoinSet;
use std::time::Instant;

pub(crate) async fn run_panel(
    state: &AppState, ctx: &RequestContext, base_req: &ChatCompletionRequest,
    creds: &std::collections::HashMap<String, ProviderCredentials>, cfg: &PanelConfig, deadline: std::time::Duration,
) -> Result<PanelResult, ApiError> {
    // 1. Resolve legs; skip members with no credential (record, don't dispatch).
    let mut set: JoinSet<LegResult> = JoinSet::new();
    let mut skipped: Vec<LegResult> = Vec::new();
    for (i, m) in cfg.members.iter().enumerate() {
        let provider = state.registry.resolve(&m.model)
            .map_err(|_| ApiError::ModelNotFound { model: m.model.clone() })?;
        let pid = provider.id().to_string();
        if !creds.contains_key(&pid) {
            crate::metrics::record_panel_leg("leg", "skipped_no_cred");
            skipped.push(LegResult { leg_index: i, role: LegRole::Leg, model: m.model.clone(), provider: pid,
                status: LegStatus::SkippedNoCred, response: None, cost_usd: None, usage: None, latency_ms: 0 });
            continue;
        }
        let mut req = base_req.clone();           // Provider::chat_completion takes req by value
        req.model = m.model.clone();
        let leg_ctx = ctx.clone();                // adapt: ctx may need per-provider creds substituted
        let idx = i;
        set.spawn(async move {
            let started = Instant::now();
            match crate::measurement::measured_single_dispatch(&provider, req, &leg_ctx, deadline).await {
                Ok(md) => LegResult { leg_index: idx, role: LegRole::Leg, model: md.response.model.clone(), provider: pid,
                    status: LegStatus::Ok, cost_usd: md.cost_usd, usage: Some(md.response.usage.clone()),
                    latency_ms: started.elapsed().as_millis() as u64, response: Some(md.response) },
                Err(_) => LegResult { leg_index: idx, role: LegRole::Leg, model: String::new(), provider: pid,
                    status: LegStatus::Error, response: None, cost_usd: None, usage: None,
                    latency_ms: started.elapsed().as_millis() as u64 },
            }
        });
    }
    let mut legs: Vec<LegResult> = skipped;
    while let Some(joined) = set.join_next().await {
        if let Ok(leg) = joined {
            crate::metrics::record_panel_leg("leg", leg.status.as_str());
            legs.push(leg);
        }
    }

    // 2. Quorum.
    let required = cfg.quorum.unwrap_or(match cfg.strategy {
        ArbiterStrategyKind::Majority => (cfg.members.len() / 2) + 1,
        _ => 1,
    });
    let survivors: Vec<&LegResult> = legs.iter().filter(|l| matches!(l.status, LegStatus::Ok)).collect();
    let met = survivors.len();
    if met < required { return Err(ApiError::PanelQuorumUnmet { required, met }); }

    // 3. Arbitrate.
    let strategy = strategy_for(cfg)?;
    let arb = strategy.arbitrate(base_req, &legs, state, ctx).await?;
    let arbiter_leg = LegResult { leg_index: legs.len(), role: LegRole::Arbiter, model: cfg.arbiter_model.model.clone(),
        provider: state.registry.resolve(&cfg.arbiter_model.model).map(|p| p.id().to_string()).unwrap_or_default(),
        status: LegStatus::Ok, cost_usd: arb.cost_usd, usage: Some(arb.response.usage.clone()), latency_ms: 0,
        response: None };
    crate::metrics::record_panel_leg("arbiter", "ok");

    // 4. Aggregate cost: SUM over legs + arbiter, None-aware (mirror sum_metered, agent_run.rs:330-340).
    let total = sum_metered(legs.iter().map(|l| l.cost_usd).chain(std::iter::once(arb.cost_usd)));

    let mut all = legs; all.push(arbiter_leg);
    Ok(PanelResult { response: arb.response, legs: all, total_cost_usd: total, quorum_required: required, quorum_met: met })
}
```

Add a local `fn sum_metered(it: impl Iterator<Item = Option<f64>>) -> Option<f64>` matching the `agent_run.rs:330-340` convention (sum of `Some`s; `None` only if you choose to propagate — here: sum the `Some`s and keep a flag; return `Some(sum)` but the caller marks the body if any leg was `None`). Add `LegStatus::as_str`.

- [ ] **Step 4: Run, verify pass.** PASS. (If the mock-provider fixture differs, adapt the test to the crate's existing harness — do not invent a new mock.)

- [ ] **Step 5: Commit.**
```bash
git add crates/core/src/routes/panel.rs crates/core/tests/panel_fanout.rs
git commit -m "feat(panel): run_panel concurrent fan-out with quorum and None-aware cost aggregation"
```

---

### Task 6: Wire `complete_panel` into `complete_once`

**Files:**
- Modify: `public/crates/core/src/routes/panel.rs` (add `complete_panel` returning `CompletionOutcome`)
- Modify: `public/crates/core/src/routes/chat.rs` (`prepare`: build `Option<PanelConfig>` from header+extras, store on `Prepared`; `complete_once`: branch to `complete_panel` BEFORE cache checks; record-once + `cached=false` + body injection)
- Modify: `public/crates/core/src/routes/chat.rs` (`Prepared` struct: add `pub panel: Option<PanelConfig>`)

**Interfaces:**
- Produces: `pub(crate) async fn complete_panel(state: &AppState, ctx: &RequestContext, prep: Prepared, cfg: PanelConfig) -> Result<CompletionOutcome, ApiError>`.

- [ ] **Step 1: Add `panel: Option<PanelConfig>` to `Prepared`** (`chat.rs:863-941`) and populate it in `prepare`: parse `panel_from_header(&headers)`; if `Some`, resolve `PanelConfig` (header + `tt_extras.panel` + defaults); if the panel is disabled (`!state.panel_enabled`) return `Err(ApiError::PanelDisabled)` from `prepare`; run `panel_budget_gate(... , cost_limit_from_header(&headers))?` HERE (before any dispatch). Leave `panel = None` otherwise.

- [ ] **Step 2: Branch in `complete_once`.** At the top of `complete_once` (`chat.rs:1009`), before the negative/L1/L2 cache checks (`:1062-1247`): `if let Some(cfg) = prep.panel.take() { return complete_panel(state, ctx, prep, cfg).await; }`. This guarantees panels bypass cache + single-flight.

- [ ] **Step 3: Implement `complete_panel`.** Call `run_panel(state, ctx, &prep.req, &prep.failover_creds, &cfg, deadline)`; on `Ok(panel_result)`:
  - Build the served `ChatCompletionResponse` = `panel_result.response`.
  - Compute the `CostBreakdown` carrying `cost_usd = panel_result.total_cost_usd.unwrap_or(0.0)` (aggregate; mirror how `complete_once` builds `CostBreakdown` but substitute the aggregate sum — do NOT double-count via `compute_cost` on the arbiter response alone).
  - **Record exactly once**, `cached=false`: reuse the same `spend_sink().record(cost_usd)` + `settle(false)` + `record_request_served` + single `request_logs` write the dispatched arm uses (`chat.rs:1629-1640`, `2164-2187`). Stamp `provider = "panel"` on the row (decision A sentinel) and `model = cfg.arbiter_model.model`.
  - Inject the panel body object into the response JSON before returning: `tokentrimmer.panel = { strategy, legs:[{leg_index,role,model,provider,cost_usd,status,tokens}], total_cost_usd, quorum:{required,met}, cost_incomplete: <true if any surviving leg cost was None> }`.
  - Return `CompletionOutcome::Dispatched { response, headers: Box::new(CompletionHeaders { trace_id, provider_id: "panel".into(), model_used: cfg.arbiter_model.model, cost_breakdown, cache_state: "none", route_matched_name: prep.route_matched_name, body_captured: false, req: prep.req, provider: prep.provider, warnings: prep.warnings }) }`.

  (The exact record/settle/log calls must mirror `complete_once`'s dispatched tail verbatim — read `chat.rs:1600-1900` and replicate, substituting the aggregate cost and the `panel` provider stamp. This is the highest-risk step; keep the one-row/one-served discipline exactly.)

- [ ] **Step 4: Compile.** `cargo build -p tt-core` → clean.

- [ ] **Step 5: Commit.**
```bash
git add crates/core/src/routes/chat.rs crates/core/src/routes/panel.rs
git commit -m "feat(panel): wire complete_panel into complete_once with aggregate one-row billing"
```

---

### Task 7: Integration tests (the invariant suite)

**Files:**
- Test: `public/crates/core/tests/panel_engine.rs` (extend from Task 1)

Use the crate's existing integration-test harness (the same one `cost_routing.rs` / `route_header.rs` / `cross_provider.rs` use — a router built over mock providers with a fake API key). Mirror their setup exactly.

- [ ] **Step 1 (7.1 off-by-default golden):** Send a normal `/v1/chat/completions` request with NO panel header through a panel-*enabled* gateway; assert the response status, body, and all `x-tokentrimmer-*` headers are byte-identical to the same request through a panel-*disabled* gateway (snapshot). Proves invariant #1.
- [ ] **Step 2 (7.2 happy path):** `X-TokenTrimmer-Panel: synthesize` + `X-TokenTrimmer-Cost-Limit-Usd: 5.0` + 2 configured members ⇒ 200; body has `tokentrimmer.panel.legs` length 3 (2 legs + arbiter); `x-tokentrimmer-cost-usd` parses to the summed mock cost.
- [ ] **Step 3 (7.3 fail-closed budget):** over-budget OR an unpriceable member ⇒ 402 `cost_limit_exceeded`; assert the mock providers recorded **zero** dispatch calls.
- [ ] **Step 4 (7.4 quorum unmet):** both mock members configured to error ⇒ 502 `panel_quorum_unmet`; assert exactly one `request_logs` row was written and it is `cached=false`.
- [ ] **Step 5 (7.5 served==rows):** a happy-path panel increments the served counter by exactly 1 and writes exactly 1 `request_logs` row; `tt_panel_legs_total` increments by 3. (Assert via the metrics registry handle the other metrics tests use.)
- [ ] **Step 6 (7.6 kill-switch):** `TT_PANEL_ENABLED` off + panel header ⇒ 403 `panel_disabled`; zero dispatches.
- [ ] **Step 7 (7.7 multi-provider):** two members on different mock providers ⇒ both dispatch; aggregate cost sums across providers; the `request_logs` row `provider == "panel"`.
- [ ] **Step 8: Run the whole suite.** `cargo test -p tt-core --test panel_engine --test panel_config --test panel_arbiter --test panel_budget --test panel_fanout` → all PASS.
- [ ] **Step 9: Full verify (CI parity).**
  Run: `cargo clippy --workspace --all-targets -- -D warnings` (touched crates clean) and `cargo test --workspace --no-run` (all targets compile). Rerun once if a workspace test fails on disk flakiness.
- [ ] **Step 10: Commit.**
```bash
git add crates/core/tests/panel_engine.rs
git commit -m "test(panel): off-by-default golden + happy/fail-closed/quorum/served-rows/kill-switch/multi-provider"
```

---

## Self-Review

**1. Spec coverage:**
- §6.1 `complete_panel` → Task 6. §6.2 types → Tasks 2,3,5. §6.3 arbiter seam → Task 3. §6.4 data-flow steps: parse→T2, entitlement→(deferred to Phase 7, noted in T6 Step 1 as default-allow), budget→T4/T6, resolve legs→T5, fan-out→T5, quorum→T5, arbitrate→T3/T5, aggregate→T5, assemble/body→T6, record-once→T6. §6.5 bypass/kill-switch→T1/T6. §6.6 metrics→T1/T5. §6.7 files→all. §6.8 tests→T7 (all seven) + per-task unit tests.
- **Gap noted:** §6.4 step 2 entitlement (CallerTier gate) is intentionally deferred to Phase 7 (CallerTier is fail-open-to-Free today); Task 6 Step 1 default-allows with a `// TODO(phase7): CallerTier entitlement gate` marker. Acceptable per spec.
- **Gap noted:** the exact `CostBreakdown` construction for the aggregate (Task 6 Step 3) cannot be fully transcribed without reading `chat.rs:1600-1900`; the task says to mirror that region and substitute the aggregate sum. This is guidance-with-citation, not a placeholder — the implementer reads the cited lines.

**2. Placeholder scan:** No "TBD/implement later". The two "adapt to exact signature at chat.rs:NNNN / registry.rs:70-74" notes are deliberate (large existing file); each cites the exact location to read.

**3. Type consistency:** `ArbiterStrategyKind`, `ModelRef`, `PanelConfig`, `LegResult`/`LegRole`/`LegStatus`, `ArbiterOutcome`, `PanelResult`, `run_panel`, `complete_panel`, `estimate_panel_cost`, `panel_budget_gate` are used consistently across Tasks 2–7. `measured_single_dispatch`/`MeasuredDispatch` match `measurement.rs:55-77`. `ApiError` variants match Task 1.
