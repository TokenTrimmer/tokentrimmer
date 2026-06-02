-- Retrieval (RAG / context-compression) chunk store — pgvector-backed.
-- Sibling of cache_entries (0002): same HNSW/cosine strategy and per-org
-- isolation, but a DIFFERENT purpose — this holds CORPUS chunks (a user's
-- codebase / docs) used to retrieve context for prompt substitution, not
-- cached provider responses. Per the Track E spec.
--
-- Index strategy mirrors cache_entries: HNSW cosine (`vector_cosine_ops` ↔ the
-- `<=>` operator). OpenAI text-embedding-3-small (1536-dim, L2-normalized) is
-- the embedder, so cosine ≡ dot product. m=16 / ef_construction=64 are the
-- pgvector defaults at a good recall/build-cost knee.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE retrieval_chunks (
  id          UUID PRIMARY KEY,
  org_id      UUID         NOT NULL,
  corpus      TEXT         NOT NULL,
  doc_id      UUID         NOT NULL,
  chunk_idx   INT          NOT NULL,
  text        TEXT         NOT NULL,
  embedding   vector(1536) NOT NULL,
  metadata    JSONB        NOT NULL DEFAULT '{}'::jsonb,
  created_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- Per-org + per-corpus isolation: every search MUST scope by (org_id, corpus).
-- This composite index supports both the filter and corpus-delete sweeps.
CREATE INDEX retrieval_chunks_org_corpus ON retrieval_chunks (org_id, corpus);

-- HNSW cosine index matching the `<=>` distance operator the search uses.
CREATE INDEX retrieval_chunks_embedding_hnsw
  ON retrieval_chunks
  USING hnsw (embedding vector_cosine_ops)
  WITH (m = 16, ef_construction = 64);
