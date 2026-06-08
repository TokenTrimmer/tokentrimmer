# Retrieval finite-embedding guard + BPE cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reject non-finite (NaN/Inf) embeddings in the retrieval path (mirroring the L2-cache poisoning guard) and stop rebuilding the tiktoken BPE on every `chunk()` call.

**Architecture:** A shared `embedding_is_finite` helper guards both the `embed()` chokepoint (covers query + insert embeddings) and each store `insert` (authoritative store invariant), returning a new `RetrievalError::InvalidEmbedding`. `chunking::chunk()` switches to a process-wide `OnceLock`-cached cl100k BPE (mirrors `tt-tokenize`).

**Tech Stack:** Rust (`crates/retrieval` = `tt-retrieval`), tiktoken-rs 0.5, httpmock, thiserror.

Spec: `docs/superpowers/specs/2026-06-07-retrieval-finite-guard-bpe-cache-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`** — run it before committing. The postgres `insert` is behind the `postgres` feature; run the postgres-feature gates too (commands below). One cohesive slice, two parts.

---

### Task 1: Finite-embedding guard (Part A) + BPE cache (Part B)

**Files:**
- Modify: `crates/retrieval/src/error.rs` (new variant)
- Modify: `crates/retrieval/src/lib.rs` (shared helper)
- Modify: `crates/retrieval/src/embed.rs` (guard + test)
- Modify: `crates/retrieval/src/store/memory.rs` (guard + test)
- Modify: `crates/retrieval/src/store/postgres.rs` (guard)
- Modify: `crates/retrieval/src/chunking.rs` (BPE cache)

#### Part A — finite guard

- [ ] **Step 1: Add the `InvalidEmbedding` error variant**

In `crates/retrieval/src/error.rs`, add a variant to `RetrievalError` (after `Malformed`):
```rust
    #[error("non-finite embedding (NaN/Inf)")]
    InvalidEmbedding,
```

- [ ] **Step 2: Add the shared `embedding_is_finite` helper**

In `crates/retrieval/src/lib.rs`, after the `pub use …;` block, add:
```rust
/// True if every component is finite (no NaN/Inf). A non-finite embedding makes
/// cosine distance NaN and corrupts top-k ranking, so it is rejected at the
/// embed chokepoint and at store insert (mirrors the L2 cache guard).
pub(crate) fn embedding_is_finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}
```

- [ ] **Step 3: Write the failing tests (embed + memory)**

In `crates/retrieval/src/embed.rs`, inside `#[cfg(test)] mod tests`, add (after `embed_5xx_errors`):
```rust
    #[tokio::test]
    async fn embed_rejects_non_finite() {
        // 1e400 overflows to f32::INFINITY on parse — a realistic non-finite
        // value (JSON has no NaN/Inf literal).
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/embeddings");
                then.status(200).json_body(serde_json::json!({
                    "data": [{ "embedding": [0.1, 1e400, 0.2] }]
                }));
            })
            .await;
        let c = EmbeddingClient {
            api_key: "k".into(),
            base_url: server.base_url(),
            model: "text-embedding-3-small".into(),
            http: reqwest::Client::new(),
        };
        let err = c.embed("hi").await.unwrap_err();
        assert!(matches!(err, RetrievalError::InvalidEmbedding));
    }
```

In `crates/retrieval/src/store/memory.rs`, inside `#[cfg(test)] mod tests`, add (uses the existing `chunk` test helper):
```rust
    #[tokio::test]
    async fn insert_rejects_non_finite_embedding() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        let err = s
            .insert(chunk(o, "x", vec![f32::NAN], "bad", "m"))
            .await
            .unwrap_err();
        assert!(matches!(err, RetrievalError::InvalidEmbedding));
        // A finite embedding still inserts.
        s.insert(chunk(o, "x", vec![1.0], "ok", "m")).await.unwrap();
    }
```

- [ ] **Step 4: Run to confirm they fail**

Run: `cargo test -p tt-retrieval embed_rejects_non_finite insert_rejects_non_finite 2>&1 | tail -20`
Expected: FAIL — `embed` currently returns `Ok([0.1, inf, 0.2])` (assert fails), and memory `insert` returns `Ok(())` for NaN (assert fails). (Both compile — the variant + helper exist from steps 1–2.)

- [ ] **Step 5: Guard `embed()`**

In `crates/retrieval/src/embed.rs`, replace the final returned expression of `embed` (the `parsed.data.into_iter().next().map(|e| e.embedding).ok_or_else(…)`) with:
```rust
        let embedding = parsed
            .data
            .into_iter()
            .next()
            .map(|e| e.embedding)
            .ok_or_else(|| RetrievalError::Embedding("empty data".into()))?;
        if !crate::embedding_is_finite(&embedding) {
            return Err(RetrievalError::InvalidEmbedding);
        }
        Ok(embedding)
```

- [ ] **Step 6: Guard `MemoryStore::insert`**

In `crates/retrieval/src/store/memory.rs`, prepend the check to `insert`:
```rust
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError> {
        if !crate::embedding_is_finite(&chunk.embedding) {
            return Err(RetrievalError::InvalidEmbedding);
        }
        self.chunks.lock().unwrap().push(chunk);
        Ok(())
    }
```

- [ ] **Step 7: Guard `PostgresStore::insert`**

In `crates/retrieval/src/store/postgres.rs`, prepend the check to `insert` (before `let embedding = pgvector::Vector::from(chunk.embedding);`):
```rust
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError> {
        if !crate::embedding_is_finite(&chunk.embedding) {
            return Err(RetrievalError::InvalidEmbedding);
        }
        let embedding = pgvector::Vector::from(chunk.embedding);
```
(Leave the rest of the existing INSERT unchanged.)

- [ ] **Step 8: Run to confirm Part A passes**

Run: `cargo test -p tt-retrieval embed_rejects_non_finite insert_rejects_non_finite 2>&1 | tail -20`
Expected: PASS. Also run `cargo test -p tt-retrieval 2>&1 | tail -10` → all existing retrieval tests still green.

#### Part B — BPE cache

- [ ] **Step 9: Cache the cl100k BPE in `chunking.rs`**

In `crates/retrieval/src/chunking.rs`, add imports + a cached accessor near the top (after the `const`s), and use it in `chunk`.

Add:
```rust
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

/// Process-wide cached cl100k BPE. `None` if it failed to load (then `chunk`
/// falls back to a single whole-text chunk). Mirrors `tt-tokenize`.
fn cl100k() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}
```
Then in `chunk`, replace the opening:
```rust
    let bpe = match tiktoken_rs::cl100k_base() {
        Ok(b) => b,
        Err(_) => {
            return vec![Chunk {
                text: text.into(),
                start_token: 0,
                end_token: 0,
            }]
        }
    };
```
with:
```rust
    let Some(bpe) = cl100k() else {
        return vec![Chunk {
            text: text.into(),
            start_token: 0,
            end_token: 0,
        }];
    };
```
The rest of `chunk` is unchanged (`bpe` is now `&CoreBPE`; `encode_with_special_tokens`/`decode` are `&self` methods).

- [ ] **Step 10: Run the chunking tests (behavior-preserving)**

Run: `cargo test -p tt-retrieval chunking 2>&1 | tail -10`
Expected: PASS — `short_text_one_chunk` + `long_text_multiple_chunks_with_overlap` unchanged/green (no new test; the BPE cache is a perf change verified by these staying green + the OnceLock pattern).

#### Gates + commit

- [ ] **Step 11: Full gates (default + postgres feature; fmt is the recurring CI miss)**

Run: `cargo test -p tt-retrieval 2>&1 | tail -10` → all pass.
Run: `cargo test -p tt-retrieval --features postgres 2>&1 | tail -10` → compiles + passes (the postgres `insert` guard compiles under cfg).
Run: `cargo fmt --check -p tt-retrieval 2>&1 | tail -5` → no diff (if drift: `cargo fmt -p tt-retrieval`, re-check).
Run: `cargo clippy -p tt-retrieval --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean | head` → none.
Run: `cargo clippy -p tt-retrieval --features postgres --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean | head` → none.

- [ ] **Step 12: Commit (stage only the six files)**

```bash
git add crates/retrieval/src/error.rs crates/retrieval/src/lib.rs crates/retrieval/src/embed.rs crates/retrieval/src/store/memory.rs crates/retrieval/src/store/postgres.rs crates/retrieval/src/chunking.rs
git commit -m "fix(retrieval): reject non-finite embeddings + cache the tiktoken BPE

Guard NaN/Inf embeddings at the embed() chokepoint and both store inserts
(new RetrievalError::InvalidEmbedding, mirrors the L2 cache guard). Cache the
cl100k BPE in a OnceLock instead of rebuilding it on every chunk() call.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-retrieval 2>&1 | tail -10
cargo test -p tt-retrieval --features postgres 2>&1 | tail -10
cargo fmt --check -p tt-retrieval
cargo clippy -p tt-retrieval --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean
cargo clippy -p tt-retrieval --features postgres --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean
```
All green / no output. **Stage only the six changed files** (the working tree also carries an unrelated stale `docs/reviews/...audit-checklist.md` edit + a `rust_out` junk file — do NOT stage them).

## Notes for the implementer
- `embedding_is_finite` is `pub(crate)` in `lib.rs` and used by `embed.rs` (always compiled), `store/memory.rs` (always), and `store/postgres.rs` (postgres feature) — always referenced, so no dead-code lint.
- The `embed()` guard covers BOTH insert-bound and query embeddings (all embeddings flow through it); the store guards are the authoritative store invariant for any caller. Both checks intended (defense-in-depth, mirrors L2).
- The BPE cache preserves the exact load-failure fallback (single whole-text chunk) — now cached as `None` so it isn't retried per call.
- Do NOT add a finite-filter inside `search`/`top_k` or change any schema — out of scope (the embed guard covers the query path).
