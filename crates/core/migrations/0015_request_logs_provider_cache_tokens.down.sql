-- Revert: drop the provider prompt-cache token telemetry columns.
ALTER TABLE request_logs
  DROP COLUMN cache_read_input_tokens,
  DROP COLUMN cache_creation_input_tokens;
