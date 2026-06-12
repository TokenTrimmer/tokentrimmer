-- Add the advisory batch-eligibility columns to request_logs (research
-- Phase 2.1 — `RouteAction::batch` marker).
--
-- `batch_eligible` records that a matched route requested the advisory
-- batch-eligibility marker AND the request survived the gateway's hard
-- ineligibility gate (non-streaming, non-interactive). The gateway is
-- synchronous today — an eligible request was STILL dispatched and billed
-- normally; the column marks route intent for the future async Batch Lane.
--
-- `batch_forgone_usd` is the FORGONE Batch-API discount (realized cost −
-- catalog batch-rate cost on the full prompt+completion, floored at 0,
-- fee-applied): a projection for the future async Batch Lane, NEVER part of
-- cost_usd / baseline_cost_usd / the saved-usd headline (those reconcile
-- against the realized provider invoice). It is priced from the served
-- model's REAL catalog batch rate — a served model with no batch tier (e.g.
-- after failover) records 0, never a fabricated 0.5x.
--
-- /costs + digest may SUM(batch_forgone_usd) WHERE batch_eligible to say
-- "$X/mo would be saved by the Batch Lane". false/0 for rows predating this
-- migration and for all unmarked / streaming / interactive traffic and TT
-- cache hits.

ALTER TABLE request_logs
  ADD COLUMN batch_eligible    BOOLEAN       NOT NULL DEFAULT false,
  ADD COLUMN batch_forgone_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
