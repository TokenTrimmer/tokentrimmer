-- Revert: drop the content-aware compression isolated-savings + flywheel-label
-- columns.
ALTER TABLE request_logs
  DROP COLUMN content_compress_kind;
ALTER TABLE request_logs
  DROP COLUMN content_compress_saved_est_usd;
