DROP TABLE IF EXISTS agent_runs;
DROP INDEX IF EXISTS request_logs_run_id;
ALTER TABLE request_logs DROP COLUMN IF EXISTS node_id;
ALTER TABLE request_logs DROP COLUMN IF EXISTS run_id;
