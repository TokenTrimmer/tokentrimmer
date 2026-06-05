# V3d-2a — Cost-Condition Routing Design

**Status:** approved (design)
**Date:** 2026-06-04
**Slice:** V3d-2a (first of two V3d-2 sub-slices; V3d-2b = per-request cost ceiling / `max_cost_usd` block, deferred)
**Part of:** V3d-2 "cost-aware routing" — the last V3 area.

## Goal

Make a request's **cost** a first-class routing signal: a route can match when the request's estimated cost crosses a dollar threshold (`estimated_cost_gt` / `estimated_cost_lt`), so expensive requests reroute to a cheaper model. The exact dollar parallel of the existing `input_tokens_gt` / `input_tokens_lt` conditions.

This is the "reroute" (cost-saving) half of V3d-2. The "block" half — a hard per-request ceiling (`max_cost_usd`) — is V3d-2b.

## Background

- The routing engine (`tt_routing`) matches `RouteConditions` against a request; the gateway supplies a cheap `input_tokens` estimate to `engine.evaluate(req, ctx, input_tokens)` (the engine never tokenizes or prices itself — that's the caller's job). Cost follows the same pattern: the caller computes an estimate and passes it in.
- **Cost is a logged field.** Unlike modality (V3a) and topic (V3c) — which Plan replay can't evaluate from history — `request_logs.baseline_cost_usd` records each request's cost on its **original** model. So `tt plan` can evaluate a cost condition against real logged cost with **no historical-data limitation** — the most accurately-projectable routing condition we have.

## Design

### 1. `tt_routing` — the cost condition
`crates/routing/src/lib.rs`:
- Add to `RouteConditions` (after the modality/topic fields):
  ```rust
  /// Match only if the request's estimated cost (USD) is greater than this.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub estimated_cost_gt: Option<f64>,
  /// Match only if the request's estimated cost (USD) is less than this.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub estimated_cost_lt: Option<f64>,
  ```
- Thread the estimate through: `evaluate(&self, req, ctx, input_tokens: u32, estimated_cost_usd: f64)` and `matches(r, req, ctx, input_tokens, estimated_cost_usd)`. Add the matcher arms (mirroring the `input_tokens_lt`/`_gt` arms):
  ```rust
  if let Some(t) = c.estimated_cost_gt {
      if estimated_cost_usd <= t { return false; }
  }
  if let Some(t) = c.estimated_cost_lt {
      if estimated_cost_usd >= t { return false; }
  }
  ```
- Update all `evaluate(...)` call sites (1 prod caller in `chat.rs`, ~25 in the lib's own tests) to pass the new arg — `0.0` in tests that don't exercise cost (a route with no cost condition ignores it). Compiler-driven.

### 2. Gateway — compute and pass the estimate
`crates/core/src/routes/chat.rs` `apply_routing`:
- Resolve the requested model's pricing (it already resolves the provider for `provider_id`): `state.registry.resolve(&req.model).and_then(|p| p.pricing(&req.model))`.
- Compute the estimate:
  ```rust
  const DEFAULT_OUTPUT_TOKENS_ESTIMATE: u32 = 1000;
  let output_est = req.max_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS_ESTIMATE);
  let estimated_cost_usd = match req_pricing {
      Some(p) => (f64::from(input_tokens) * p.input_per_million
          + f64::from(output_est) * p.output_per_million) / 1_000_000.0,
      None => 0.0, // unknown pricing → 0 cost estimate → cost conditions don't fire (permissive)
  };
  ```
  (`input_tokens` is the existing token estimate; `req.max_tokens` is the OpenAI-compat field — verify exact name/type in the plan.)
- Pass `estimated_cost_usd` to `engine.evaluate(req, ctx, input_tokens, estimated_cost_usd)`.
- The estimate is computed on the **originally-requested** model (pre-rewrite) — "if this request as-submitted would be expensive, reroute it." Matches the intent and uses the pricing already on hand.

### 3. `tt_plan_core` — mirror (accurately projectable)
`crates/plan-core/src/types.rs` `RouteConditions`: add the same two `Option<f64>` fields.
`crates/plan-core/src/routing.rs` `matches_conditions(req, c)`: add arms comparing **`req.baseline_cost_usd`** (the request's logged cost on its original model — the exact analogue of the gateway's pre-rewrite estimate):
```rust
if let Some(t) = c.estimated_cost_gt {
    if req.baseline_cost_usd <= t { return false; }
}
if let Some(t) = c.estimated_cost_lt {
    if req.baseline_cost_usd >= t { return false; }
}
```
No estimation/limitation caveat needed — this is real logged cost.

### 4. CLI
`crates/cli/src/route/mod.rs` + `main.rs`: add `--when-cost-gt <USD>` / `--when-cost-lt <USD>` to `tt route add`, mapping to `when.estimated_cost_gt` / `estimated_cost_lt` in the `NewRoute` body (mirrors `--when-prompt-contains` / `--when-tag`).

## Testing
- **`tt_routing`:** matcher matrix — `estimated_cost_gt` matches when est > threshold (strict), `_lt` when est < threshold; AND-ed with `model_in`; absent fields ignore cost. (Add cost args to existing tests as `0.0`.)
- **`tt-core` integration:** a route `when {estimated_cost_gt: X} → cheaper-model` reroutes an expensive request and passes through a cheap one (two requests, different sizes / max_tokens, against a model-aware mock pricing).
- **`tt-plan-core`:** a replay where a high-`baseline_cost_usd` request matches `estimated_cost_gt` and reroutes (rerouted + savings), and a low-cost request does not; determinism snapshot unaffected (new `Option` fields skip-serialize when `None`).
- **`tt-cli`:** `--when-cost-gt/--when-cost-lt` map into `when` (present when set, omitted when not).
- `cargo clippy --workspace --all-targets -D warnings`; `cargo fmt`.

## Out of Scope (V3d-2b and beyond)
- **`RouteAction.max_cost_usd`** + the post-rewrite **402** block gate + the Plan "would-block N requests" caveat — **V3d-2b**.
- Making `DEFAULT_OUTPUT_TOKENS_ESTIMATE` configurable; org-global ceiling; historical-ratio output estimation.
- Dashboard exposure of the cost condition (cloud follow-up).
