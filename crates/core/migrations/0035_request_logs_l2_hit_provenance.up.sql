-- L2 (semantic-cache) hit provenance (Slice 2 of the verifiable cache receipts).
--
-- Three optional columns on request_logs that persist the cache-hit provenance a
-- signed L2 receipt (tt_telemetry::l2_receipt, the l2:v1| Ed25519 receipt
-- mirroring the VCR) attests. Set ONLY by the L2-hit row
-- (request_log_for_l2_hit in crates/core/src/routes/chat.rs); NULL for every
-- other row (L1 hits, dispatches, rows predating this migration).
--
-- Why on request_logs (not a separate table): the L2-hit row already exists
-- (cache_layer = 'l2', cost_usd = 0, baseline_cost_usd = the hit's saved
-- baseline). The provenance fields ARE that row's attributes — splitting them
-- into a provenance table would add a join + a second write on the hot cache
-- path for no benefit. The cloud mint endpoint reads these off the same row,
-- mirroring how the VCR mint endpoint reads content_compress_saved_est_usd off
-- the request_logs row.
--
-- Additive + back-compat: all three are NULLABLE with no default change, so
-- existing rows + every non-L2 request stay byte-identical. The cloud mint
-- endpoint returns 400 when the row is NULL (no L2 provenance → no receipt).
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS l2_matched_entry_id UUID;
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS l2_similarity REAL;
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS l2_verdict TEXT;
