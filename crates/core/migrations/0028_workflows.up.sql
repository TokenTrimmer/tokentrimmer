CREATE TABLE IF NOT EXISTS workflow_definitions (
  id           UUID NOT NULL,
  org_id       UUID NOT NULL,
  version      INT  NOT NULL,
  definition   JSONB NOT NULL,
  content_hash TEXT NOT NULL,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (id, version)
);
CREATE INDEX IF NOT EXISTS workflow_definitions_org ON workflow_definitions (org_id, id);

CREATE TABLE IF NOT EXISTS workflow_runs (
  id           UUID PRIMARY KEY,
  workflow_id  UUID NOT NULL,
  version      INT  NOT NULL,
  org_id       UUID NOT NULL,
  status       TEXT NOT NULL,
  inputs       JSONB,
  cost_usd     NUMERIC(12,6) NOT NULL DEFAULT 0,
  max_cost_usd NUMERIC(12,6),
  error        TEXT,
  started_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  finished_at  TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS workflow_runs_org_started ON workflow_runs (org_id, started_at DESC);

CREATE TABLE IF NOT EXISTS workflow_node_runs (
  id          UUID PRIMARY KEY,
  run_id      UUID NOT NULL,
  node_id     TEXT NOT NULL,
  attempt     INT  NOT NULL DEFAULT 1,
  status      TEXT NOT NULL,
  output      JSONB,
  cost_usd    NUMERIC(12,6) NOT NULL DEFAULT 0,
  model_used  TEXT,
  error       TEXT,
  started_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  finished_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS workflow_node_runs_run ON workflow_node_runs (run_id);
