-- Add baseline_cost_usd to cache_entries (L2 semantic cache).
--
-- On an L2 hit the gateway reports saved_usd to the caller. Before this
-- column existed the hit path had no pricing data for the entry's model and
-- used a hardcoded $1/M-input + $2/M-output placeholder — overstating savings
-- ~6.7x for cheap models and understating 15x+ for expensive ones, and
-- contradicting the methodology page's claim that savings come from the
-- versioned pricing catalog.
--
-- New inserts store the catalog-derived baseline (the same `compute_cost`
-- math used to price live dispatches) so hits report honest savings.
--
-- Nullable by design: rows inserted before this migration carry NULL. The
-- hit path then re-prices the row's stored model/token counts against the
-- CURRENT catalog, or reports 0 saved when the model is absent from the
-- catalog — never the fabricated placeholder.

ALTER TABLE cache_entries
    ADD COLUMN IF NOT EXISTS baseline_cost_usd DOUBLE PRECISION;
