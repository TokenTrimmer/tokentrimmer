-- Add embedding_model column to cache_entries.
--
-- Without this column the gateway cannot distinguish vectors produced by
-- different embedding models.  If the operator swaps from
-- text-embedding-3-small to text-embedding-3-large (or any other model) the
-- old rows carry 1536-dim vectors while the new rows carry 3072-dim vectors;
-- comparing them with pgvector's <=> operator produces meaningless scores and
-- can return silently incorrect cache hits.
--
-- New rows always carry the embedding model name; existing NULL rows are
-- excluded by the lookup query so they do not produce wrong answers — they
-- will simply expire naturally and be replaced by correctly-tagged inserts.

ALTER TABLE cache_entries
    ADD COLUMN IF NOT EXISTS embedding_model TEXT;

-- Index so the lookup WHERE clause (org_id, model, embedding_model, expires_at)
-- can use a partial index scan before the HNSW walk.
CREATE INDEX IF NOT EXISTS cache_entries_model_idx
    ON cache_entries (org_id, model, embedding_model)
    WHERE embedding_model IS NOT NULL;
