-- Revert migration 0013: drop the judge-verdict columns + their partial index.
DROP INDEX IF EXISTS cache_entries_judge_degraded;
ALTER TABLE cache_entries
    DROP COLUMN IF EXISTS judge_verdict,
    DROP COLUMN IF EXISTS quality_score;
