-- Gateway-owned request-log cost-breakdown columns.
--
-- The gateway writes these three values for every request, so their schema
-- belongs with the gateway's `request_logs` owner. Cloud migration 0048 added
-- the same columns with `IF NOT EXISTS` for legacy hosted databases; retain
-- that historical repair, but a clean self-hosted gateway install must not
-- depend on the cloud control-plane migrator to create columns its own writer
-- selects/inserts.
--
-- Additive and defaulted so existing rows remain compatible. The downstream
-- cloud migration remains harmless in either ordering because it is also
-- `IF NOT EXISTS`.
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS flex_saved_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS doc_compaction_saved_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS summarizer_tax_usd NUMERIC(12,6) NOT NULL DEFAULT 0;
