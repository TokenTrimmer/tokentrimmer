# Retrieval embedding-model partitioning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tag each retrieval chunk with the embedding model that produced its vector and filter searches by that model, so vectors from different embedding models are never compared.

**Architecture:** Migration 0009 adds a nullable `embedding_model` column + partial index to `retrieval_chunks` (mirroring the L2-cache 0007). `Chunk` gains a non-optional `embedding_model: String`. `RetrievalStore::search` and `top_k` gain a trailing `embedding_model: &str`; both store impls filter on it (Postgres `AND embedding_model = $5`, which excludes legacy NULL rows; memory `==`). `substitute` passes `&embedder.model`. The Rust change is one atomic compile unit because the trait-signature change ripples through every impl and caller.

**Tech Stack:** Rust, sqlx/pgvector, `tokio`/`uuid`/`httpmock` (existing crate deps).

Spec: `docs/superpowers/specs/2026-06-06-retrieval-embedding-model-partition-design.md`

---

### Task 1: Migration 0009 — `embedding_model` column + index

**Files:**
- Create: `crates/core/migrations/0009_retrieval_chunks_embedding_model.up.sql`
- Create: `crates/core/migrations/0009_retrieval_chunks_embedding_model.down.sql`

- [ ] **Step 1: Write the up migration**

Create `crates/core/migrations/0009_retrieval_chunks_embedding_model.up.sql`:

```sql
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
```

- [ ] **Step 2: Write the down migration**

Create `crates/core/migrations/0009_retrieval_chunks_embedding_model.down.sql`:

```sql
-- Revert migration 0009: remove embedding_model column and its index.
DROP INDEX IF EXISTS retrieval_chunks_model_idx;
ALTER TABLE retrieval_chunks DROP COLUMN IF EXISTS embedding_model;
```

- [ ] **Step 3: Verify the files exist and the crate still builds**

Run: `ls crates/core/migrations/0009_retrieval_chunks_embedding_model.*`
Expected: both `.up.sql` and `.down.sql` listed.

Run: `cargo build -p tt-core 2>&1 | tail -5`
Expected: builds clean (sqlx `migrate!` macro picks up the new files at compile time if used; no error).

- [ ] **Step 4: Commit**

```bash
git add crates/core/migrations/0009_retrieval_chunks_embedding_model.up.sql crates/core/migrations/0009_retrieval_chunks_embedding_model.down.sql
git commit -m "feat(retrieval): migration 0009 — embedding_model column + index

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Thread `embedding_model` through Chunk, store, top_k, substitute

**Files:**
- Modify: `crates/retrieval/src/types.rs`
- Modify: `crates/retrieval/src/store/mod.rs`
- Modify: `crates/retrieval/src/store/memory.rs`
- Modify: `crates/retrieval/src/store/postgres.rs`
- Modify: `crates/retrieval/src/search.rs`
- Modify: `crates/retrieval/src/substitute.rs`

This is one atomic compile unit: the `RetrievalStore::search` signature change forces all impls and callers to change together. The "red" state is a compile failure against the new signatures/field; "green" is the crate compiling with the partition filter in place and all tests passing.

- [ ] **Step 1: Write the new memory-store partition test (RED — won't compile yet)**

In `crates/retrieval/src/store/memory.rs`, update the test helper `chunk` to take an `embedding_model` and set it, and add a partition test. Replace the existing test helper:

```rust
    fn chunk(org: Uuid, corpus: &str, emb: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(),
            org_id: org,
            corpus: corpus.into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: text.into(),
            embedding: emb,
            metadata: json!({}),
        }
    }
```

with:

```rust
    fn chunk(org: Uuid, corpus: &str, emb: Vec<f32>, text: &str, model: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(),
            org_id: org,
            corpus: corpus.into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: text.into(),
            embedding: emb,
            embedding_model: model.into(),
            metadata: json!({}),
        }
    }
```

Update the three existing memory tests to pass a model to `chunk(...)` and to `search(...)`. Replace the bodies of `search_returns_highest_similarity_first`, `search_isolates_by_org_and_corpus`, and `delete_corpus_returns_removed_count` as follows:

```rust
    #[tokio::test]
    async fn search_returns_highest_similarity_first() {
        let s = MemoryStore::new();
        let org = Uuid::new_v4();
        s.insert(chunk(org, "x", vec![1.0, 0.0], "first", "m"))
            .await
            .unwrap();
        s.insert(chunk(org, "x", vec![0.0, 1.0], "second", "m"))
            .await
            .unwrap();
        s.insert(chunk(org, "x", vec![0.9, 0.1], "third", "m"))
            .await
            .unwrap();
        let r = s.search(org, "x", &[1.0, 0.0], 2, "m").await.unwrap();
        assert_eq!(r[0].text, "first");
        assert_eq!(r[1].text, "third");
    }

    #[tokio::test]
    async fn search_isolates_by_org_and_corpus() {
        let s = MemoryStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        s.insert(chunk(a, "x", vec![1.0], "a-x", "m")).await.unwrap();
        s.insert(chunk(b, "x", vec![1.0], "b-x", "m")).await.unwrap();
        s.insert(chunk(a, "y", vec![1.0], "a-y", "m")).await.unwrap();
        let r = s.search(a, "x", &[1.0], 10, "m").await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "a-x");
    }

    #[tokio::test]
    async fn delete_corpus_returns_removed_count() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        s.insert(chunk(o, "x", vec![1.0], "1", "m")).await.unwrap();
        s.insert(chunk(o, "x", vec![1.0], "2", "m")).await.unwrap();
        s.insert(chunk(o, "y", vec![1.0], "y", "m")).await.unwrap();
        let removed = s.delete_corpus(o, "x").await.unwrap();
        assert_eq!(removed, 2);
    }

    #[tokio::test]
    async fn search_partitions_by_embedding_model() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        s.insert(chunk(o, "x", vec![1.0, 0.0], "from-a", "m-a"))
            .await
            .unwrap();
        s.insert(chunk(o, "x", vec![1.0, 0.0], "from-b", "m-b"))
            .await
            .unwrap();
        let r = s.search(o, "x", &[1.0, 0.0], 10, "m-a").await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "from-a");
    }
```

- [ ] **Step 2: Run to confirm RED (compile failure)**

Run: `cargo test -p tt-retrieval --lib store::memory 2>&1 | tail -15`
Expected: FAIL — compile errors: `Chunk` has no field `embedding_model`, `chunk` takes 4 args not 5, `search` takes 4 args not 5. (This is the expected red against the not-yet-changed types/signatures.)

- [ ] **Step 3: Add the `embedding_model` field to `Chunk`**

In `crates/retrieval/src/types.rs`, in `struct Chunk`, add the field after `embedding`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: Uuid,
    pub org_id: Uuid,
    pub corpus: String,
    pub doc_id: Uuid,
    pub chunk_idx: u32,
    pub text: String,
    pub embedding: Vec<f32>, // 1536-dim
    pub embedding_model: String,
    pub metadata: serde_json::Value,
}
```

- [ ] **Step 4: Change the `RetrievalStore::search` trait signature**

In `crates/retrieval/src/store/mod.rs`, change the `search` method signature to add the trailing param:

```rust
    async fn search(
        &self,
        org_id: Uuid,
        corpus: &str,
        query_embedding: &[f32],
        k: usize,
        embedding_model: &str,
    ) -> Result<Vec<RetrievalResult>, RetrievalError>;
```

- [ ] **Step 5: Update the memory store impl (signature + filter)**

In `crates/retrieval/src/store/memory.rs`, change the `search` impl to accept the param and filter on it:

```rust
    async fn search(
        &self,
        org_id: Uuid,
        corpus: &str,
        q: &[f32],
        k: usize,
        embedding_model: &str,
    ) -> Result<Vec<RetrievalResult>, RetrievalError> {
        let snap: Vec<_> = self.chunks.lock().unwrap().clone();
        let mut scored: Vec<(f32, &Chunk)> = snap
            .iter()
            .filter(|c| {
                c.org_id == org_id && c.corpus == corpus && c.embedding_model == embedding_model
            })
            .map(|c| (cosine(q, &c.embedding), c))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let out = scored
            .into_iter()
            .take(k)
            .map(|(s, c)| RetrievalResult {
                chunk_id: c.id,
                doc_id: c.doc_id,
                chunk_idx: c.chunk_idx,
                text: c.text.clone(),
                similarity: s,
            })
            .collect();
        Ok(out)
    }
```

- [ ] **Step 6: Update the Postgres store impl (insert binds col; search filters)**

In `crates/retrieval/src/store/postgres.rs`, change `insert` to include the new column:

```rust
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError> {
        let embedding = pgvector::Vector::from(chunk.embedding);
        sqlx::query(
            r#"INSERT INTO retrieval_chunks
                 (id, org_id, corpus, doc_id, chunk_idx, text, embedding, embedding_model, metadata)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
               ON CONFLICT (id) DO NOTHING"#,
        )
        .bind(chunk.id)
        .bind(chunk.org_id)
        .bind(&chunk.corpus)
        .bind(chunk.doc_id)
        .bind(i32::try_from(chunk.chunk_idx).unwrap_or(i32::MAX))
        .bind(&chunk.text)
        .bind(embedding)
        .bind(&chunk.embedding_model)
        .bind(&chunk.metadata)
        .execute(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(())
    }
```

Then change `search` to accept the param and filter (note the bind positions: org=$1, corpus=$2, query_vec=$3, limit=$4, model=$5):

```rust
    async fn search(
        &self,
        org_id: Uuid,
        corpus: &str,
        q: &[f32],
        k: usize,
        embedding_model: &str,
    ) -> Result<Vec<RetrievalResult>, RetrievalError> {
        let query_vec = pgvector::Vector::from(q.to_vec());
        // Raise ef_search for THIS query only (SET LOCAL is transaction-scoped),
        // so the org/corpus-filtered HNSW search keeps high recall.
        let mut tx = self.pool.begin().await.map_err(store_err)?;
        sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", self.ef_search))
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;
        let rows: Vec<(Uuid, Uuid, i32, String, f32)> = sqlx::query_as(
            r#"SELECT id, doc_id, chunk_idx, text,
                      CAST(1.0 - (embedding <=> $3) AS REAL) AS similarity
                 FROM retrieval_chunks
                WHERE org_id = $1 AND corpus = $2 AND embedding_model = $5
                ORDER BY embedding <=> $3
                LIMIT $4"#,
        )
        .bind(org_id)
        .bind(corpus)
        .bind(query_vec)
        .bind(i64::try_from(k).unwrap_or(i64::MAX))
        .bind(embedding_model)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_err)?;
        tx.commit().await.map_err(store_err)?;

        Ok(rows
            .into_iter()
            .map(
                |(chunk_id, doc_id, chunk_idx, text, similarity)| RetrievalResult {
                    chunk_id,
                    doc_id,
                    chunk_idx: chunk_idx.max(0) as u32,
                    text,
                    similarity,
                },
            )
            .collect())
    }
```

- [ ] **Step 7: Update `top_k` to thread the param**

In `crates/retrieval/src/search.rs`, change the `top_k` signature and the `store.search` call (keep the k-clamp line from #69):

```rust
pub async fn top_k(
    store: &dyn RetrievalStore,
    org_id: Uuid,
    corpus: &str,
    query_embedding: &[f32],
    k: usize,
    min_similarity: f32,
    embedding_model: &str,
) -> Result<Vec<RetrievalResult>, RetrievalError> {
    let k = k.min(crate::tags::MAX_RETRIEVAL_K as usize);
    let raw = store
        .search(org_id, corpus, query_embedding, k, embedding_model)
        .await?;
    Ok(raw
        .into_iter()
        .filter(|r| r.similarity >= min_similarity)
        .collect())
}
```

Then update the `search.rs` test helper `c` and the two existing tests. Replace the helper:

```rust
    fn c(org: Uuid, emb: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(),
            org_id: org,
            corpus: "x".into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: text.into(),
            embedding: emb,
            metadata: json!({}),
        }
    }
```

with (add the field; the helper always uses model `"m"`):

```rust
    fn c(org: Uuid, emb: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(),
            org_id: org,
            corpus: "x".into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: text.into(),
            embedding: emb,
            embedding_model: "m".into(),
            metadata: json!({}),
        }
    }
```

Update the two existing tests' `top_k(...)` calls to pass `"m"` as the trailing arg:

```rust
    #[tokio::test]
    async fn min_similarity_filter() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        s.insert(c(o, vec![1.0, 0.0], "hi-sim")).await.unwrap();
        s.insert(c(o, vec![0.0, 1.0], "low-sim")).await.unwrap();
        let r = top_k(&s, o, "x", &[1.0, 0.0], 5, 0.5, "m").await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "hi-sim");
    }

    #[tokio::test]
    async fn top_k_clamps_oversized_k() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        // Insert more chunks than MAX_RETRIEVAL_K so an unclamped k would return
        // all of them. min_similarity = 0.0 keeps every hit.
        for i in 0..(crate::tags::MAX_RETRIEVAL_K as usize + 10) {
            s.insert(c(o, vec![1.0, 0.0], &format!("chunk-{i}")))
                .await
                .unwrap();
        }
        let r = top_k(&s, o, "x", &[1.0, 0.0], 10_000, 0.0, "m")
            .await
            .unwrap();
        assert!(r.len() <= crate::tags::MAX_RETRIEVAL_K as usize);
    }
```

- [ ] **Step 8: Update `substitute.rs` (production call + test helpers + new cross-model test)**

In `crates/retrieval/src/substitute.rs`, change the production `top_k` call at the retrieval loop to pass the embedder's model:

```rust
            let floor = t.min_similarity.unwrap_or(DEFAULT_MIN_SIMILARITY);
            let hits = top_k(
                store,
                org_id,
                &t.corpus,
                &query_emb,
                t.k as usize,
                floor,
                &embedder.model,
            )
            .await?;
```

Update the test helper `chunk` (around `:181`) to set `embedding_model` to `"x"` (the model `mock_embedder` uses):

```rust
    fn chunk(org: uuid::Uuid, corpus: &str, emb: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            id: uuid::Uuid::new_v4(),
            org_id: org,
            corpus: corpus.into(),
            doc_id: uuid::Uuid::new_v4(),
            chunk_idx: 0,
            text: text.into(),
            embedding: emb,
            embedding_model: "x".into(),
            metadata: json!({}),
        }
    }
```

For the inline `Chunk { ... }` literal further down (around `:462`), add `embedding_model: "x".into(),` after its `embedding:` field. (Find the literal `.insert(Chunk {` block and add the field; it must match the `mock_embedder` model `"x"` so that test still retrieves.)

Add a new test at the end of the `mod tests` block (before the closing `}`) proving cross-model chunks are not retrieved. It mirrors the structure of the existing retrieval tests in this file — a `mock_embedder` returning `[1.0, 0.0]` (model `"x"`), a store seeded with a chunk under a *different* model `"other"`, and an assertion that the tag span is left intact (not substituted):

```rust
    #[tokio::test]
    async fn cross_model_chunk_is_not_retrieved() {
        let server = MockServer::start_async().await;
        let embedder = mock_embedder(&server, vec![1.0, 0.0]).await; // model "x"
        let store = MemoryStore::new();
        let org = uuid::Uuid::new_v4();
        // Chunk indexed under a DIFFERENT embedding model than the query embedder.
        store
            .insert(Chunk {
                id: uuid::Uuid::new_v4(),
                org_id: org,
                corpus: "docs".into(),
                doc_id: uuid::Uuid::new_v4(),
                chunk_idx: 0,
                text: "would-be-retrieved".into(),
                embedding: vec![1.0, 0.0],
                embedding_model: "other".into(),
                metadata: json!({}),
            })
            .await
            .unwrap();

        let mut messages = vec![json!({
            "role": "user",
            "content": r#"<retrievable corpus="docs">original-payload</retrievable>"#
        })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder)
            .await
            .unwrap();

        // The cross-model chunk is invisible → nothing clears the floor → the
        // original payload is left intact and counted as a low-confidence skip.
        assert_eq!(report.substitutions, 0);
        assert_eq!(report.low_confidence_skips, 1);
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("original-payload"));
    }
```

- [ ] **Step 9: Run to confirm GREEN**

Run: `cargo test -p tt-retrieval 2>&1 | tail -15`
Expected: PASS — all tests green, including the new `search_partitions_by_embedding_model` (memory) and `cross_model_chunk_is_not_retrieved` (substitute).

- [ ] **Step 10: Run the gates**

Run: `cargo clippy -p tt-retrieval --all-targets -- -D warnings 2>&1 | tail -15`
Expected: no warnings.

Run: `cargo fmt -p tt-retrieval -- --check 2>&1 | tail -5`
Expected: clean. If it shows a diff, run `cargo fmt -p tt-retrieval` and re-stage only `crates/retrieval/` files.

- [ ] **Step 11: Confirm no other crate calls the changed signatures**

The retrieval public API (`top_k`, `RetrievalStore::search`, `Chunk`) is consumed by the gateway via `crates/core/src/middleware/retrieval.rs`, which uses the store through `substitute_in_messages` (not `top_k`/`search`/`Chunk` directly). Verify nothing else broke:

Run: `cargo build -p tt-core 2>&1 | tail -10`
Expected: builds clean (the middleware constructs the store and calls `substitute_in_messages`; it does not call `search`/`top_k`/construct `Chunk` directly, so it is unaffected).

If `cargo build -p tt-core` reports an error about a `Chunk` literal or a `search`/`top_k` call outside `crates/retrieval`, update that call site to set `embedding_model` / pass the model arg (use the model from the `EmbeddingClient` in scope), then re-run.

- [ ] **Step 12: Commit**

```bash
git add crates/retrieval/src/types.rs crates/retrieval/src/store/mod.rs crates/retrieval/src/store/memory.rs crates/retrieval/src/store/postgres.rs crates/retrieval/src/search.rs crates/retrieval/src/substitute.rs
git commit -m "feat(retrieval): partition chunks/search by embedding_model

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Notes for the implementer

- `cargo fmt --check` is a public-repo CI gate. Run `cargo fmt -p tt-retrieval` before the final commit if `--check` shows a diff; stage only the files you edited.
- Do NOT whole-workspace `cargo fmt`.
- No production indexer is added — `RetrievalStore::insert` / `chunking::chunk()` still have no non-test caller. This slice is the schema + filter guard only.
- The DB column is nullable; the `embedding_model = $5` filter excludes any NULL legacy rows automatically (same as the L2 cache). No backfill.
- If Step 11 surfaces an unexpected external caller of `Chunk`/`search`/`top_k`, that is a real integration point — fix the call site rather than reverting the signature.
