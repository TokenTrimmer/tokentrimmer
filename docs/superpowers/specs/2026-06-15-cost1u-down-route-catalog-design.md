# COST-1(U) — Opt-in Down-Route Catalog

**Status:** approved design (2026-06-15) · **Finding:** `COST-1(U)` (COMPREHENSIVE_REVIEW_2026-06-15.md) · **Repo:** public OSS core

## Problem

A customer who points their app at the gateway gets **zero routing savings** until they hand-author routing rules. Model right-sizing is the single largest realized-savings lever, but `RouteAction.target_model` is `None` for every unrouted request and the only auto-suggesters are inert on the live path (`mcp/find_route_for` ignores telemetry; `preview/route_suggestions::suggest` is preview-only). The drop-in funnel therefore delivers no savings — the headline "cut your LLM cost" promise is unrealized by default.

## Goal

When a customer **opts in**, the gateway delivers model-right-sizing savings with **no hand-authored rules** — conservatively, transparently, and **self-reverting** if quality regresses. This is mostly *assembling existing seams*; one new safety condition is the only genuinely new logic.

## Non-goals (v1)

- Derived/catalog-computed sibling selection (v1 is a curated static table).
- Cross-provider down-routes (v1 is same-provider only — the customer's flagship key already works for its mini sibling).
- Down-routing pure-reasoning flagships (`o`-series) — excluded from v1.
- Canary auto-ramp of catalog routes.
- Per-request task-class classification on the live path (the ingress `TaskClass` is effectively single-variant today — see `COST-5`).
- The hosted **cloud dashboard** toggle (a cloud-repo follow-up reusing the shared builder below).

## Reused existing machinery (no new logic)

- **Candidates:** `crates/preview/src/route_suggestions.rs` already encodes cheaper-model candidates per task class.
- **Self-revert safety:** `crates/core/src/route_autopause.rs` + `crates/core/src/quality_sample.rs` — a route with `auto_pause: true` + `pause_floor_pass_rate` is sampled by a paired LLM judge on ~2% of down-routed traffic; if the paired pass-rate drops below the floor over ≥`pause_min_verdicts`, the route **sticky-pauses → reverts to the original model** (fail-safe). Resume is operator-gated with a post-resume watermark.
- **Savings ledger:** `crates/core/src/route_savings.rs` (`ROUTE_SAVINGS_SQL`) nets `gross - judge_tax - shadow_tax` per route (can be negative — honest), and feeds the signed attestation. A catalog route lands in the same `request_logs`/`quality_verdicts` tables and is reconciled identically.
- **Apply seam:** `crates/core/src/routes/chat.rs::apply_routing` (~5507–5728) already rewrites `req.model` from `RouteAction.target_model`, honors `paused`, and passes cost/latency **signals** into `engine.evaluate_with_signals(...)`.

## Design

### 1. Shared catalog data (curated, same-provider)
New `catalog` module in `crates/routing` with a hand-maintained flagship→sibling table and a `catalog_routes() -> Vec<NewRoute>` builder. v1 mappings (chat flagships only):

| Provider | Source models | Target (cheaper sibling) |
|---|---|---|
| OpenAI | `gpt-4o*`, `gpt-4.1*` | `gpt-4o-mini` |
| Anthropic | `claude-opus-*`, `claude-sonnet-*` / `claude-3.5-sonnet` | `claude-haiku-4-5` |
| Gemini | `gemini-*-pro` | `gemini-flash` |

Each entry records: source model patterns, target model, and capabilities to preserve. One shared module so the CLI and (later) the cloud dashboard build **identical** routes. The exact source-model lists are validated against the embedded `ModelCatalog` at build/test time (targets exist, same provider, target is cheaper).

### 2. Activation (opt-in; no new gateway state)
New CLI subcommand `tt route catalog <enable|disable|status>`:
- **`enable`** — build the catalog `NewRoute`s from the shared module and `POST` each to the existing `/v1/routes` CRUD, tagged with a small additive marker `managed_by: "catalog"`. Idempotent (re-enable does not duplicate).
- **`disable`** — list routes and `DELETE` only those with `managed_by == "catalog"` (never user-authored routes).
- **`status`** — list catalog routes with active/paused state, pass-rate, and netted savings.

"Catalog enabled" is therefore simply "catalog routes exist for this org" — **no new gateway endpoint or org-settings store**. The hosted dashboard toggle (cloud repo) will reuse the same shared builder.

### 3. Materialized down-routes
Each catalog route:
- `when`: `{ model_in: [<flagship sources>], not_reasoning_class: true }`
- `then`: `{ target_model: <mini>, auto_pause: true, pause_floor_pass_rate: 0.92, pause_min_verdicts: 20 }`
- low `priority` (user-authored routes always win on tie-break), caching left on, `disable_cache: false`.

### 4. New safety condition: `not_reasoning_class` (the one new piece)
Add `RouteConditions.not_reasoning_class: bool` (`#[serde(default, skip_serializing_if = "is_false")]` → wire back-compat). Because `reasoning_class.rs` lives in `crates/core` and the routing engine is in `crates/routing` (and `core` depends on `routing`, not vice-versa), the classification is computed in `core` and passed as a **signal**, mirroring the existing cost/latency signals:
- `apply_routing` computes `reasoning_class::classify(<request text>)` → an `is_reasoning_class: bool` signal.
- `evaluate_with_signals(...)` gains that signal; the `not_reasoning_class` condition matches only when `is_reasoning_class == false`.
- Effect: a Math/Code/Legal/Medical request on a flagship **falls through to the original model**; everything else down-routes. `auto_pause` is the per-route backstop for content the guard doesn't catch.

### 5. Safety / savings / transparency (reused)
- **Self-revert:** `auto_pause: true` + floor 0.92 → existing paired-judge auto-pause reverts a regressing route to the flagship.
- **Savings:** netted via `route_savings.rs` (judge tax itemized); flows into the signed attestation like any route.
- **Transparency:** catalog routes are normal routes — listable (`tt route list`), pausable, deletable; the customer sees and controls exactly what is happening.

## Components (isolation)

| Unit | Location | Responsibility | Depends on |
|---|---|---|---|
| `catalog` module | `crates/routing/src/catalog.rs` | curated mapping table + `catalog_routes() -> Vec<NewRoute>` builder + `MANAGED_BY_CATALOG` marker const | `ModelCatalog` (validation), `Route`/`NewRoute` types |
| `not_reasoning_class` condition | `crates/routing/src/lib.rs` (`RouteConditions`) + engine eval | new condition + signal check | the new `is_reasoning_class` signal |
| reasoning-class signal | `crates/core/src/routes/chat.rs::apply_routing` + `evaluate_with_signals` | compute `reasoning_class::classify` and thread it as a signal | `crates/core/src/reasoning_class.rs` |
| `tt route catalog` CLI | `crates/cli/src/route/` | enable/disable/status via `/v1/routes` CRUD + the shared builder | `catalog` module, gateway client |
| `managed_by` marker | route model + store | distinguish catalog routes from user routes | additive serde field |

## Error handling / edge cases

- **Marker is additive:** `managed_by: Option<String>` (serde `default`), so existing routes and old JSON deserialize unchanged.
- **`disable` never touches user routes:** deletes strictly `managed_by == "catalog"`.
- **Re-enable idempotency:** `enable` skips a catalog route that already exists (keyed by source→target).
- **Target not in the org's accessible models / no key:** same-provider mapping means the flagship key covers the mini; if a target is absent from the catalog, that mapping entry is skipped (logged), not created.
- **Disabled = zero behavior change:** with the catalog not enabled, no catalog routes exist. The `is_reasoning_class` signal is computed cheaply (deterministic substring match, no LLM call) but is only ever *consulted* by a route that sets `not_reasoning_class: true`; conditions default `false`, so no existing route's matching changes. To avoid even the cheap classification cost when unused, `apply_routing` computes the signal lazily only when the org has ≥1 route with `not_reasoning_class` set.

## Testing

- **Catalog validity:** every target exists in `ModelCatalog`, is the same provider as its sources, and is cheaper than each source (price check).
- **Enable/disable idempotency:** enable twice → identical route set; disable removes only `managed_by == "catalog"`, leaving user routes intact.
- **`apply_routing` behavior:** a flagship request → rewritten to the mini; a reasoning-class request (`"prove the theorem…"`, code, legal, medical) on the same flagship is **not** down-routed (falls through); a request on a non-flagship model is untouched.
- **Auto-pause reuse:** a catalog route with simulated sub-floor verdicts sticky-pauses and reverts to the flagship.
- **Wire back-compat:** `RouteConditions` without `not_reasoning_class` and routes without `managed_by` round-trip byte-identically; the `route_action`/conditions lockstep guard (plan-core) stays green.
- **Determinism:** catalog disabled = no-op; no plan/attestation golden shifts.

## Rollout

1. OSS core: this spec's components, behind the opt-in CLI. Default off.
2. Cloud follow-up (separate cloud-repo PR): a dashboard toggle that calls the same shared `catalog_routes()` builder; surface catalog savings + pass-rate on the routes/savings pages.
