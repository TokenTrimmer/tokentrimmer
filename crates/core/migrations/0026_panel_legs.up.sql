-- Per-leg detail for deep-research panel requests. One row per dispatched leg
-- (member or arbiter), keyed by the parent request_logs row's id.
--
-- `request_log_id` references request_logs.id by convention; no enforced FK,
-- matching the no-FK convention established in migration 0001 (those tables
-- landed after this one and FKs were intentionally omitted throughout). A plain
-- index on `request_log_id` is sufficient for the per-request child-row lookup.
--
-- `leg_index` is 0-based for member legs; the arbiter uses a high sentinel
-- value (e.g. 255) or is distinguished by `role='arbiter'`.
--
-- `cost_usd` is NULLABLE — NULL means unmetered/unpriced (never coerced to 0,
-- mirroring the fail-closed cost-limit gate behaviour).
--
-- `status` values: 'ok' | 'error' | 'timeout' | 'skipped_no_cred'.

CREATE TABLE panel_legs (
    request_log_id  UUID             NOT NULL,  -- = request_logs.id (no enforced FK, per 0001 convention)
    leg_index       INT              NOT NULL,  -- 0..N-1 for member legs; arbiter uses a high sentinel or role
    role            TEXT             NOT NULL,  -- 'leg' | 'arbiter'
    provider        TEXT             NOT NULL,  -- per-leg provider (unblocks Phase-3 per-provider invoice recon)
    model           TEXT             NOT NULL,
    input_tokens    BIGINT,
    output_tokens   BIGINT,
    cached_tokens   BIGINT,
    cost_usd        DOUBLE PRECISION,           -- per-leg cost; NULL = unmetered/unpriced (never coerced to 0)
    latency_ms      BIGINT,
    status          TEXT             NOT NULL,  -- 'ok' | 'error' | 'timeout' | 'skipped_no_cred'
    error_class     TEXT,
    PRIMARY KEY (request_log_id, leg_index)
);

CREATE INDEX panel_legs_request_log_id_idx ON panel_legs (request_log_id);
