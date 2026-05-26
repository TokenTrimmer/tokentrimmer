-- Inspect run + findings tables. Per spec §8.2.
--
-- One row per scan in `inspect_runs`; many findings per run in `inspect_findings`.
-- Index strategy:
--   - inspect_runs.org_id for per-org listing
--   - inspect_findings.run_id for fetching a run's findings
--   - inspect_findings.severity for filtering by severity
--   - inspect_findings.rule_id for filtering by rule (e.g. per-rule FP triage)

CREATE TABLE inspect_runs (
  id              UUID PRIMARY KEY,
  org_id          UUID         NOT NULL,
  repo_url        TEXT,
  commit_sha      TEXT,
  started_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
  finished_at     TIMESTAMPTZ,
  status          TEXT         NOT NULL DEFAULT 'running',
  CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled'))
);

CREATE INDEX inspect_runs_org_started ON inspect_runs (org_id, started_at DESC);

CREATE TABLE inspect_findings (
  id                              UUID PRIMARY KEY,
  run_id                          UUID         NOT NULL REFERENCES inspect_runs(id) ON DELETE CASCADE,
  rule_id                         TEXT         NOT NULL,
  severity                        TEXT         NOT NULL,
  file_path                       TEXT         NOT NULL,
  line                            INT          NOT NULL,
  message                         TEXT         NOT NULL,
  confidence                      REAL         NOT NULL,
  fix_hint                        TEXT,
  fix_diff                        TEXT,
  estimated_annual_savings_usd    NUMERIC(12,2),
  created_at                      TIMESTAMPTZ  NOT NULL DEFAULT now(),
  CHECK (severity IN ('low', 'medium', 'high', 'critical'))
);

CREATE INDEX inspect_findings_run     ON inspect_findings (run_id);
CREATE INDEX inspect_findings_sev     ON inspect_findings (run_id, severity);
CREATE INDEX inspect_findings_rule    ON inspect_findings (rule_id);
