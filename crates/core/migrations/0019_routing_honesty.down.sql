DROP INDEX request_logs_org_route_ts;
DROP INDEX quality_verdicts_route_idx;
ALTER TABLE request_logs DROP COLUMN route_paused;
DROP TABLE route_pauses;
