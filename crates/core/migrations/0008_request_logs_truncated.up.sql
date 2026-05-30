-- Add `truncated` column to request_logs (§rv-truncated-column).
-- Rows where the SSE stream was dropped before a finish_reason chunk arrived
-- are flagged truncated=true. Existing rows default to false (not truncated).

ALTER TABLE request_logs
  ADD COLUMN truncated BOOLEAN NOT NULL DEFAULT false;
