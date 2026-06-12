DROP INDEX IF EXISTS quality_verdicts_cache_entry_idx;
ALTER TABLE quality_verdicts
    DROP COLUMN IF EXISTS hit_similarity,
    DROP COLUMN IF EXISTS cache_entry_id;
ALTER TABLE cache_entries
    DROP COLUMN IF EXISTS lexical_sig;
