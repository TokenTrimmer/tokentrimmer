-- Attribute each request_logs row to the agent run (and future workflow node) that produced it.
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS run_id  UUID;
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS node_id UUID;
CREATE INDEX IF NOT EXISTS request_logs_run_id ON request_logs (run_id) WHERE run_id IS NOT NULL;

-- Durable identity + terminal state for a server-side agent run (transcript stays in Redis).
CREATE TABLE IF NOT EXISTS agent_runs (
  id            UUID PRIMARY KEY,
  org_id        UUID         NOT NULL,
  status        TEXT         NOT NULL,
  model         TEXT         NOT NULL,
  turns         INT          NOT NULL DEFAULT 0,
  max_turns     INT,
  max_cost_usd  NUMERIC(12,6),
  cost_usd      NUMERIC(12,6) NOT NULL DEFAULT 0,
  stop_reason   TEXT,
  tag           TEXT,
  started_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
  finished_at   TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS agent_runs_org_started ON agent_runs (org_id, started_at DESC);
