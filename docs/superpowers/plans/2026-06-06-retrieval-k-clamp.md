# Retrieval `k`-clamp (DoS guard) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound the work an untrusted `<retrievable>`-tagged message can force on the retrieval path — cap a single tag's `k` and cap the number of tags honored per message.

**Architecture:** Two `pub const`s in `crates/retrieval/src/tags.rs`. `tags::parse` clamps each tag's `k` and truncates the returned vec to the tag cap (the single producer of `RetrievableTag`s). `search::top_k` re-clamps `k` before `store.search` as defense-in-depth (the single caller of `store.search`), so the pgvector `LIMIT` is always ≤ `MAX_RETRIEVAL_K`. Pure unit tests, no DB, no schema change.

**Tech Stack:** Rust, `regex`, `tokio`/`uuid` (existing test deps in the crate).

Spec: `docs/superpowers/specs/2026-06-06-retrieval-k-clamp-design.md`

---

### Task 1: Clamp `k` and tag-count in `tags::parse`

**Files:**
- Modify: `crates/retrieval/src/tags.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `#[cfg(test)] mod tests` block in `crates/retrieval/src/tags.rs` (after the existing `no_tags_is_empty` test, before the closing `}`):

```rust
    #[test]
    fn k_is_clamped_to_max() {
        let t = parse(r#"<retrievable corpus="x" k="4000000000">y</retrievable>"#).unwrap();
        assert_eq!(t[0].k, MAX_RETRIEVAL_K);
    }

    #[test]
    fn k_under_cap_is_unchanged() {
        let t = parse(r#"<retrievable corpus="x" k="10">y</retrievable>"#).unwrap();
        assert_eq!(t[0].k, 10);
    }

    #[test]
    fn tag_count_is_capped() {
        // Build MAX_RETRIEVABLE_TAGS + 1 tags, each with a distinct corpus so we
        // can assert which ones survived truncation.
        let mut body = String::new();
        for i in 0..(MAX_RETRIEVABLE_TAGS + 1) {
            body.push_str(&format!(r#"<retrievable corpus="c{i}">p</retrievable>"#));
        }
        let t = parse(&body).unwrap();
        assert_eq!(t.len(), MAX_RETRIEVABLE_TAGS);
        // First-N in document order are kept.
        assert_eq!(t[0].corpus, "c0");
        assert_eq!(t[MAX_RETRIEVABLE_TAGS - 1].corpus, format!("c{}", MAX_RETRIEVABLE_TAGS - 1));
    }

    #[test]
    fn tag_count_at_cap_all_kept() {
        let mut body = String::new();
        for i in 0..MAX_RETRIEVABLE_TAGS {
            body.push_str(&format!(r#"<retrievable corpus="c{i}">p</retrievable>"#));
        }
        let t = parse(&body).unwrap();
        assert_eq!(t.len(), MAX_RETRIEVABLE_TAGS);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tt-retrieval --lib tags:: 2>&1 | tail -20`
Expected: FAIL — `k_is_clamped_to_max` fails (k == 4000000000, not 50); `tag_count_is_capped` fails (len == 17, not 16); plus compile errors referencing `MAX_RETRIEVAL_K` / `MAX_RETRIEVABLE_TAGS` not found (the consts don't exist yet).

- [ ] **Step 3: Add the constants**

In `crates/retrieval/src/tags.rs`, after the `use` lines (after `use crate::types::RetrievableTag;`, before `pub fn parse`), add:

```rust
/// Maximum chunks a single `<retrievable>` tag may request. Caps the pgvector
/// `LIMIT` so an untrusted `k="4000000000"` cannot force an unbounded scan.
pub const MAX_RETRIEVAL_K: u32 = 50;

/// Maximum number of `<retrievable>` tags honored per message. Bounds the
/// per-message fan-out (one embedding search per tag). Tags beyond this are
/// ignored (the first `MAX_RETRIEVABLE_TAGS` in document order are kept).
pub const MAX_RETRIEVABLE_TAGS: usize = 16;
```

- [ ] **Step 4: Clamp `k` at parse time**

In `crates/retrieval/src/tags.rs::parse`, change the `k` extraction from:

```rust
        let k = k_re
            .captures(attrs)
            .and_then(|c| c.get(1))
            .and_then(|s| s.as_str().parse::<u32>().ok())
            .unwrap_or(5);
```

to:

```rust
        let k = k_re
            .captures(attrs)
            .and_then(|c| c.get(1))
            .and_then(|s| s.as_str().parse::<u32>().ok())
            .unwrap_or(5)
            .min(MAX_RETRIEVAL_K);
```

- [ ] **Step 5: Truncate the tag count**

In `crates/retrieval/src/tags.rs::parse`, change the final return from:

```rust
    }
    Ok(out)
}
```

to:

```rust
    }
    out.truncate(MAX_RETRIEVABLE_TAGS);
    Ok(out)
}
```

- [ ] **Step 6: Document the caps in the module doc**

In `crates/retrieval/src/tags.rs`, change the top module doc-comment from:

```rust
//! Parse `<retrievable corpus="X" k="N">...</retrievable>` tags from message
//! text. Returns each tag's corpus, k, and span in the text.
```

to:

```rust
//! Parse `<retrievable corpus="X" k="N">...</retrievable>` tags from message
//! text. Returns each tag's corpus, k, and span in the text.
//!
//! Two caps bound the work an untrusted message can request: a single tag's `k`
//! is clamped to [`MAX_RETRIEVAL_K`], and at most [`MAX_RETRIEVABLE_TAGS`] tags
//! are honored per message (the rest are ignored).
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p tt-retrieval --lib tags:: 2>&1 | tail -20`
Expected: PASS — all `tags::tests` pass (existing `single_tag`, `default_k_when_missing`, `per_tag_min_similarity_parsed`, `multiple_tags_in_order`, `no_tags_is_empty` plus the four new ones).

- [ ] **Step 8: Commit**

```bash
git add crates/retrieval/src/tags.rs
git commit -m "feat(retrieval): clamp per-tag k and tag count in parse

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Defense-in-depth clamp in `search::top_k`

**Files:**
- Modify: `crates/retrieval/src/search.rs`

- [ ] **Step 1: Write the failing test**

Add this test inside the existing `#[cfg(test)] mod tests` block in `crates/retrieval/src/search.rs` (after the existing `min_similarity_filter` test, before the closing `}`):

```rust
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
        let r = top_k(&s, o, "x", &[1.0, 0.0], 10_000, 0.0).await.unwrap();
        assert!(r.len() <= crate::tags::MAX_RETRIEVAL_K as usize);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-retrieval --lib search:: 2>&1 | tail -20`
Expected: FAIL — `top_k_clamps_oversized_k` returns `MAX_RETRIEVAL_K + 10` results (MemoryStore honors the raw k=10_000), so `r.len() <= 50` is false.

Note: if `MemoryStore::search` itself caps results at `k` and `k` is passed through unclamped, the assertion still fails because `MAX_RETRIEVAL_K + 10` (60) chunks exist and k=10_000 returns all 60 > 50. (Confirms the clamp must live in `top_k`.)

- [ ] **Step 3: Add the clamp**

In `crates/retrieval/src/search.rs::top_k`, change the body from:

```rust
    let raw = store.search(org_id, corpus, query_embedding, k).await?;
```

to:

```rust
    let k = k.min(crate::tags::MAX_RETRIEVAL_K as usize);
    let raw = store.search(org_id, corpus, query_embedding, k).await?;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p tt-retrieval --lib search:: 2>&1 | tail -20`
Expected: PASS — `min_similarity_filter` and `top_k_clamps_oversized_k` both pass.

- [ ] **Step 5: Run the full crate gates**

Run: `cargo test -p tt-retrieval 2>&1 | tail -15`
Expected: PASS — all tests in `tt-retrieval` green.

Run: `cargo clippy -p tt-retrieval --all-targets -- -D warnings 2>&1 | tail -15`
Expected: no warnings.

Run: `cargo fmt -p tt-retrieval -- --check 2>&1 | tail -5`
Expected: clean (no diff). If it reports a diff, run `cargo fmt -p tt-retrieval` and re-stage.

- [ ] **Step 6: Commit**

```bash
git add crates/retrieval/src/search.rs
git commit -m "feat(retrieval): defense-in-depth k clamp in top_k

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Notes for the implementer

- `cargo fmt --check` is a public-repo CI gate (it has bitten this campaign before via hand-written test line-wrap). Run `cargo fmt -p tt-retrieval` before the final commit if `--check` shows any diff, and re-stage only `crates/retrieval/` files.
- Do not whole-workspace `cargo fmt`. Stage only the two files you edited.
- No migration, no trait-signature change, no `Chunk` change — embedding-model partitioning is a separate follow-up slice.
