-- L2 false-positive verify gate (research Phase 2.2).
-- cache_entries.lexical_sig: one-way 64-bit SimHash of the canonicalized
-- embedded context text — NEVER the text itself (privacy invariant of 0002
-- stands: vectors + responses + a 64-bit sketch, no source prompts).
-- NULL for pre-0017 rows; the verify gate fails open on NULL.
ALTER TABLE cache_entries
    ADD COLUMN IF NOT EXISTS lexical_sig BIGINT;

-- quality_verdicts: attribute a judged-from-L2 verdict to the entry that
-- served it + the hit similarity, so FP-rate analysis is durable/auditable.
-- NULL on dispatch-path verdicts. Additive + nullable.
ALTER TABLE quality_verdicts
    ADD COLUMN IF NOT EXISTS cache_entry_id UUID,
    ADD COLUMN IF NOT EXISTS hit_similarity REAL;

CREATE INDEX IF NOT EXISTS quality_verdicts_cache_entry_idx
    ON quality_verdicts (cache_entry_id)
    WHERE cache_entry_id IS NOT NULL;
