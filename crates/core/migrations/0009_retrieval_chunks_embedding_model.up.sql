-- Add embedding_model column to retrieval_chunks (mirrors 0007 for cache_entries).
--
-- Without this column the gateway cannot tell which embedding model produced a
-- chunk's vector. If the operator swaps text-embedding-3-small (1536-dim) for
-- a different model (e.g. text-embedding-3-large, 3072-dim), old chunks and new
-- query vectors would be compared with pgvector's <=> operator and produce
-- meaningless similarity scores / silently wrong retrievals.
--
-- New rows always carry the embedding model name; existing NULL rows are
-- excluded by the search filter (embedding_model = $N never matches NULL) so
-- they cannot produce wrong answers.

ALTER TABLE retrieval_chunks
    ADD COLUMN IF NOT EXISTS embedding_model TEXT;

-- Partial index so the search WHERE clause (org_id, corpus, embedding_model)
-- can prefilter before the HNSW walk. Mirrors cache_entries_model_idx.
CREATE INDEX IF NOT EXISTS retrieval_chunks_model_idx
    ON retrieval_chunks (org_id, corpus, embedding_model)
    WHERE embedding_model IS NOT NULL;
