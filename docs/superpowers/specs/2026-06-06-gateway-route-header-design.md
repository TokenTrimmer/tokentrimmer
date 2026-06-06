# Honor `X-TokenTrimmer-Route` + emit `X-TokenTrimmer-Route-Matched` (F8) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** F8. Honors the `X-TokenTrimmer-Route` request header (force a named route) and emits the `X-TokenTrimmer-Route-Matched` response header, both documented "Planned".

## Goal

- **Request:** `X-TokenTrimmer-Route: <name>` forces the named route to apply on `/v1/chat/completions`, **ignoring its `when` conditions** (hard override). Unknown name → `400`.
- **Response:** `X-TokenTrimmer-Route-Matched: <name>` is set on every routed response (forced **or** condition-matched).

Downstream guards are unchanged: the route's `target_model` still rewrites `req.model`; its `max_cost_usd`, `disable_cache`, `fallbacks`, the capability guard, and the F6 provider-pin / F7 cache-header all still apply.

## Background (current behavior)

- `apply_routing(state, ctx, req) -> Option<RouteMatch>` (`chat.rs:1880`) fetches the org's cached `RoutingEngine` (`store.engine_for(org)`), estimates input tokens + cost, calls `engine.evaluate_with_cost(...)` (first enabled route by priority whose `when` matches), runs a **capability guard** on the target model (`chat.rs:1930-1950` — on failure returns `None`, request passes through unrouted), rewrites `req.model` in place (`chat.rs:1952`), and returns a `RouteMatch`.
- `RouteMatch` (`chat.rs:1862`): `route_id`, `fallbacks`, `disable_cache`, `max_cost_usd`, `input_tokens_estimate`.
- `RoutingEngine` (`routing/src/lib.rs:124`) holds priority-sorted routes; `evaluate`/`evaluate_with_cost` are the only selectors — **no by-name lookup**.
- The handler call site: `let route_match = apply_routing(&state, &ctx, &mut req).await;` (`chat.rs:544`); `embeddings.rs:153` calls it with a synthetic request.
- `attach_cost_headers` already emits `x-tokentrimmer-model-used` (the served model) on every response.
- Docs: `X-TokenTrimmer-Route` request row (`docs/04-gateway-api-reference.md:408`, "Planned"); `X-TokenTrimmer-Route-Matched` response row (`:426`, "Planned (not yet emitted)").

## Architecture

### 1. `RoutingEngine::find_by_name` (`crates/routing/src/lib.rs`)
```rust
/// Find an enabled route by exact name (case-sensitive), bypassing condition
/// evaluation — used to honor a forced-route request header.
pub fn find_by_name(&self, name: &str) -> Option<&Route> {
    self.routes.iter().find(|r| r.enabled && r.name == name)
}
```

### 2. Header reader (`chat.rs`, `pub(crate)`)
```rust
/// `X-TokenTrimmer-Route` — an exact route name to force (case-sensitive).
pub(crate) fn route_override_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tokentrimmer-route")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
```

### 3. `RouteMatch` gains the route name
Add `pub(crate) route_name: String` to `RouteMatch`. Populate from the applied route's `name`.

### 4. `apply_routing` — add `forced_route` + return `ApiResult`
Signature → `apply_routing(state, ctx, req, forced_route: Option<&str>) -> ApiResult<Option<RouteMatch>>`.

- A forced route that cannot be honored is a `400`. Helper at the early-out points:
  ```rust
  // no routing store, or no resolvable org → a forced route can't exist.
  let Some(store) = state.routing_store.as_ref() else {
      return forced_miss(forced_route);
  };
  if ctx.org_id == Uuid::nil() {
      return forced_miss(forced_route);
  }
  ```
  where
  ```rust
  fn forced_miss(forced: Option<&str>) -> ApiResult<Option<RouteMatch>> {
      match forced {
          Some(name) => Err(ApiError::InvalidRequest(format!("unknown route: {name}"))),
          None => Ok(None),
      }
  }
  ```
- On a backend engine error, keep the existing "never fail user traffic" pass-through: `return Ok(None);` (forced or not — a transient backend failure shouldn't 400 the caller).
- Selection (replaces the `evaluate_with_cost(...)?` line):
  ```rust
  let m: &Route = match forced_route {
      Some(name) => engine
          .find_by_name(name)
          .ok_or_else(|| ApiError::InvalidRequest(format!("unknown route: {name}")))?,
      None => match engine.evaluate_with_cost(req, ctx, input_tokens, estimated_cost_usd) {
          Some(r) => r,
          None => return Ok(None),
      },
  };
  ```
  (`input_tokens`/`estimated_cost_usd` are still computed first — `input_tokens` feeds `RouteMatch` + the capability guard.)
- The capability guard (`chat.rs:1930-1950`) is unchanged and runs for forced routes too — on failure `return Ok(None)` (request passes through on its original model; the forced route is not applied — no `route-matched` header).
- Build `RouteMatch { route_id: m.id, route_name: m.name.clone(), fallbacks, disable_cache, max_cost_usd, input_tokens_estimate }` and `return Ok(Some(...))`.

### 5. Response header wrapper (`chat.rs`)
```rust
/// Stamp `X-TokenTrimmer-Route-Matched` with the applied route's name (no-op when
/// `name` is `None` or not header-safe).
fn with_route_matched(mut resp: Response, name: Option<&str>) -> Response {
    if let Some(name) = name {
        if let Ok(v) = name.parse() {
            resp.headers_mut()
                .insert("x-tokentrimmer-route-matched", v);
        }
    }
    resp
}
```

### 6. Handler wiring (`chat.rs`)
- Near the other header reads: `let forced_route = route_override_from_header(&headers);`
- Routing call (line 544): `let route_match = apply_routing(&state, &ctx, &mut req, forced_route.as_deref()).await?;`
- Capture the name before `route_match` is consumed (alongside `matched_route_id` etc., ~545): `let route_matched_name = route_match.as_ref().map(|m| m.route_name.clone());`
- Wrap each success-response exit with `with_route_matched(resp, route_matched_name.as_deref())`:
  - `718` (fake-stream L1 hit): `Ok(with_route_matched(sse::stream_response(fake, &provider, trace_id, None), route_matched_name.as_deref()))`
  - `887` (live stream): wrap the `sse::stream_response(...)` result.
  - `934` (negative-cache hit): wrap `resp`.
  - `972` & `1066` (L1 hits): `return Ok(with_route_matched(build_hit_l1_response(entry, trace_id), route_matched_name.as_deref()));`
  - `1013` (L2 hit): `return Ok(with_route_matched(build_hit_l2_response(entry, similarity, trace_id)?, route_matched_name.as_deref()));`
  - `1392` (dispatched non-stream): the `http_response` is a mutable local — set the header inline before `Ok(http_response)`:
    ```rust
    if let Some(name) = route_matched_name.as_deref() {
        if let Ok(v) = name.parse() {
            http_response.headers_mut().insert("x-tokentrimmer-route-matched", v);
        }
    }
    ```
  (The `tt_test_*` sandbox exit at `482` runs before routing — no header, unchanged.)

### 7. `embeddings.rs`
- `apply_routing(&state, &ctx, &mut synth, None).await?` (route header is chat-only; adapts to the new `ApiResult` return). No response-header emission for embeddings.

## Precedence / interactions
Forcing a route only changes **which** route is selected; everything after (provider re-resolve, cost ceiling, `disable_cache` → `cache_behavior`, fallbacks, F6 provider pin, F7 cache header) is identical to a condition-matched route. Hard override ignores only the `when` conditions; the capability guard and `max_cost_usd` ceiling still apply.

## Testing

Integration (`crates/core/tests/route_header.rs`, mirroring `disable_cache.rs` — auth key + routing store; a `CountingProvider` serving two models so the routed model is observable via `x-tokentrimmer-model-used`):
- **`forced_route_applies_ignoring_conditions`**: a route `force-me` whose `when` does NOT match the request (e.g. `tag_equals: "other"`), `target_model: m2`. Request for `m1` with `X-TokenTrimmer-Route: force-me` → `x-tokentrimmer-model-used == m2` and `x-tokentrimmer-route-matched == "force-me"`.
- **`forced_route_overrides_normal_match`**: two routes — `auto` (matches the request, `target_model: m2`, higher priority) and `manual` (`when` doesn't match, `target_model: m3`). Force `manual` → served model `m3`, `route-matched == "manual"`.
- **`unknown_forced_route_is_400`**: `X-TokenTrimmer-Route: nope` → `400`.
- **`condition_matched_route_emits_route_matched`**: a normally-matching route, no header → `route-matched` == the route's name (verifies the response header for the non-forced path).
- **`no_route_no_header`**: no routes configured (or none match), no header → no `x-tokentrimmer-route-matched` header.

Unit:
- `route_override_from_header` (`chat.rs`): present/trim/case-preserved, empty/absent → None.
- `RoutingEngine::find_by_name` (`routing/src/lib.rs`): finds an enabled route by exact name; returns None for unknown / disabled.

Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p tt-core -p tt-routing`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-core --no-deps`.

## Docs
- `docs/04-gateway-api-reference.md:408` `X-TokenTrimmer-Route` → "Honored" (+ note: forces a named route, ignoring its conditions; unknown name → 400; chat completions only).
- `:426` `X-TokenTrimmer-Route-Matched` → "Emitted" (the applied route's name, on forced and condition-matched responses).

## Out of scope
- Embeddings (the route header is chat-only; no `route-matched` on embeddings responses).
- The `force_cache_layer` route field (still unwired).
- Forcing a *disabled* route (only enabled routes are loaded into the engine; a disabled name → 400 unknown).
