-- Immutable route-definition provenance for request traces.
--
-- The cloud-owned `route_versions` ledger has a BIGINT identity. Keep this
-- nullable and deliberately do NOT add a foreign key here: public migrations
-- can run before the cloud ledger migration, historical request rows cannot be
-- backfilled honestly, and a route version remains meaningful after its live
-- route is deleted. The gateway records NULL whenever no exact ledger identity
-- was available at the runtime-route refresh; it must never substitute the
-- mutable `routes.revision` concurrency token.

ALTER TABLE request_logs
    ADD COLUMN IF NOT EXISTS route_version_id BIGINT;

-- Version-history investigations begin with an organization plus immutable
-- route-version ID; keep chronological trace reads bounded without imposing an
-- index cost on the overwhelmingly common NULL/unrouted rows.
CREATE INDEX IF NOT EXISTS request_logs_org_route_version_ts_idx
    ON request_logs (org_id, route_version_id, ts DESC)
    WHERE route_version_id IS NOT NULL;
