-- Add `cache_bust_penalty_usd` to request_logs (cache-aware pass lane —
-- negative savings booking).
--
-- A deliberate, NON-deterministic mutation of a request's cache-stable prefix
-- (a booked CacheBustEstimate) reduces the TT-attributed savings headline the
-- gateway sends in `x-tokentrimmer-saved-usd` / the OTel span. Persisting the
-- penalty keeps the row-derived ledger in agreement with those channels: the
-- TT headline derived from a row is now
--
--   max(0, baseline_cost_usd - cost_usd - provider_cache_saved_usd
--             - cache_bust_penalty_usd)
--
-- Without the column, any dashboard recomputing savings from request_logs
-- would silently report MORE saving than the per-request headers whenever a
-- bust books. The penalty is an ESTIMATE of induced future cost: it is never
-- folded into cost_usd / baseline_cost_usd (those reconcile against the
-- realized provider invoice). 0 for every request whose stable prefix was
-- untouched — including all redaction traffic, whose ingress-deterministic
-- mutation busts nothing — and for rows from before this migration.

ALTER TABLE request_logs
  ADD COLUMN cache_bust_penalty_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
