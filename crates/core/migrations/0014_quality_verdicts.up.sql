-- Paired A/B quality verdicts (research Phase 0.4). One row per judged sample.
-- ADDITIVE new table — request_logs keeps its append-only semantics and its
-- 25-bind INSERT untouched (the request_logs row is written before the
-- detached judge resolves; a second writer would be racy).
--
-- request_id is the request's trace id. NOTE for the Phase-2 netting join:
-- request_logs.trace_id is a NULLABLE TEXT column (not UUID-typed), so the
-- join must cast THIS side — quality_verdicts.request_id::text =
-- request_logs.trace_id (casting trace_id::uuid would throw on non-UUID
-- values) — and request_logs has no trace_id index yet; Phase 2 should add
-- one alongside its netting query.
--
-- Cost conventions (the honesty-of-the-ledger rules):
--   * judge_cost_usd      — the judge tax: cost of the judge call(s), summed
--                           over both orders in both-orders mode. NULL means
--                           a judge call was billed but UNMETERED (no catalog
--                           pricing) — never persisted as 0, so downstream
--                           can tell "genuinely ~$0" from "unknown".
--   * baseline_cost_usd   — cost of the baseline reference dispatch inside
--                           the detached judge task; NULL when nothing extra
--                           was dispatched OR the dispatch was unmetered —
--                           baseline_dispatched disambiguates.
--   * baseline_dispatched — TRUE when a baseline reference dispatch actually
--                           ran (and was billed upstream). TRUE with NULL
--                           baseline_cost_usd = a real billed dispatch on an
--                           unpriced model.
--   Both costs are measurement tax: kept OUT of request_logs cost columns so
--   savings stay invoice-reconcilable; never counted as or against savings.
--   Partial failures that already incurred billed spend (baseline billed,
--   then the judge errored; first order billed, then the second errored;
--   baseline billed but came back empty) still record a row — verdict
--   'unclear', reason prefixed 'unjudged:' — so the ledger is
--   invoice-complete even when the judge model is flaky.
--
-- Debiasing audit trail:
--   * optimized_position — the blind slot ('a'/'b') the OPTIMIZED answer
--                          occupied in the paired prompt (position
--                          randomization is auditable per row).
--   * orders_judged      — 1, or 2 in both-orders mode (0 on an unjudged
--                          spend-ledger row where no judge call completed).
--   * orders_agreed      — whether the two orders' mapped verdicts agreed;
--                          NULL unless both orders were judged.
CREATE TABLE quality_verdicts (
    id                  UUID PRIMARY KEY,
    org_id              UUID NOT NULL,
    route_id            UUID,
    request_id          UUID NOT NULL,
    ts                  TIMESTAMPTZ NOT NULL DEFAULT now(),
    requested_model     TEXT NOT NULL,            -- baseline (originally requested)
    served_model        TEXT NOT NULL,            -- optimized (served)
    verdict             TEXT NOT NULL CHECK (verdict IN ('acceptable','degraded','unclear')),
    reason              TEXT NOT NULL DEFAULT '',
    judge_model         TEXT NOT NULL,
    judge_cost_usd      NUMERIC(12,6),            -- NULL = unmetered, never 0
    baseline_cost_usd   NUMERIC(12,6),
    baseline_dispatched BOOLEAN NOT NULL DEFAULT FALSE,
    optimized_position  TEXT CHECK (optimized_position IN ('a','b')),
    orders_judged       SMALLINT NOT NULL DEFAULT 1,
    orders_agreed       BOOLEAN
);

CREATE INDEX quality_verdicts_swap_idx ON quality_verdicts (org_id, requested_model, served_model, ts);
CREATE INDEX quality_verdicts_request_idx ON quality_verdicts (request_id);
