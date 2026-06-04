# Design: V3b-1 — Privacy cache opt-out (`disable_cache` route action)

_Date: 2026-06-04 · Status: approved design, pre-implementation · Repo: `public` (gateway `tt-core`, `tt-routing`, `tt-plan-core`, `tt` CLI)_

> First half of **V3b — Privacy → local** (V3 roadmap). Ships the universal
> privacy primitive: a route can mark matching requests so they are **never read
> from or written to TokenTrimmer's cache**. Works on hosted *and* self-hosted
> gateways. Routing sensitive requests to a **local** model is the self-host-only
> follow-on **V3b-2**.

## Problem

A user wants some requests (legal/sensitive/private) to leave no trace in
TokenTrimmer's semantic cache — nothing of theirs persisted in the shared L2 store.
Today a route can rewrite the model or set fallbacks, but it cannot say "do not
cache this." The cache decision is computed per request from structural
eligibility + `tt_extras.cache`, with no route-driven override.

## Current state (verified 2026-06-04)

- **`tt_routing::RouteAction`** (`crates/routing/src/lib.rs:79-97`): `target_model:
  String`, `fallbacks: Vec<String>` (`skip_serializing_if = Vec::is_empty`),
  `force_cache_layer: Option<String>` (present but **unwired** at runtime — a
  Plan round-trip carry-through). `tt_plan_core::types::RouteAction` mirrors it.
- **Sensitive signal exists:** `X-TokenTrimmer-Tag: <v>` → `RequestContext.tag`
  (`chat.rs:379-382`); the routing engine already matches it via
  `RouteConditions.tag_equals` (`routing/src/lib.rs`). So a privacy route needs
  **no new condition** — `tag_equals: "sensitive"` already works.
- **Cache behavior:** `CacheBehavior { do_lookup, do_insert, ttl_secs }`
  (`chat.rs:238-286`), resolved from structural eligibility + `tt_extras.cache`.
  L1/L2 **lookup** is gated on `do_lookup` (`chat.rs:685,717`); **insert** on
  `do_insert` (`chat.rs:984,1041`). L2 additionally requires a paid tier.
- **Ordering:** `apply_routing` (`chat.rs:~393`) runs **before** the cache lookup
  (`:685+`), returning `RouteMatch { route_id, fallbacks }`. So the matched route
  is known before the cache is consulted — a clean override point.
- **CLI:** `tt route add` (`crates/cli/src/route/mod.rs::build_new_route`) builds
  the `when`/`then` JSON from flags; no `--disable-cache` and no `--when-tag` yet.

## Goals / non-goals

**Goals:** a `disable_cache: bool` action on `RouteAction` (+ plan-core mirror) that,
when a matched route sets it, forces `do_lookup = do_insert = false` for that
request; `tt route add --disable-cache` + `--when-tag <tag>` so privacy routes are
creatable from the CLI; tests proving a matched privacy route bypasses L1/L2.

**Non-goals:** routing to a **local** model (V3b-2); dashboard exposure of
`disable_cache` (small cloud follow-up, mirrors V3a-3); a per-org "always private"
org setting (ADR-017); making `target_model` optional (a cache-only route pins to
the same model, e.g. `--always gpt-4o --disable-cache`); wiring `force_cache_layer`.

## Design

### 1. `RouteAction.disable_cache` (`tt-routing` + `tt-plan-core`)

Add to **`tt_routing::RouteAction`**:
```rust
/// When true, a request this route matches skips L1+L2 entirely (no lookup,
/// no insert). Used for privacy/sensitive traffic that must not persist in the
/// shared cache. Default false.
#[serde(default, skip_serializing_if = "std::ops::Not::not")]
pub disable_cache: bool,
```
Mirror the same field on `tt_plan_core::types::RouteAction` for lockstep + lossless
Plan round-trip (and so Plan's cache projection can later honor it — noted, not
wired here).

### 2. Surface it from routing (`tt-routing` + `chat.rs`)

`apply_routing`'s `RouteMatch` gains `disable_cache: bool`, populated from the
matched route's `then.disable_cache`. (RouteMatch is gateway-internal in `chat.rs`;
extend its struct + the line that builds it.)

### 3. Honor it in the cache decision (`chat.rs`)

`CacheBehavior` is computed before `apply_routing`; immediately **after**
`apply_routing` (and before the cache lookup at `:685`), override:
```rust
if route_match.as_ref().is_some_and(|m| m.disable_cache) {
    cache_behavior.do_lookup = false;
    cache_behavior.do_insert = false;
}
```
(`cache_behavior` is made `mut`.) This guarantees no L1/L2 read or write for the
request, regardless of tier or structural eligibility.

### 4. CLI (`tt route`)

Extend `build_new_route` + the `Add` clap args:
- `--disable-cache` → `then.disable_cache = true`.
- `--when-tag <tag>` → `when.tag_equals = <tag>` (exposes the existing condition so
  privacy routes are creatable without the raw-JSON path).

Example: `tt route add --when-tag sensitive --always gpt-4o --disable-cache`.

## Data flow

`X-TokenTrimmer-Tag: sensitive` → `ctx.tag = "sensitive"` → `apply_routing` matches a
route with `when.tag_equals = "sensitive"` → `RouteMatch.disable_cache = true` →
`cache_behavior` forced off → the request dispatches to the provider with **no**
L1/L2 lookup or insert; nothing persists.

## Error handling

`disable_cache` is a safe boolean — no validation needed (no same-provider /
capability implication). A route with `disable_cache: true` and no special tag
simply disables cache for everything it matches (the user's choice). Existing
same-provider/capability validation on the rest of the action is unchanged.

## Testing (TDD; scoped `cargo test -p <crate>`)

- `tt-routing`: `RouteAction` serde — `disable_cache` defaults false, omitted when
  false, round-trips when true.
- `tt-plan-core`: mirror field parses + defaults.
- `tt-core` integration (mirror `route_rewrite.rs` harness, add L1): two identical
  requests through a `tag_equals`+`disable_cache` route → provider called **twice**,
  second response is **not** `hit-l1`; a control route without `disable_cache` →
  second request **is** `hit-l1`.
- `tt-cli`: `build_new_route` maps `--disable-cache` → `then.disable_cache=true` and
  `--when-tag X` → `when.tag_equals="X"`.

## Success criteria

- A route matching `tag_equals: "sensitive"` with `disable_cache: true` causes
  matched requests to skip L1+L2 (verified: no `hit-l1`, provider called every time,
  no insert).
- `tt route add --when-tag sensitive --always <model> --disable-cache` creates it.
- `disable_cache` defaults false and is omitted from JSON when false (existing routes
  + tests unchanged); plan-core mirror stays in lockstep.

## Out of scope (restated)

Local-model routing (V3b-2); dashboard `disable_cache` control (cloud follow-up);
per-org privacy org-setting; optional/passthrough `target_model`; Plan projection
honoring `disable_cache`; `force_cache_layer` wiring.
