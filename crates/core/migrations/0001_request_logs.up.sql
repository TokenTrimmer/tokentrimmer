-- Per-request telemetry. Spec §8.2 baseline + cached_tokens / upstream_latency_ms / trace_id
-- additions for cost-attribution accuracy and trace correlation.
--
-- Note: foreign keys to orgs(id), api_keys(id), routes(id) are intentionally
-- omitted in this migration — those tables land in later auth/routing migrations
-- and we add the FK constraints there. We keep these as UUID columns now so the
-- write path can be wired ahead of the schema landing.

CREATE TABLE request_logs (
  id                   UUID PRIMARY KEY,
  org_id               UUID         NOT NULL,
  api_key_id           UUID         NOT NULL,
  ts                   TIMESTAMPTZ  NOT NULL,
  provider             TEXT         NOT NULL,
  model                TEXT         NOT NULL,
  input_tokens         INT          NOT NULL,
  output_tokens        INT          NOT NULL,
  cached_tokens        INT          NOT NULL DEFAULT 0,
  cost_usd             NUMERIC(12,6) NOT NULL,
  baseline_cost_usd    NUMERIC(12,6) NOT NULL,
  cached               BOOLEAN      NOT NULL,
  cache_layer          TEXT,                       -- null | 'l1' | 'l2'
  route_id             UUID,
  latency_ms           INT          NOT NULL,
  upstream_latency_ms  INT,
  status               INT          NOT NULL,
  tag                  TEXT,
  error_class          TEXT,
  trace_id             TEXT
);

CREATE INDEX request_logs_org_ts ON request_logs (org_id, ts DESC);

-- CHECK constraint: cache_layer must be null, 'l1', or 'l2'.
ALTER TABLE request_logs
  ADD CONSTRAINT request_logs_cache_layer_valid
  CHECK (cache_layer IS NULL OR cache_layer IN ('l1', 'l2'));
