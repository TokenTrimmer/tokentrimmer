# V3d-2b — Per-Request Cost Ceiling Design

**Status:** approved (design — part of the approved V3d-2 "cost-aware routing" design)
**Date:** 2026-06-05
**Slice:** V3d-2b (second of two V3d-2 sub-slices). Completes **V3d-2** → completes **V3**.
**Depends on:** V3d-2a (#20) merged to `main` — reuses its cost-estimation plumbing (`DEFAULT_OUTPUT_TOKENS_ESTIMATE`, the `input_tokens × input_rate + (max_tokens||default) × output_rate` estimate in `apply_routing`).

## Goal

The "block" half of "reroute then block": a route can carry a hard per-request ceiling `max_cost_usd`. After the route's rewrite, if the **rerouted** model's estimated cost still exceeds the ceiling, the gateway rejects the request with a clear **402** instead of dispatching. Combined with V3d-2a's cost condition, one route expresses "downgrade expensive requests, and hard-cap the result": `when estimated_cost_gt $0.05 → gpt-4o-mini, max_cost_usd $0.10`.

## Background

V3d-2a made cost a routing signal (reroute). The existing tier-driven `BudgetEnforcer` (monthly accumulated spend) explicitly *cannot* gate a single request pre-flight. V3d-2b adds exactly that: a per-request, user-configured, pre-flight ceiling — enforced on the same worst-case estimate V3d-2a introduced (so `max_tokens`-bounded requests get a true upper-bound guarantee).

## Design

### 1. `tt_routing` — the ceiling field
`crates/routing/src/lib.rs` `RouteAction`:
```rust
/// Hard per-request ceiling (USD). After this route's rewrite, if the rerouted
/// model's estimated cost still exceeds this, the gateway rejects the request
/// (402) instead of dispatching. `None` = no ceiling.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub max_cost_usd: Option<f64>,
```

### 2. Shared estimate helper
Extract V3d-2a's inline estimate into a small pure helper (so the pre-rewrite condition check and the post-rewrite ceiling check share one definition):
```rust
fn estimate_cost_usd(pricing: &ModelPricing, input_tokens: u32, max_tokens: Option<u32>) -> f64 {
    let output_est = max_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS_ESTIMATE);
    (f64::from(input_tokens) * pricing.input_per_million
        + f64::from(output_est) * pricing.output_per_million) / 1_000_000.0
}
```
`apply_routing` uses it for the pre-rewrite estimate (refactor of V3d-2a, no behavior change).

### 3. Gateway — the post-rewrite gate
- `RouteMatch` (returned by `apply_routing`) gains `max_cost_usd: Option<f64>` and `input_tokens_estimate: u32` (so the handler can re-estimate on the routed model without recomputing the token count).
- In `chat.rs::handler`, after the matched route's provider is re-resolved (the V3d-1 block, ~`chat.rs:407-430`), when `max_cost_usd` is set:
  ```rust
  if let Some(ceiling) = route_max_cost_usd {
      let routed_cost = provider
          .pricing(&req.model)
          .map(|pr| estimate_cost_usd(&pr, route_input_tokens, req.max_tokens))
          .unwrap_or(0.0); // unknown pricing → can't exceed → permissive (no block)
      if routed_cost > ceiling {
          return Err(ApiError::CostLimitExceeded { estimated_usd: routed_cost, ceiling_usd: ceiling });
      }
  }
  ```
  Runs before dispatch (and before the cache lookup is irrelevant — a blocked request is never served). Permissive on unknown pricing (mirrors V3d-2a / other unknown-data stances).

### 4. The 402 error
`crates/core/src/error.rs`: add
```rust
#[error("estimated cost ${estimated_usd:.4} exceeds the ${ceiling_usd:.4} per-request ceiling")]
CostLimitExceeded { estimated_usd: f64, ceiling_usd: f64 },
```
`into_response` arm → `StatusCode::PAYMENT_REQUIRED` (402), type `billing_error`, code `cost_limit_exceeded`, message naming both numbers. (Distinct code from the subscription `PaymentRequired`'s `subscription_required`.) Add arms to the two other `ApiError` match sites in `chat.rs` (`is_deterministic_client_error` → config-dependent, **don't** negative-cache → `false`; `error_status_code` → `402`).

### 5. CLI
`tt route add --max-cost <USD>` → `then.max_cost_usd`. `AddArgs.max_cost: Option<f64>`, mapped in `build_new_route`; clap arg + dispatch in `main.rs`.

### 6. Plan honesty — "would block" caveat
`crates/plan-core/src/types.rs` `RouteAction`: mirror `max_cost_usd: Option<f64>` (round-trip parity).
`crates/plan-core/src/replay.rs` `project_requests`: when a matched route has `max_cost_usd` and the request's **projected** (routed) cost exceeds it, the request would be **rejected** at runtime. Represent honestly:
- Project its cost as **unchanged** (`req.cost_usd`) — never claim a blocked request as a saving.
- Count it in a local `would_block` counter; `build_caveats` emits "`N request(s) would be rejected by a max_cost_usd ceiling`".
- No new `Aggregates` field (caveats already vary) → determinism snapshot stays byte-identical for fixtures without `max_cost_usd`.

## Testing
- **`tt_routing`:** `RouteAction` serde round-trips `max_cost_usd` (present when Some, omitted when None).
- **`tt-core` integration (`cost_routing.rs`):**
  - "reroute then block": route `when estimated_cost_gt 0.02 → gpt-4o-mini, max_cost_usd 0.10`; an expensive request that fits after downgrade → **200** (served gpt-4o-mini); a request whose downgraded cost still exceeds the ceiling (large `max_tokens`) → **402** `cost_limit_exceeded`.
  - A `max_cost_usd` with no rewrite (catch-all `always`-style route) blocks an over-ceiling request → 402.
  - Permissive: unknown-pricing target → no block (200).
- **`tt-cli`:** `--max-cost` maps to `then.max_cost_usd`.
- **`tt-plan-core`:** a replay where a matched route's projected cost exceeds `max_cost_usd` → that request is projected unchanged (no savings) + a "would be rejected" caveat; `snapshot_canned_replay` + determinism byte-identical.
- `cargo clippy --workspace --all-targets -D warnings`; `cargo fmt`.

## Out of Scope / Follow-ups
- Org-global ceiling independent of routes (a settings store) — per-route `max_cost_usd` + a catch-all route covers it for now.
- Making `DEFAULT_OUTPUT_TOKENS_ESTIMATE` configurable; historical-ratio output estimation.
- Dashboard exposure of the cost condition + ceiling (cloud follow-up).
- Modeling a blocked request's *downstream* effect (caller retry) in Plan — out of scope; the caveat is the honest surface.
