# Public honesty/accuracy cleanup (batch 7a) — Design

**Status:** approved (gap-sweep follow-up, 2026-06-08)
**Date:** 2026-06-08
**Slice:** Audit-remediation, public repo. Two small code/doc fixes surfaced by a cross-repo gap sweep, plus a stale-checklist reconciliation. Locally verifiable.

## Fix 1 — `route_suggestions` rationale claims telemetry-backed history it doesn't have (ux/medium)
`crates/preview/src/route_suggestions.rs:57` built the rationale `"{candidate} historically handles {task_class:?} tasks at lower cost"` while simultaneously setting `quality_risk_band: QualityRiskBand::Unknown` ("not yet computed for your org") — internally contradictory, and re-introducing the exact unsubstantiated "historically handles" claim the team deliberately removed from `find_route_for` (which has a `no_historical_language` guard test). Surfaced on `/v1/preview`.
**Fix:** rewrite the rationale to be honest — a lower-cost option by a **static pricing + capability heuristic, "not based on your telemetry"** (mirroring `find_route_for`'s wording). The real `savings_usd` is surfaced separately; the UNKNOWN-band disclosure stays. Added a guard test forbidding `"historically"` and requiring the telemetry disclaimer.

## Fix 2 — `RequestContext` doc references a non-existent `tier` field (docs/medium)
`crates/shared/src/context.rs:13` — the `CallerTier` doc said "The `tier` field on `RequestContext` … is `Option<CallerTier>`", but `RequestContext` has no `tier` field (only `trace_id`/`org_id`/`api_key_id`/`credentials`/`tag`/`deadline`); tier lives on `tt_auth::ApiKeyContext`. An integrator wiring tier-based TTL off the shared request type follows a dead pointer.
**Fix:** correct the doc to point at `tt_auth::ApiKeyContext` and note `RequestContext` has no `tier` field.

## Checklist reconciliation (no code) — 5 stale OPEN entries already fixed this session
The gap sweep flagged several checklist items marked `🔴 OPEN` that are already fixed in code (verified before flipping):
- `min_similarity` NaN/range → #87 · Gemini partial-usage reconcile → #86 · `LocalProvider::dropped_params` → #86 · `tt route` timeout → #85 (and `tt models` is bounded via `fetch_catalog`'s per-request `.timeout(5s)`) · `mask_key` char-safe → #85.
Flipped all five to ✅ with a "stale checkbox reconciled" note, so they aren't re-investigated.

**Dropped (false positive):** the sweep's "`catalog.rs` bare `Client::new()` has no timeout" — verified NOT a bug: the only production caller routes through `fetch_catalog`, which applies a per-**request** `.timeout(5s)`; the other bare clients are in `#[tokio::test]`s. The request, not the client, is bounded.

## Verification (done)
- `cargo test -p tt-preview -p tt-shared` — 23 + 74 pass (incl. the new honesty guard). `cargo clippy --all-targets` clean. `cargo fmt --check` clean on the two files.
