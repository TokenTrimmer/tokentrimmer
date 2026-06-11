-- Paired A/B quality verdicts (research Phase 0.4). One row per judged sample.
-- ADDITIVE new table — request_logs keeps its append-only semantics and its
-- 25-bind INSERT untouched (the request_logs row is written before the
-- detached judge resolves; a second writer would be racy).
--
-- request_id joins request_logs.trace_id, which is how Phase 2 nets the
-- measurement tax into routing attribution.
--
-- Cost conventions (the honesty-of-the-ledger rules):
--   * judge_cost_usd     — the judge tax: cost of the judge call(s), summed
--                          over both orders in both-orders mode. NOT NULL:
--                          every judged row carries it; 0 means the judge
--                          model had no catalog pricing (unmetered), never
--                          "free".
--   * baseline_cost_usd  — cost of the baseline reference dispatch inside the
--                          detached judge task; NULL when the reference
--                          answer pre-existed (nothing extra was dispatched).
--   Both are measurement tax: kept OUT of request_logs cost columns so
--   savings stay invoice-reconcilable; never counted as or against savings.
--
-- Debiasing audit trail:
--   * optimized_position — the blind slot ('a'/'b') the OPTIMIZED answer
--                          occupied in the paired prompt (position
--                          randomization is auditable per row).
--   * orders_judged      — 1, or 2 in both-orders mode.
--   * orders_agreed      — whether the two orders' mapped verdicts agreed;
--                          NULL unless both orders were judged.
CREATE TABLE quality_verdicts (
    id                 UUID PRIMARY KEY,
    org_id             UUID NOT NULL,
    route_id           UUID,
    request_id         UUID NOT NULL,
    ts                 TIMESTAMPTZ NOT NULL DEFAULT now(),
    requested_model    TEXT NOT NULL,            -- baseline (originally requested)
    served_model       TEXT NOT NULL,            -- optimized (served)
    verdict            TEXT NOT NULL CHECK (verdict IN ('acceptable','degraded','unclear')),
    reason             TEXT NOT NULL DEFAULT '',
    judge_model        TEXT NOT NULL,
    judge_cost_usd     NUMERIC(12,6) NOT NULL DEFAULT 0,
    baseline_cost_usd  NUMERIC(12,6),
    optimized_position TEXT CHECK (optimized_position IN ('a','b')),
    orders_judged      SMALLINT NOT NULL DEFAULT 1,
    orders_agreed      BOOLEAN
);

CREATE INDEX quality_verdicts_swap_idx ON quality_verdicts (org_id, requested_model, served_model, ts);
CREATE INDEX quality_verdicts_request_idx ON quality_verdicts (request_id);
