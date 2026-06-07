# Retrieval embedding-model partitioning — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** Audit-remediation Wave 3 (public repo, `crates/retrieval` + one migration). The sibling HIGH finding to the k-clamp (#69). Tag each retrieval chunk with the embedding model that produced its vector, and filter searches by that model — so vectors from different embedding models (different dimensionality / geometry) are never compared with pgvector's `<=>`.

## Goal

Mirror the L2 semantic-cache fix (migration `0007_cache_embedding_model`, `tt_cache::PostgresL2Cache`) for the retrieval store. Without this, if the operator swaps the embedder (e.g. `text-embedding-3-small` 1536-dim → `text-embedding-3-large` 3072-dim), old chunks and new query vectors get compared with `<=>` and produce meaningless similarity scores / silently wrong retrievals.

**Preventive, not live-exploitable today:** the production *indexing* path that inserts chunks is not yet wired — `chunking::chunk()` and `RetrievalStore::insert` have no non-test caller. This slice adds the schema + threading + filter guard (and tests) so that whenever indexing is wired, cross-model comparison is impossible. It does **not** add a production indexer.

## Background (verified)

- L2 precedent — `crates/core/migrations/0007_cache_embedding_model.up.sql`: `ALTER TABLE cache_entries ADD COLUMN IF NOT EXISTS embedding_model TEXT;` + partial index `(org_id, model, embedding_model) WHERE embedding_model IS NOT NULL`. Lookup filters by `embedding_model`; legacy NULL rows excluded (they expire/replace naturally).
- `crates/core/migrations/0005_retrieval_chunks.up.sql`: `retrieval_chunks(id, org_id, corpus, doc_id, chunk_idx, text, embedding vector(1536), metadata, created_at)`; indexes `(org_id, corpus)` and HNSW `vector_cosine_ops`. Latest migration on disk is `0008` → **next number is `0009`**.
- `crates/retrieval/src/types.rs`: `Chunk { id, org_id, corpus, doc_id, chunk_idx, text, embedding: Vec<f32>, metadata }` — **no model field**. `RetrievalResult` (search output) has no model field and does not need one.
- `crates/retrieval/src/store/mod.rs`: `trait RetrievalStore { insert(Chunk); search(org_id, corpus, q, k); delete_corpus(org_id, corpus) }`.
- `crates/retrieval/src/store/postgres.rs`: `insert` binds 8 cols ($1–$8). `search` binds org=$1, corpus=$2, query_vec=$3 (used twice), limit=$4; `WHERE org_id=$1 AND corpus=$2`.
- `crates/retrieval/src/store/memory.rs`: `search` filters `c.org_id == org_id && c.corpus == corpus`. Test helper `chunk(...)`.
- `crates/retrieval/src/search.rs::top_k(store, org_id, corpus, query_embedding, k, min_similarity)` → `store.search(...)`. Test helper `c(...)`.
- `crates/retrieval/src/substitute.rs`: holds `embedder: &EmbeddingClient` (which has `.model: String`); calls `top_k(store, org_id, &t.corpus, &query_emb, t.k as usize, floor)` at `:109`. Test helpers `mock_embedder` (model `"x"`), `chunk(...)` (`:181`), and one inline `Chunk { ... }` literal (`:462`).
- `crates/retrieval/src/embed.rs`: `EmbeddingClient { api_key, base_url, model, http }`; `openai()` sets `model: "text-embedding-3-small"`. This `.model` is the value recorded per chunk and filtered on at search.

## Architecture

Decision (user): `Chunk.embedding_model` is a non-optional `String` (every insert knows its model). The DB column is nullable only to tolerate any pre-existing rows, which the search filter excludes.

### 1. Migration `0009_retrieval_chunks_embedding_model.{up,down}.sql`
Up (mirrors 0007):
```sql
ALTER TABLE retrieval_chunks
    ADD COLUMN IF NOT EXISTS embedding_model TEXT;

CREATE INDEX IF NOT EXISTS retrieval_chunks_model_idx
    ON retrieval_chunks (org_id, corpus, embedding_model)
    WHERE embedding_model IS NOT NULL;
```
Down:
```sql
DROP INDEX IF EXISTS retrieval_chunks_model_idx;
ALTER TABLE retrieval_chunks DROP COLUMN IF EXISTS embedding_model;
```

### 2. `Chunk` (`types.rs`)
Add `pub embedding_model: String,` after `embedding`.

### 3. `RetrievalStore` trait (`store/mod.rs`)
`search` gains a trailing `embedding_model: &str` param. `insert` is unchanged (it takes the whole `Chunk`, which now carries the model). `delete_corpus` unchanged.

### 4. Postgres store (`store/postgres.rs`)
- `insert`: add `embedding_model` to the column list and bind `&chunk.embedding_model` (now 9 cols, $1–$9).
- `search(... , embedding_model: &str)`: add `AND embedding_model = $5` to the `WHERE`, bind `embedding_model` as `$5`. (org=$1, corpus=$2, query_vec=$3, limit=$4, model=$5.) NULL legacy rows can never match — exactly the cache semantics.

### 5. Memory store (`store/memory.rs`)
- `search(... , embedding_model: &str)`: extend the filter to `c.org_id == org_id && c.corpus == corpus && c.embedding_model == embedding_model`.
- `insert` unchanged.

### 6. `top_k` (`search.rs`)
Add trailing `embedding_model: &str`; forward to `store.search(org_id, corpus, query_embedding, k, embedding_model)`. (Keeps the k-clamp line from #69.)

### 7. `substitute.rs`
At the `top_k` call (`:109`), pass `&embedder.model` as the final arg.

## Data flow
Index (future): chunk built with `embedding_model = <embedder model>` → `insert` persists it. Query: `substitute` embeds with `embedder` → calls `top_k(..., &embedder.model)` → store filters `embedding_model = that model`. A chunk embedded by a different model is invisible to the query. Consistent by construction: the same `EmbeddingClient.model` is used to embed and to filter.

## Error handling
No new error modes. Cross-model rows simply don't match the filter (return fewer/zero hits) rather than producing wrong scores — the safe failure. Legacy NULL rows are excluded identically to the cache.

## Testing (pure — no DB)
- `memory.rs`: new test — insert two chunks in the same `(org, corpus)` with embedding_models `"m-a"` and `"m-b"`; `search(..., "m-a")` returns only the `"m-a"` chunk. (Partition correctness.)
- `memory.rs`: existing tests updated — `chunk(...)` helper sets `embedding_model`, and the existing `search` calls pass a model arg (use the same model the helper sets so current assertions hold).
- `search.rs`: `top_k` test helper `c(...)` sets `embedding_model`; existing `min_similarity_filter` + the #69 `top_k_clamps_oversized_k` pass a model arg matching the inserted chunks.
- `substitute.rs`: `chunk(...)` helper + the `:462` `Chunk {}` literal set `embedding_model` to `"x"` (the `mock_embedder` model), so substitution tests still retrieve. Add one test: a chunk indexed under a *different* model than the embedder is not retrieved (its span is left intact / counted as a low-confidence skip).
- DB-backed (`#[ignore]` + `TEST_DATABASE_URL`) tests are **not** added — retrieval has none today; the memory store + the migration's mirror of the proven 0007 pattern are the coverage. (Migration correctness is exercised by the cloud ephemeral-Neon `--migrate-only` run on the cloud side; on the public side the SQL mirrors 0007 exactly.)

Gates: `cargo test -p tt-retrieval`; `cargo clippy -p tt-retrieval --all-targets -- -D warnings`; `cargo fmt -p tt-retrieval -- --check`. Migration files are also picked up by the plan-replay / migration CI if applicable.

## Out of scope
- Production indexing endpoint / `chunking::chunk()` wiring (no caller exists — separate future work).
- The `chunking::chunk()` BPE-cache rebuild perf finding (medium — separate follow-up).
- Backfilling `embedding_model` on any existing rows (none in a deploy-blocked platform; NULL rows are simply excluded).
- Re-embedding / model-migration tooling.
- Adding `embedding_model` to `RetrievalResult` (search output) — not needed by any consumer.
