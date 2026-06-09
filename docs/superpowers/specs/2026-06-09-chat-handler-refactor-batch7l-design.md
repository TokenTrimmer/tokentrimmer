# chat.rs handler refactor (batch 7l) — Design

**Status:** approved (2026-06-09)
**Slice:** Audit-remediation, public repo, `crates/core/src/routes/chat.rs` (+ no other files). One dx/low. **Behavior-preserving** — the existing `tt-core` cache integration suite is the safety net; no new tests.

## Finding (checklist L150)
The `POST /v1/chat/completions` handler is a ~1060-line monolith. Two concrete issues: (a) the streaming branch hand-rolls a per-message text concatenation for the input-token estimate (chat.rs:793–836) instead of the shared `tt_shared::message_text_for_estimation` used at every other site (679, 1209, 2151) — they can diverge near token boundaries; (b) the module docstring (lines 1–16) is stale ("real auth lands in Week 7", "L1 exact-match is W7", a "Deferred" list — all shipped).

## Scope (conservative, chosen during brainstorming)
Behavior-preserving cleanup only. Dispatch (3c), cost compute (3d), and the cache inserts (3e/3f) stay inline. Response-builders (`build_hit_l1_response`, `build_hit_l2_response`) and `insert_into_l2` are already extracted and are not touched.

## Part 1 — streaming token-estimate uses the shared helper
Replace the inline ~45-line concat (chat.rs:793–836) with:
```rust
let estimated_input_tokens =
    tt_tokenize::estimate_tokens(provider.id(), &tt_shared::message_text_for_estimation(&req)) as i32;
```
Verified equivalent: `message_text_for_estimation` extracts the same per-message-type text (`User`/`System`/`Assistant`/`Tool`, `Text` + `Parts` text-only) and `join("")`s identically. This removes the duplication AND the divergence hazard (the streaming estimate now matches routing/preview).

## Part 2 — refresh the module docstring
Rewrite lines 1–16 to describe the actual current pipeline (auth, routing, L1/L2 cache, telemetry are all live). Remove the "Week N" futures and the "Deferred" block.

## Part 3 — extract the early-returning cache *lookup* branches
Three `async` helpers, each moving its block **verbatim** (same metrics counters, `spawn_request_log`, `bump_hit_count`, `with_route_matched` wrapping). `None` = fall through to the next step.

- `async fn try_negative_cache_hit(l1: &L1Layer, l1_key: &str, route_matched_name: Option<&str>) -> Option<Response>` — the 3a-neg negative-cache lookup (returns the cached deterministic-4xx error response on hit).
- `async fn try_l1_hit(state, ctx, l1_key, trace_id, request_started, matched_route_id, route_matched_name) -> Option<Response>` — the 3a positive L1 lookup.
- `async fn try_l2_hit(state, ctx, req, trace_id, request_started, matched_route_id, route_matched_name) -> Option<ApiResult<Response>>` — the 3b L2 semantic lookup. Returns `Option<ApiResult<…>>` so the existing `build_hit_l2_response(...)?` error propagation is preserved (`Some(Err)` = hit-but-deserialize-failed → handler returns the error; `Some(Ok)` = hit; `None` = miss).

`l1_key` is computed once in the handler and passed in (the later 3e insert reuses it, so it stays a handler local). The `cache_behavior.do_lookup` / `l2_allowed` gates remain at the handler call sites (the helper is only invoked when the gate passes), keeping each helper's precondition obvious. Exact parameter types are pinned during planning by reading the live locals; parameter lists are wide-but-explicit (no context struct — that was the rejected "aggressive" option).

Resulting handler shape:
```rust
if let Some(r) = try_negative_cache_hit(...).await { return Ok(with_route_matched? ...); }
if let Some(r) = try_l1_hit(...).await { return Ok(r); }
if let Some(r) = try_l2_hit(...).await { return r; }
```
(The exact wrapping — whether `with_route_matched` is applied inside the helper or at the call site — is kept identical to today; planning will pin it by moving the code verbatim.)

## Verification
- `cargo test -p tt-core` — the full suite (~166 tests incl. `l1_cache_hit`, `negative_cache`, `disable_cache`, `cache_header`, `route_rewrite`, `single_flight_coalesce`) must pass unchanged. This is the behavior-preservation contract.
- `cargo clippy -p tt-core --all-targets` clean; `cargo fmt --all --check` clean.
- No new tests (pure refactor). Checklist L150 flipped to DONE.

## Out of scope
- Extracting dispatch/cost/insert/orchestration or introducing a handler-context struct (the "aggressive" option).
- Any behavior change.
