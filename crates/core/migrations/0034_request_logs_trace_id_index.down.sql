-- Revert: drop the P2a trace_id partial index. The column is unchanged
-- (nullable TEXT, migration 0001); only the index is dropped.
DROP INDEX IF EXISTS request_logs_trace_id_idx;
