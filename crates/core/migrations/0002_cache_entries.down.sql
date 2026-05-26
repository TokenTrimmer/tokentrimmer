-- Reverse of 0002_cache_entries.up.sql. We do NOT drop the pgvector extension
-- here — other migrations or external tooling may rely on it. Dropping the
-- extension is an explicit operator decision.

DROP INDEX IF EXISTS cache_entries_embedding_hnsw;
DROP INDEX IF EXISTS cache_entries_org_expires;
DROP TABLE IF EXISTS cache_entries;
