-- Reverse 0036: drop the quality_verdict column from workflow_runs.
-- Frozen v2 receipts already minted stay valid (the signature is over the
-- canonical payload the mint rebuilt at mint-time; the stored column is only
-- the FREEZE-serve optimization). Future runs lose the persisted verdict.
ALTER TABLE workflow_runs
  DROP COLUMN IF EXISTS quality_verdict;
