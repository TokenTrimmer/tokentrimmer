-- Add `provider_cache_saved_usd` to request_logs (P0 #12 — savings attribution).
-- The provider's automatic prompt-cache discount (read discount net of any
-- cache-write premium) is recorded separately from TokenTrimmer-attributed
-- savings so the TT headline (baseline_cost_usd - cost_usd -
-- provider_cache_saved_usd) survives reconciliation against the provider
-- invoice. Existing rows default to 0 (discount previously folded into the
-- baseline/cost delta).

ALTER TABLE request_logs
  ADD COLUMN provider_cache_saved_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
