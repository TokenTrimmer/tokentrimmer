# Streaming cache-write cost — Design

**Status:** approved (design)
**Date:** 2026-06-07
**Slice:** Audit-remediation Wave 3 (public repo, `crates/core/src/routes/sse.rs`). Closes the bug/medium finding: the streaming SSE cost path prices Anthropic `cache_creation_input_tokens` at the base input rate instead of the ~1.25× cache-write premium, undercounting `cost_usd` and recorded spend. The reconstructed cache entry also drops the field, so a streamed-then-cached response replays with a wrong usage breakdown.

## Goal

Make the streaming cost path price the three input buckets — fresh / cache_read / cache_write — exactly like the non-streaming path, and carry `cache_creation_input_tokens` through to the cache entry. Fix the **root cause**: the streaming path maintains a *parallel* cost function (`compute_streaming_cost` / `compute_streaming_baseline`) that drifted from the authoritative `chat::compute_cost`. Unify on `compute_cost` so the two cannot drift again.

## Background (verified)

- Non-streaming `crates/core/src/routes/chat.rs::compute_cost(usage: &Usage, pricing, baseline_pricing, fee_multiplier) -> (f64, f64)` (`:1832`) is `pub(crate)` and already correct: `cache_read = cached.min(prompt)`, `cache_write = cache_creation_input_tokens.unwrap_or(0).min(prompt - cache_read)`, `fresh = prompt - cache_read - cache_write`; rates fresh→`input_per_million`, cache_read→`cached_input_per_million` (fallback base), cache_write→`cache_write_per_million` (fallback base); `+ completion × output_per_million`. Baseline = `prompt × baseline_input + completion × baseline_output` (no discount), `baseline_pricing` falling back to `pricing`. Both ×`fee_multiplier`, returned as a tuple.
- `Usage` (`crates/shared/src/usage.rs`): `prompt_tokens, completion_tokens, total_tokens, cached_tokens: u64`, `cache_creation_input_tokens: Option<u64>`.
- Streaming `sse.rs`:
  - `PartialUsage { input_tokens: i32, output_tokens: i32, cached_tokens: i32 }` (`:44`) — **no cache_creation**.
  - `UsageTrackingStream.authoritative: Option<(i32, i32, i32)>` (`:70`) = (prompt, completion, cached). `poll_next` (`:159-165`) sets it from `usage.prompt_tokens/completion_tokens/cached_tokens` — **drops `cache_creation_input_tokens`**.
  - `snapshot()` (`:115`) builds `PartialUsage` from the tuple (authoritative) or a tokenizer fallback (no authoritative → cache_creation is 0).
  - `cache_completion_data()` (`:101`) reconstructs `Usage` with `cache_creation_input_tokens: None` (`:110`).
  - `compute_streaming_cost` (`:653`) and `compute_streaming_baseline` (`:668`) — parallel cost math, no cache_write bucket. Two call sites: `TrackedEventStream::usage_event` (`:297-303`, the terminal `tokentrimmer.usage` SSE event) and the `DropGuard` (`:468-470`, records realized spend + request log). Both multiply by `fee_multiplier` at the call site and compute `saved = (baseline - cost).max(0)`.
  - Unit tests (`sse.rs` `:989-1007`, §2.13) assert `compute_streaming_cost`/baseline each scale by the fee multiplier.

## Architecture (`crates/core/src/routes/sse.rs`)

Decision (user): **unify on `compute_cost`** — capture cache_creation, then replace the parallel cost fns with a `PartialUsage → Usage` conversion + a `chat::compute_cost` call.

### 1. Capture cache_creation
- `PartialUsage`: add `pub cache_creation_tokens: i32`.
- `UsageTrackingStream.authoritative`: widen to `Option<(i32, i32, i32, i32)>` = (prompt, completion, cached, cache_creation).
- `poll_next`: set the 4th element from `usage.cache_creation_input_tokens.unwrap_or(0) as i32`.
- `snapshot()`: authoritative arm sets `cache_creation_tokens` from the 4th element; the tokenizer-fallback arm sets it to `0` (no authoritative block → no known cache-creation).
- `cache_completion_data()`: read the 4-tuple; set `cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation as u64)` on the reconstructed `Usage` (carries the breakdown into the streamed-then-cached entry). Keep returning `None` for the whole fn when there is no authoritative usage (unchanged truncation guard).

### 2. Convert + delegate to `compute_cost`
Add a private helper in `sse.rs`:
```rust
/// Build a `Usage` from accumulated streaming counts so the streaming path can
/// reuse the authoritative non-streaming cost math (`chat::compute_cost`).
fn partial_to_usage(u: &PartialUsage) -> tt_shared::Usage {
    let prompt = u.input_tokens.max(0) as u64;
    let completion = u.output_tokens.max(0) as u64;
    let cache_creation = u.cache_creation_tokens.max(0) as u64;
    tt_shared::Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cached_tokens: u.cached_tokens.max(0) as u64,
        cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation),
    }
}
```
- Delete `compute_streaming_cost` and `compute_streaming_baseline`.
- **Call site A — `usage_event`** (`:297-303`): replace the two calls with
  ```rust
  let (cost_usd, baseline_cost_usd) = crate::routes::chat::compute_cost(
      &partial_to_usage(&usage),
      Some(pricing),
      self.baseline_pricing.as_ref(),
      self.fee_multiplier,
  );
  let saved_usd = (baseline_cost_usd - cost_usd).max(0.0_f64);
  ```
  (`compute_cost` applies the fee internally and falls back `baseline_pricing → pricing`, matching the old `.or(Some(pricing))`.)
- **Call site B — `DropGuard`** (`:468-470`): same replacement, using the guard's captured `pricing.as_ref()`, `baseline_pricing.as_ref()`, `fee_multiplier`. Downstream `spend_sink.record(org_id, cost_usd, …)` and the `RequestLogRow` are unchanged apart from consuming the new `cost_usd`/`baseline_cost_usd`.

Behavior parity: for a response with no cache-creation tokens (`cache_creation = 0`), `compute_cost` reduces to fresh+cache_read+output — identical to the old `compute_streaming_cost`; the baseline formula is identical. So non-Anthropic and non-cache-write streams are unaffected; only Anthropic cache-write streams change (now priced at the write premium).

## Error handling
No new failure modes. `pricing == None` → `compute_cost` returns `(0.0, 0.0)` (same as the old guard). Negative counts are clamped (`.max(0)`) in the conversion (counts are always ≥ 0 in practice).

## Testing (`sse.rs` unit tests)
- **New:** a streaming `PartialUsage` with `cache_creation_tokens > 0` (and a `ModelPricing` with a `cache_write_per_million` premium) prices the cache-write bucket at the premium — assert the cost equals the hand-computed fresh+cache_read+cache_write+output and is **greater** than the same usage priced with cache_creation folded into fresh input. (Pins the undercount fix.)
- **New:** `cache_completion_data()` carries `cache_creation_input_tokens = Some(n)` when the terminal chunk had cache-creation tokens, and `None` when it had zero.
- **New:** `poll_next` captures `cache_creation_input_tokens` into the authoritative tuple / `snapshot()` (feed a terminal chunk whose `usage.cache_creation_input_tokens = Some(n)`; assert `snapshot().cache_creation_tokens == n`).
- **Update:** the §2.13 fee-multiplier test — rewrite to call `compute_cost` via `partial_to_usage` (or assert the fee scaling through `usage_event`/the public path) since `compute_streaming_cost`/baseline no longer exist. Keep the assertion that cost and baseline both scale by `fee_multiplier`.
- **Regression:** an all-fresh-input stream (cache_creation = 0, cached = 0) produces the same cost as before the change (parity).
- Existing snapshot/fallback tests (`:933`, `:973`) keep passing (the fallback arm sets `cache_creation_tokens = 0`; add the field to any `PartialUsage { .. }` literals in tests).

Gates: `cargo test -p tt-core`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all -- --check`; `cargo test --workspace --no-run` (per `ci-verify-all-targets`).

## Out of scope
- The streaming spend-cap concurrency amplification (separate finding `:130-131` — documented best-effort).
- The chat.rs handler refactor / stale-docstring cleanup (separate finding `:150-151`).
- Any change to `compute_cost` itself (it is already correct).
- Non-Anthropic providers (they emit no `cache_creation_input_tokens`, so `Some(0)`→`None`; unaffected).
