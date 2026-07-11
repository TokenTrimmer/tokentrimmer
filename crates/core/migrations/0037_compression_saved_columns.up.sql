-- TR-2: persist the namesake conservative-compression savings.
--
-- `compression_saved_usd` (USD, fee-applied) + `compression_tokens_removed`
-- (token count) for the lossless conservative `compress` pass
-- (`RouteAction::compress`, off by default). The pass removes redundant
-- input tokens before dispatch (token-true-gated); its USD value is computed
-- in chat.rs from `pass_effects.compression_tokens_removed × input rate`,
-- applied with the provider fee, shipped on the
-- `X-TokenTrimmer-Compression-Saved-Usd` header — but until this migration was
-- NEVER persisted, so "how much did compression save this month, split from
-- routing / doc-compaction / cache?" was unanswerable (review §3 TR-2).
--
-- These are MEASURED, headline-folding figures (the removed tokens' value
-- raises `baseline_cost_usd` via the same baseline fold as doc-compaction, so
-- the saving rides `baseline − cost`) — NOT an isolated estimate like
-- `content_compress_saved_est_usd` (migration 0033) or
-- `doc_vision_saved_est_usd` (migration 0032). Additive + back-compat:
-- existing rows (and every request whose route did not opt into `compress`)
-- carry 0 / 0.0. Dashboards may SUM(compression_saved_usd) into the per-lever
-- rollup alongside routing/cache/doc-compaction; the token count feeds
-- TR-1's per-request waterfall + methodology reconciliation.
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS compression_saved_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS compression_tokens_removed BIGINT NOT NULL DEFAULT 0;
