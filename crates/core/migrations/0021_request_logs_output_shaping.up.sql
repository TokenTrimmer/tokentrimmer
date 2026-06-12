-- Output shaping (research Phase 3.3 + 3.4): format-switch + delta/diff markers.
-- format_switched: 'csv'|'bare' when a route's format_switch VALIDATED on this
-- response (NULL = not switched). format_switch_saved_est_usd is a LABELED
-- ESTIMATE (JSON-equivalent reconstruction − emitted body, output-rate-priced,
-- fee-applied) — NEVER part of cost_usd/baseline_cost_usd/saved-usd (those
-- reconcile against the provider invoice). diff_saved_usd is the measured
-- output-token reduction (reconstructed artifact − billed patch), included in
-- the headline via the baseline fold (compression precedent, migration 0016
-- header note applies). diff_failed marks a fail-closed full re-emit;
-- diff_failed_cost_usd is the failed patch attempt's realized cost — folded
-- into cost_usd (real invoice spend) AND kept here so it can be unpicked.
ALTER TABLE request_logs
  ADD COLUMN format_switched             TEXT          CHECK (format_switched IN ('csv','bare')),
  ADD COLUMN format_switch_saved_est_usd NUMERIC(12,6) NOT NULL DEFAULT 0,
  ADD COLUMN diff_applied                BOOLEAN       NOT NULL DEFAULT false,
  ADD COLUMN diff_saved_usd              NUMERIC(12,6) NOT NULL DEFAULT 0,
  ADD COLUMN diff_failed                 BOOLEAN       NOT NULL DEFAULT false,
  ADD COLUMN diff_failed_cost_usd        NUMERIC(12,6) NOT NULL DEFAULT 0;
