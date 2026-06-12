-- Revert: drop the advisory batch-eligibility columns from request_logs.

ALTER TABLE request_logs
  DROP COLUMN batch_forgone_usd,
  DROP COLUMN batch_eligible;
