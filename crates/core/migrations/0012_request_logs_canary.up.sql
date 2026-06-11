-- Add canary traffic-split + shadow-mode columns to request_logs (#454).
--
-- All three are ADDITIVE and NULLABLE so existing rows and older deploys
-- continue to INSERT/SELECT unchanged (no DEFAULT/backfill needed — the
-- gateway writes NULL for the >99% of traffic with no canary route):
--
--   * shadow_model       — the discarded shadow-mode candidate model that was
--                          ALSO dispatched (RouteAction::shadow_model). NULL
--                          when no shadow fired.
--   * shadow_cost_usd    — the shadow dispatch's cost, kept SEPARATE from
--                          cost_usd so the doubled spend is attributable to the
--                          experiment, not folded into served-traffic cost.
--   * traffic_split_arm  — 'canary' / 'control' arm the sticky traffic_pct split
--                          assigned (RouteAction::traffic_pct). NULL when the
--                          route declared no split (or no route matched).

ALTER TABLE request_logs
  ADD COLUMN shadow_model      TEXT          NULL,
  ADD COLUMN shadow_cost_usd   NUMERIC(12,6) NULL,
  ADD COLUMN traffic_split_arm TEXT          NULL;
