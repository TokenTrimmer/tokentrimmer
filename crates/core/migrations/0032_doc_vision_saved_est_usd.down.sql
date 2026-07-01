-- Revert: drop the Document Lane vision-avoided estimated-savings column.
ALTER TABLE request_logs
  DROP COLUMN doc_vision_saved_est_usd;
