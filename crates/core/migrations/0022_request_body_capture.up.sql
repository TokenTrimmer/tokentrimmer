-- Opt-in encrypted body capture for hosted /logs replay.
-- Plaintext never lands in Postgres; request_enc/response_enc are encrypted by
-- tt-telemetry::body_capture using a per-org key derived from TT_MASTER_KEY.

CREATE TABLE IF NOT EXISTS request_body_capture_settings (
  org_id UUID PRIMARY KEY,
  enabled BOOLEAN NOT NULL DEFAULT false,
  retention_days INTEGER NOT NULL DEFAULT 7 CHECK (retention_days BETWEEN 1 AND 30),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS request_body_captures (
  id UUID PRIMARY KEY,
  org_id UUID NOT NULL,
  api_key_id UUID NOT NULL,
  trace_id TEXT NOT NULL,
  ts TIMESTAMPTZ NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL,
  endpoint TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  request_enc BYTEA NOT NULL,
  response_enc BYTEA,
  request_bytes INTEGER NOT NULL,
  response_bytes INTEGER,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (org_id, trace_id)
);

CREATE INDEX IF NOT EXISTS request_body_captures_org_ts
  ON request_body_captures (org_id, ts DESC);

CREATE INDEX IF NOT EXISTS request_body_captures_org_expires
  ON request_body_captures (org_id, expires_at);
