# Retrieval finite-embedding guard + BPE cache — Design

**Status:** approved (design)
**Date:** 2026-06-07
**Slice:** Audit-remediation Wave 4 (public repo, `crates/retrieval`). Closes two findings: *"Retrieval insert does not reject non-finite embeddings"* (bug/medium) and *"chunking::chunk() rebuilds the tiktoken BPE on every call"* (perf/medium).

## Background (verified against current code)
- **Finite guard gap:** `EmbeddingClient::embed` (embed.rs:27) returns the provider's `Vec<f32>` unvalidated; `MemoryStore::insert` (store/memory.rs:48) and `PostgresStore::insert` (store/postgres.rs:47) store `chunk.embedding` with no finite check. A NaN/Inf component makes cosine distance NaN and corrupts top-k ordering for that org/corpus. The L2 cache already guards this exact class: `embedding_is_finite` (l2.rs:153) + `CacheError::InvalidEmbedding`, checked at both insert sites (l2.rs:291, 410). Retrieval has no equivalent.
- **BPE rebuild:** `chunking::chunk()` (chunking.rs:13) calls `tiktoken_rs::cl100k_base()` on **every** call, which loads + parses the cl100k merge ranks each time. The `tt-tokenize` crate already solved this with a process-wide cache (tokenize/src/lib.rs:43): `static BPE: OnceLock<Option<CoreBPE>>; BPE.get_or_init(|| cl100k_base().ok()).as_ref()`.
- `RetrievalError` (error.rs) variants: `Embedding`, `Store`, `Tag`, `Malformed`. `RetrievalStore::insert(&self, chunk: Chunk) -> Result<(), RetrievalError>` (store/mod.rs:9). tiktoken-rs = "0.5". `crates/retrieval/src/lib.rs` is a module-decl + re-export root.

## Decision (user-approved)
Two independent fixes:
1. Reject non-finite embeddings at **both** invariant points — the `embed()` chokepoint (covers insert-bound AND query embeddings) and each store `insert` (authoritative store invariant, any caller). Mirrors L2 (guards insert + filters lookup).
2. Cache the cl100k BPE process-wide, mirroring `tt-tokenize`.

## Architecture

### Fix 1 — finite-embedding guard
**`crates/retrieval/src/error.rs`** — add a variant:
```rust
    #[error("non-finite embedding (NaN/Inf)")]
    InvalidEmbedding,
```

**`crates/retrieval/src/lib.rs`** — add a shared helper after the `pub use` block (mirrors l2.rs):
```rust
/// True if every component is finite (no NaN/Inf). A non-finite embedding makes
/// cosine distance NaN and corrupts top-k ranking, so it is rejected at the
/// embed chokepoint and at store insert (mirrors the L2 cache guard).
pub(crate) fn embedding_is_finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}
```

**`crates/retrieval/src/embed.rs`** — in `embed`, validate the parsed embedding before returning. Replace the final `parsed.data … .ok_or_else(…)` expression with a binding + finite check:
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
(The function returns `Result<Vec<f32>, RetrievalError>`; this covers query embeddings too, since all embeddings flow through `embed()`.)

**`crates/retrieval/src/store/memory.rs`** — at the top of `insert`, before pushing:
```rust
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError> {
        if !crate::embedding_is_finite(&chunk.embedding) {
            return Err(RetrievalError::InvalidEmbedding);
        }
        self.chunks.lock().unwrap().push(chunk);
        Ok(())
    }
```

**`crates/retrieval/src/store/postgres.rs`** — at the top of `insert`, before building the `pgvector::Vector`:
```rust
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError> {
        if !crate::embedding_is_finite(&chunk.embedding) {
            return Err(RetrievalError::InvalidEmbedding);
        }
        let embedding = pgvector::Vector::from(chunk.embedding);
        // …existing INSERT unchanged…
```

### Fix 2 — cache the BPE
**`crates/retrieval/src/chunking.rs`** — add a cached accessor and use it:
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
In `chunk`, replace the per-call `let bpe = match tiktoken_rs::cl100k_base() { Ok(b) => b, Err(_) => return <single chunk> };` with:
```rust
    let Some(bpe) = cl100k() else {
        return vec![Chunk {
            text: text.into(),
            start_token: 0,
            end_token: 0,
        }];
    };
```
`bpe` is now `&CoreBPE`; `bpe.encode_with_special_tokens(text)` and `bpe.decode(slice.to_vec())` are `&self` methods — the rest of `chunk` is unchanged.

## Error handling
- Non-finite embedding → `RetrievalError::InvalidEmbedding` at the first guard reached (embed or insert). No panics; the existing fail-closed retrieval middleware surfaces it as a normal error.
- BPE load failure → unchanged single-whole-text-chunk fallback (now cached as `None`, so it's not retried every call — acceptable; a failed embedded-data load is permanent for the process).

## Testing
- **`embed.rs`** (httpmock, mirrors `embed_round_trip`): mock `{"data":[{"embedding":[1e400]}]}` — `1e400` overflows to `f32::INFINITY` on parse — assert `embed()` returns `RetrievalError::InvalidEmbedding`. (JSON has no NaN/Inf literal; an over-magnitude number is the realistic non-finite source.)
- **`store/memory.rs`** (unit): `insert` a `Chunk` with `embedding: vec![f32::NAN]` → `Err(RetrievalError::InvalidEmbedding)`; a finite embedding still returns `Ok(())`. This exercises the shared `embedding_is_finite` that `PostgresStore::insert` reuses verbatim (postgres `insert` has no unit DB harness; its identical early-return guard is covered by review).
- **`chunking.rs`**: the existing `short_text_one_chunk` + `long_text_multiple_chunks_with_overlap` tests must stay green (behavior-preserving). The BPE caching is a perf change verified by the `OnceLock` pattern + green behavior tests — a speedup is not cleanly unit-testable (stated, not faked).

Gates (public repo, scoped per ADR-012): `cargo test -p tt-retrieval`; **`cargo fmt --check -p tt-retrieval`** (public CI gates fmt — the recurring miss); `cargo clippy -p tt-retrieval --all-targets -- -D warnings` clean. The postgres `insert` is behind the `postgres` feature (Cargo.toml `postgres = ["dep:sqlx", "dep:pgvector", …]`), so ALSO run `cargo clippy -p tt-retrieval --features postgres --all-targets -- -D warnings` + `cargo test -p tt-retrieval --features postgres` so the postgres guard compiles + the `crate::embedding_is_finite` reference resolves under that cfg.

## Out of scope
- A separate finite-filter inside `search`/`top_k` (L2-style lookup filter) — the `embed()` guard rejects a non-finite query before search; noting it.
- Unbounded-`k` clamp + embedding-model partitioning (separate, already-handled findings).
- Any schema/migration change (insert-time validation only).
- Switching to `tiktoken_rs::cl100k_base_singleton()` if it exists in 0.5 — the explicit `OnceLock` mirrors the in-repo `tt-tokenize` pattern and preserves the load-failure fallback; not worth diverging.
