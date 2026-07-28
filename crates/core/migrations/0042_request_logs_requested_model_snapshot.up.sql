-- Caller-model provenance for historical route-condition evidence.
--
-- `request_logs.model` is the model ultimately served after routing, cache,
-- and failover. RouteConditions.model_in is instead evaluated against the
-- caller's model before those decisions. Keep that exact pre-routing snapshot
-- separately and nullable: rows written before this migration (or by an older
-- gateway during a rolling deploy) have no honest value and must remain NULL.
-- There is intentionally no backfill, default, or foreign key.

ALTER TABLE request_logs
    ADD COLUMN IF NOT EXISTS requested_model TEXT;

-- The historical preview is tenant- and time-bounded and filters only rows
-- that have a captured caller-model snapshot. Keep those reads indexable
-- without penalizing legacy/unrelated rows that remain NULL.
CREATE INDEX IF NOT EXISTS request_logs_org_requested_model_ts_idx
    ON request_logs (org_id, requested_model, ts DESC)
    WHERE requested_model IS NOT NULL;
