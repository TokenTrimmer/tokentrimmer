-- Routing honesty (research Phase 2.3): auto-pause state + netting indexes.
-- route_pauses: gateway-owned sticky pause records for down-routes whose
-- paired pass-rate regressed. Deliberately NO FK to routes: the routes table
-- is cloud-owned (tokentrimmer-cloud 0002) and absent on OSS deploys; public
-- migrations must apply standalone. Paused = rewrite suppressed (requests
-- flow to the original model — the EXPENSIVE, quality-safe direction);
-- sticky until an explicit resume stamps `resumed_at`. The row is RETAINED
-- after resume: `resumed_at` is the watermark the auto-pause verdict window
-- is bounded by, so a resumed route is judged only on POST-resume verdicts
-- (the frozen pre-pause window can never instantly re-pause it). A row with
-- resumed_at IS NULL = active pause; resumed_at IS NOT NULL = historical
-- record + watermark. Deleting a route deletes its pause record.
CREATE TABLE route_pauses (
    route_id           UUID PRIMARY KEY,
    org_id             UUID NOT NULL,
    paused_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    paused_by          TEXT NOT NULL DEFAULT 'auto' CHECK (paused_by IN ('auto','manual')),
    reason             TEXT NOT NULL DEFAULT '',
    pass_rate          DOUBLE PRECISION,   -- observed windowed pass rate at pause time
    verdicts_in_window INTEGER,            -- classified n backing the decision
    resumed_at         TIMESTAMPTZ         -- NULL = actively paused; set by resume
);
CREATE INDEX route_pauses_org_idx ON route_pauses (org_id);

-- request_logs-visible pause marker (additive, default keeps old 28-column
-- INSERTs from older binaries valid).
ALTER TABLE request_logs ADD COLUMN route_paused BOOLEAN NOT NULL DEFAULT FALSE;

-- Netting query indexes (the 0014 header anticipated "Phase 2 should add an
-- index alongside its netting query" — route-level netting groups by
-- route_id, so these, not a trace_id index, are what Phase 2.3 needs).
CREATE INDEX quality_verdicts_route_idx ON quality_verdicts (org_id, route_id, ts DESC);
CREATE INDEX request_logs_org_route_ts  ON request_logs (org_id, route_id, ts DESC);
