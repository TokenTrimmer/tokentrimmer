DROP INDEX IF EXISTS request_logs_org_requested_model_ts_idx;

ALTER TABLE request_logs
    DROP COLUMN IF EXISTS requested_model;
