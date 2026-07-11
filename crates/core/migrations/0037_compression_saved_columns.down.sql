-- Revert: drop the conservative-compression savings + token-count columns.
ALTER TABLE request_logs
  DROP COLUMN compression_tokens_removed;
ALTER TABLE request_logs
  DROP COLUMN compression_saved_usd;
