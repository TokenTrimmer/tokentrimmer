-- Revert: drop `truncated` column from request_logs.

ALTER TABLE request_logs
  DROP COLUMN truncated;
