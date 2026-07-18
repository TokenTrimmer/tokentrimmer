DROP INDEX IF EXISTS request_logs_org_route_version_ts_idx;

ALTER TABLE request_logs
    DROP COLUMN IF EXISTS route_version_id;
