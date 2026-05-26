-- Semantic (L2) cache entries — pgvector-backed.
-- Per spec §4.4 (semantic cache) and §8.2 (Postgres schema).
--
-- Privacy: we store the embedding + the cached provider response, NEVER the
-- original prompt text. The embedding is one-way for human eyes; if an attacker
-- exfiltrates this table they get vectors + responses but no source prompts.
--
-- Index strategy: HNSW with cosine distance. cosine is the right operator
-- because OpenAI text-embedding-3-small (ADR-008) returns L2-normalized vectors
-- where cosine ≡ dot product. m=16 / ef_construction=64 are the pgvector
-- defaults that hit a good recall/build-cost knee for ~1M vectors per org.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE cache_entries (
  id              UUID PRIMARY KEY,
  org_id          UUID         NOT NULL,
  embedding       vector(1536) NOT NULL,
  response        JSONB        NOT NULL,
  model           TEXT         NOT NULL,
  input_tokens    INT          NOT NULL,
  output_tokens   INT          NOT NULL,
  hit_count       BIGINT       NOT NULL DEFAULT 0,
  created_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
  expires_at      TIMESTAMPTZ  NOT NULL
);

-- Per-org isolation: every semantic-lookup query MUST scope by org_id.
-- The composite index supports both org-scoped expiry sweeps and the
-- HNSW-search filter on org_id.
CREATE INDEX cache_entries_org_expires ON cache_entries (org_id, expires_at);

-- HNSW index for cosine similarity. `vector_cosine_ops` is the operator class
-- that matches the `<=>` distance operator we query with.
CREATE INDEX cache_entries_embedding_hnsw
  ON cache_entries
  USING hnsw (embedding vector_cosine_ops)
  WITH (m = 16, ef_construction = 64);

-- Operational TTL: a periodic worker DELETEs WHERE expires_at < now().
-- The (org_id, expires_at) index makes that sweep O(stale-rows) not O(N).
