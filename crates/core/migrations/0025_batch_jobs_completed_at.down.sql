-- Roll back the Batch-Lane reconciliation-parity columns.
DROP INDEX IF EXISTS batch_jobs_org_completed_at_idx;
ALTER TABLE batch_jobs DROP COLUMN IF EXISTS batch_cost_usd;
ALTER TABLE batch_jobs DROP COLUMN IF EXISTS batch_completed_at;
