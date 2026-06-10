-- Revert migration 0010: remove baseline_cost_usd from cache_entries.
ALTER TABLE cache_entries DROP COLUMN IF EXISTS baseline_cost_usd;
