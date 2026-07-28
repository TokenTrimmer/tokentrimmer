-- Reverse the gateway-owned cost-breakdown columns.
--
-- In a shared deployment, reverse cloud migration 0048 before this migration
-- so an older cloud control plane is not left selecting columns it no longer
-- has. `IF EXISTS` keeps a rollback after an older/public-only installation
-- idempotent.
ALTER TABLE request_logs
  DROP COLUMN IF EXISTS summarizer_tax_usd;
ALTER TABLE request_logs
  DROP COLUMN IF EXISTS doc_compaction_saved_usd;
ALTER TABLE request_logs
  DROP COLUMN IF EXISTS flex_saved_usd;
