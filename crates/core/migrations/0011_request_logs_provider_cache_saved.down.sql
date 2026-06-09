-- Revert: drop `provider_cache_saved_usd` column from request_logs.

ALTER TABLE request_logs
  DROP COLUMN provider_cache_saved_usd;
