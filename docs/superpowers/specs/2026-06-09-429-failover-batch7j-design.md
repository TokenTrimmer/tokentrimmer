# 429 → cross-provider failover (batch 7j) — Design

**Status:** approved (2026-06-09)
**Slice:** Audit-remediation, public repo, `crates/{shared,core}`. One opportunity/low behavior change.

## Finding (checklist L169)
`ProviderError::is_fallback_eligible()` excluded `RateLimited`, so a sustained 429 on the primary provider was only retried on the *same* provider (via `with_retry`) and then surfaced — it never failed over to a configured fallback provider that may have spare quota. For a multi-provider routing product, a primary capacity crunch is exactly when cross-provider failover adds reliability.

## Change
Add `ProviderError::RateLimited { .. } => true` to `is_fallback_eligible()` (`crates/shared/src/error.rs`); update the doc comment (it previously said 429 was "handled separately").

**Ordering is correct for free.** In `crates/core/src/failover.rs`, `dispatch_with_failover` (and its streaming sibling `dispatch_stream_with_failover`) wrap each candidate in `with_retry(...)`, which already retries `is_retriable` errors — including `RateLimited`, honoring `retry_after_ms` backoff — on the same provider up to `max_attempts`. Fallback-eligibility is only checked on `with_retry`'s *final* result. So making 429 fallback-eligible yields exactly "retry the same provider N times (the existing retry budget), then fail over to the next candidate" with no additional gating. Both match-arm sites (non-streaming + streaming) share `is_fallback_eligible`, so the behavior applies to both.

**Edge cases:**
- Single-provider routes: no next candidate → the loop returns the 429 after retries (unchanged).
- On failover, `breaker.record_failure(provider)` is recorded (same as the 5xx path) — a rate-limited provider trips its circuit and is skipped during cooldown, which is the desired response to a sustained 429 (stop hammering it).
- Cross-provider failover still requires the org to have a credential for the alternate provider (the loop already skips uncredentialed candidates).

## Tests
- `crates/shared/src/error.rs`: `rate_limited_is_fallback_eligible` (asserts both `is_fallback_eligible` and `is_retriable`).
- `crates/core/src/failover.rs`: added `Behavior::Fail429` to the test mock (returns `RateLimited { retry_after_ms: 0 }`); `falls_over_to_next_candidate_on_429` and `stream_falls_over_to_next_candidate_on_429` mirror the existing `..._on_5xx` tests (primary 429 → serves the healthy secondary).

## Verification (done)
`cargo test -p tt-shared -p tt-core` — all pass incl. the 3 new tests. `cargo clippy -p tt-shared -p tt-core --all-targets` clean; `cargo fmt --all --check` clean.
