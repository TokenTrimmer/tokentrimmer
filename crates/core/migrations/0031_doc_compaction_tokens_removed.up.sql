-- Document Lane D2: lossless document-compaction token accounting.
--
-- `doc_compaction_tokens_removed` is the pipeline-MEASURED, token-true-gated
-- count of input tokens the lossless document-compaction pass
-- (`RouteAction::doc_compaction`) removed from LARGE non-prose documents before
-- dispatch. Additive + back-compatible: existing rows (and every request whose
-- route did not opt into doc_compaction) carry 0. The USD value of these
-- removed tokens folds into the saved-usd headline via the same baseline fold
-- as compression and is surfaced separately on
-- `X-TokenTrimmer-Doc-Compaction-Saved-Usd`; this column is the token-denominated
-- record for reconciliation / methodology breakdowns.
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS doc_compaction_tokens_removed BIGINT NOT NULL DEFAULT 0;
