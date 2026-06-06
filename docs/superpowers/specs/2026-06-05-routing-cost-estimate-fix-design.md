# Routing Cost-Estimate Undercount Fix Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** Follow-on F1. Fixes a pre-existing billing/routing-correctness bug surfaced by the #41 review.

## Goal

`apply_routing` (crates/core/src/routes/chat.rs) estimates the input tokens used by
**cost-based route conditions** (`estimated_cost_gt` / `estimated_cost_lt`) and by
the stored `RouteMatch.input_tokens_estimate` (which feeds the route `max_cost_usd`
→ 402 ceiling) from `last_user_message_text(req)` — **only the trailing user
message**. Make it use the **full prompt**, aligning with every other estimate in
the system.

## Why this is wrong (verified)

The full-prompt estimate is canonical everywhere except this one path:
- `/v1/preview` uses `concat_message_text(messages)` (all messages) —
  `crates/preview/src/token_estimator.rs:21`; its module doc states "`/v1/preview`,
  live dispatch, and routing all agree on the estimate."
- The **capability guard inside `apply_routing` itself** uses
  `tt_shared::message_text_for_estimation(req)` (full prompt) — chat.rs:1827-1830.
- The live streaming + failover dispatch paths use the full prompt
  (chat.rs ~605, ~996).
- The #41 `X-TokenTrimmer-Cost-Limit-Usd` fix uses `message_text_for_estimation`.

So `apply_routing`'s cost path is the lone outlier: a multi-turn conversation or
large system prompt undercounts input tokens, so cost-`gt` route conditions
under-fire and the route ceiling under-rejects. The comment at chat.rs:1797
claiming it "uses the SAME count `/v1/preview` reports" is factually false.

## The fix (one focused change, `apply_routing`)

1. Replace the `input_tokens` computation (chat.rs:1803-1805):
   ```rust
   let input_tokens = last_user_message_text(req)
       .map(|s| tt_tokenize::estimate_tokens(provider_id, s))
       .unwrap_or(0);
   ```
   with a full-prompt estimate computed once:
   ```rust
   // Full-prompt token estimate — the SAME count /v1/preview, live dispatch, and
   // the capability guard below all use (system + every turn). Counting only the
   // last user message undercounts multi-turn / large-system-prompt requests,
   // under-firing cost conditions and the route `max_cost_usd` ceiling.
   let input_tokens = {
       let combined = tt_shared::message_text_for_estimation(req);
       tt_tokenize::estimate_tokens(provider_id, &combined)
   };
   ```
2. Update the now-inaccurate comment block above it (chat.rs:1795-1800) so it no
   longer claims to match `/v1/preview` via the last-user heuristic — the new code
   genuinely does match it.
3. Reuse `input_tokens` for the capability guard's `estimated_tokens` (chat.rs:1827-1830)
   instead of recomputing the same concat+tokenize:
   ```rust
   let estimated_tokens = u64::from(input_tokens);
   ```
4. `last_user_message_text` is now unused (the #41 fix already removed its only
   other caller; a workspace grep confirms chat.rs:1803 is its last use). Remove
   the function — leaving it would fail `-D warnings` (dead_code). There is no unit
   test for it to remove.

No signature changes; `RouteMatch.input_tokens_estimate` stays `u32`;
`evaluate_with_cost`/`estimate_cost_usd` already take `u32`.

## Behavior change

Cost-based route conditions and the `max_cost_usd` ceiling now evaluate against the
**full prompt** rather than the last user message:
- A multi-turn / large-system-prompt request that *should* match an
  `estimated_cost_gt` route (or trip a ceiling) now does — more accurate.
- **Single-message requests are unchanged** (last user message == full prompt), so
  the existing routing tests (`cost_routing.rs`, `route_rewrite.rs`, etc., which use
  single-message requests) stay green.
- This is strictly a correctness improvement (the cap/threshold now reflects the
  cost the provider actually bills), but it is a behavior change for any org with
  cost-based routes on multi-turn traffic.

## Testing

- **New integration test** (`crates/core/tests/`, mirroring `cost_routing.rs`'s
  harness): a request with a **large system prompt + a tiny last user message**,
  and an `estimated_cost_gt` route whose threshold sits **between** the
  last-message cost and the full-prompt cost. Assert the request reroutes
  (`x-tokentrimmer-model-used` == the route target). Under the old last-message
  estimate the cost would be below the threshold and the route would NOT fire, so
  this test fails on the pre-fix code and passes after.
- **Regression:** the existing `cost_routing.rs` (single-message gpt-4o reroute /
  pass-through) and other routing tests stay green unchanged.
- **Gates:** `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`
  (also confirms `last_user_message_text` removal left no dangling refs);
  `cargo test -p tt-core`; `cargo deny check advisories`.

## Out of scope

- Changing the routing-engine API or the `RouteConditions` shape.
- The output-token side of the cost estimate (still `max_tokens` or the default) —
  unchanged; this fix only corrects the **input**-token undercount.
- Any change to `/v1/preview` (already correct) or the live dispatch paths (already
  correct).
