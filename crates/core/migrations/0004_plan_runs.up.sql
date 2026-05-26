-- Plan replay history. Per spec §8.2 and ADR-015 (auditable for every tier).
--
-- One row per `tt plan` invocation. The `apply_diff` column holds the full
-- proposed config JSON so historical "what did we apply when?" questions
-- are answerable without joining other tables. Projected metrics are stored
-- alongside actual metrics (populated by the reconciliation worker that
-- compares the projection against observed traffic post-apply).

CREATE TABLE plan_runs (
  id                          UUID PRIMARY KEY,
  org_id                      UUID         NOT NULL,
  name                        TEXT,
  config_diff                 JSONB        NOT NULL,
  window_start                TIMESTAMPTZ  NOT NULL,
  window_end                  TIMESTAMPTZ  NOT NULL,
  sample_size                 INT          NOT NULL,

  -- Projection at plan-creation time. NULL when the projection failed.
  projected_savings_usd       NUMERIC(12,4),
  projected_savings_ci_low    NUMERIC(12,4),
  projected_savings_ci_high   NUMERIC(12,4),
  projected_savings_pct       NUMERIC(7,4),
  projected_cache_hit_rate    NUMERIC(7,4),

  -- LOW / MEDIUM / HIGH per spec §6.4.
  quality_risk                TEXT,
  CHECK (quality_risk IS NULL OR quality_risk IN ('low', 'medium', 'high')),

  -- Reconciliation: filled by the worker 7 days / 30 days after apply.
  -- NULL until the worker first runs against this plan.
  actual_savings_usd          NUMERIC(12,4),
  actual_savings_pct          NUMERIC(7,4),
  actual_cache_hit_rate       NUMERIC(7,4),
  trust_score                 NUMERIC(5,4),     -- 0..1 — rolling projection-vs-actual variance

  -- Lifecycle.
  status                      TEXT         NOT NULL DEFAULT 'projected',
  CHECK (status IN ('projected', 'applied', 'reverted', 'failed')),
  applied_at                  TIMESTAMPTZ,
  reverted_at                 TIMESTAMPTZ,
  created_at                  TIMESTAMPTZ  NOT NULL DEFAULT now(),
  updated_at                  TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX plan_runs_org_created ON plan_runs (org_id, created_at DESC);
CREATE INDEX plan_runs_status      ON plan_runs (status);
