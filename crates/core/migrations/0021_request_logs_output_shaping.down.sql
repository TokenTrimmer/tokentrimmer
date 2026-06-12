ALTER TABLE request_logs
  DROP COLUMN diff_failed_cost_usd,
  DROP COLUMN diff_failed,
  DROP COLUMN diff_saved_usd,
  DROP COLUMN diff_applied,
  DROP COLUMN format_switch_saved_est_usd,
  DROP COLUMN format_switched;
