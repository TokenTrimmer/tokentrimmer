-- Revert: drop the minify estimated-savings column.
ALTER TABLE request_logs
  DROP COLUMN minify_saved_est_usd;
