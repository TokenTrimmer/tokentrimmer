# Retrieval `k`-clamp (DoS guard) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** Audit-remediation Wave 3 (public repo, `crates/retrieval`). Closes the HIGH audit finding that the per-message `<retrievable k="N">` count is parsed from untrusted prompt text and passed unbounded into the pgvector `LIMIT`, and that an unbounded number of `<retrievable>` tags per message fans out into unbounded embedding/search work.

## Goal

Bound the work a single untrusted message can force the retrieval path to do:
- a single tag's `k` is capped, so the pgvector query can never be asked to return billions of rows;
- the number of `<retrievable>` tags honored per message is capped, so the per-message fan-out (one `top_k`/search per tag) is bounded.

This is live-exploitable today: `<retrievable>` tags are parsed in the production substitute path (`crates/core/src/middleware/retrieval.rs` → `substitute_in_messages`). (The *indexing* path that inserts chunks is not yet wired, so this slice does not touch it.)

## Background (verified)

- `crates/retrieval/src/tags.rs::parse` (`:35-39`): `k` is `k_re.captures(...).parse::<u32>().ok()).unwrap_or(5)` — **unbounded** (any `\d+` up to `u32::MAX`). Returns `Vec<RetrievableTag>` with no length cap.
- `crates/retrieval/src/substitute.rs::substitute_in_messages` (`:79`, `:105-109`): calls `tags::parse(&text)?`, then loops every tag calling `top_k(store, org_id, &t.corpus, &query_emb, t.k as usize, floor)`. One search per tag → fan-out scales with tag count.
- `crates/retrieval/src/search.rs::top_k` (`:9-22`): forwards `k` straight to `store.search(...)`.
- `crates/retrieval/src/store/postgres.rs::search`: binds `k` as the SQL `LIMIT` (`i64::try_from(k).unwrap_or(i64::MAX)`). No upper bound of its own.
- `RetrievableTag { corpus, k: u32, min_similarity, span }` — `k` is `u32`.

## Architecture (`crates/retrieval`)

Single chokepoint: `tags::parse` is the only producer of `RetrievableTag`s, and `top_k` is the only caller of `store.search`. Clamp at both — parse-time for the real fix, `top_k` for defense-in-depth.

### 1. Constants (`tags.rs`)
```rust
/// Maximum chunks a single `<retrievable>` tag may request. Caps the pgvector
/// `LIMIT` so an untrusted `k="4000000000"` cannot force an unbounded scan.
pub const MAX_RETRIEVAL_K: u32 = 50;

/// Maximum number of `<retrievable>` tags honored per message. Bounds the
/// per-message fan-out (one embedding search per tag). Tags beyond this are
/// ignored.
pub const MAX_RETRIEVABLE_TAGS: usize = 16;
```

### 2. Parse-time clamp (`tags.rs::parse`)
- Clamp each tag's `k`: `.unwrap_or(5).min(MAX_RETRIEVAL_K)` (default stays 5; `k="0"` stays 0 — already a no-op downstream, not our concern here).
- After the capture loop, if `out.len() > MAX_RETRIEVABLE_TAGS`, `out.truncate(MAX_RETRIEVABLE_TAGS)`. Truncation keeps the first N in document order (matches the existing in-order contract).

### 3. Defense-in-depth (`search.rs::top_k`)
- First line: `let k = k.min(MAX_RETRIEVAL_K as usize);` before `store.search`, so any non-`parse` caller of `top_k` is also bounded and the Postgres `LIMIT` is always ≤ `MAX_RETRIEVAL_K`.
- Import `crate::tags::MAX_RETRIEVAL_K`.

### 4. Documentation
- Note both caps in the `tags.rs` module doc-comment (the documented behavior of the `<retrievable>` tag), so the limits are discoverable where the tag syntax is described.

## Testing (pure unit tests — no DB)

`tags.rs`:
- `k="4000000000"` → tag's `k == MAX_RETRIEVAL_K` (50).
- `k="10"` (valid, under cap) → `k == 10` (unchanged).
- absent `k` → `k == 5` (existing default preserved — existing test already covers; keep).
- a message with `MAX_RETRIEVABLE_TAGS + 1` tags → `parse` returns exactly `MAX_RETRIEVABLE_TAGS`, and the returned tags are the first N in order (assert first/last corpus).
- a message with exactly `MAX_RETRIEVABLE_TAGS` tags → all returned (boundary).

`search.rs`:
- `top_k(..., k = 10_000, ...)` against a `MemoryStore` with a few chunks → returns at most `MAX_RETRIEVAL_K` results (the in-`top_k` clamp holds even though `MemoryStore` itself wouldn't otherwise bound). Assert `r.len() <= MAX_RETRIEVAL_K as usize`.

Gates: `cargo test -p tt-retrieval`; `cargo clippy --all-targets -- -D warnings`; `cargo fmt --check`.

## Out of scope
- **Embedding-model partitioning** (the sibling HIGH finding) — next slice; needs a migration + `Chunk` schema change + trait-signature change, and is preventive (no production indexer exists yet).
- The `chunking::chunk()` BPE-cache rebuild perf finding (medium) — separate quick follow-up.
- Changing `RetrievableTag.k`'s type, the `min_similarity` handling, or the substitute/splice logic.
