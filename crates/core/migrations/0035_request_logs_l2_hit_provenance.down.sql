-- Reverse 0035: drop the L2-hit provenance columns.
-- Receipts already minted from these fields remain valid (the signature is over
-- the canonical payload the mint endpoint rebuilt at mint-time — see
-- compression_receipt's "recompute-each-time, no ledger freeze, v1" note); only
-- future L2 hits lose the persisted provenance.
ALTER TABLE request_logs
  DROP COLUMN IF EXISTS l2_verdict;
ALTER TABLE request_logs
  DROP COLUMN IF EXISTS l2_similarity;
ALTER TABLE request_logs
  DROP COLUMN IF EXISTS l2_matched_entry_id;
