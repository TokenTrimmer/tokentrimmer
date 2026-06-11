-- Revert: drop the canary traffic-split + shadow-mode columns from request_logs.

ALTER TABLE request_logs
  DROP COLUMN shadow_model,
  DROP COLUMN shadow_cost_usd,
  DROP COLUMN traffic_split_arm;
