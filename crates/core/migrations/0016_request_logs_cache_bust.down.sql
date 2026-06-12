-- Revert: drop `cache_bust_penalty_usd` column from request_logs.

ALTER TABLE request_logs
  DROP COLUMN cache_bust_penalty_usd;
