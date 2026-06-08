# Batch 3a — Input-validation & robustness Implementation Plan

> **For agentic workers:** Steps use checkbox (`- [ ]`) syntax. TDD throughout.

**Goal:** Close 5 low-severity validation/robustness findings in `crates/{retrieval,mcp,inspect-rules-tier1}`.

**Architecture:** Five independent, additive fixes; no public-signature changes; no workspace ripple.

**Tech Stack:** Rust, regex, tempfile, tokio.

---

### Task 1: `min_similarity` NaN/range guard (`crates/retrieval/src/tags.rs`)
- [ ] Test: parse `<retrievable corpus="d" k="3" min_similarity="nan">` → tag.min_similarity is `None`; `min_similarity="1.5"` → `None`; `min_similarity="-0.2"` → `None`; `min_similarity="0.7"` → `Some(0.7)`.
- [ ] Run test → fails.
- [ ] Add `.filter(|v| v.is_finite() && (0.0..=1.0).contains(v))` after the `.parse::<f32>().ok()` at tags.rs:58.
- [ ] Run test → passes.

### Task 2: Secret-detection OpenAI regex (`crates/inspect-rules-tier1/src/rules/config_agents_md_contains_secrets.rs`)
- [ ] Test: a CLAUDE.md line with `sk-proj-` + 25 alnum/`_-` → fires; a line with a 48-char legacy `sk-` key → fires; a line with `sk-` + exactly 20 junk alnum (e.g. `sk-abcdefghij1234567890`) → does NOT fire as an OpenAI key.
- [ ] Run → fails.
- [ ] Replace the `("OpenAI API key", r"sk-[A-Za-z0-9]{20,}")` entry with the scoped + legacy pair from the design (3a.2).
- [ ] Run → passes.

### Task 3: `inspect_diff` extension sanitizer (`crates/mcp/src/tools/inspect_diff.rs`)
- [ ] Test (if an existing test harness for the tool exists, extend it; else assert the sanitizer helper): a `file_path` of `"x.rs"` → suffix `.rs`; `"x.../weird ext!!"` → sanitized to alnum, capped 16; no extension → empty suffix (temp file still created, scan runs).
- [ ] Run → fails (or add the sanitizer).
- [ ] Apply the sanitizer from design 3a.3 (filter `is_ascii_alphanumeric`, `take(16)`, empty→no suffix).
- [ ] Run → passes.

### Task 4: `classify_task` Reasoning-before-Code (`crates/mcp/src/tools/find_route_for.rs`)
- [ ] Test: `classify_task("analyze this code refactor")` → `TaskClass::Reasoning`; `classify_task("refactor this function")` → `TaskClass::Code`; `classify_task("compare these two diffs")` → `Reasoning`.
- [ ] Run → fails (currently Code).
- [ ] Swap the `Code` and `Reasoning` `else if` blocks so `Reasoning` is checked first.
- [ ] Run → passes.

### Task 5: `CleanupStream` Drop sync-removal (`crates/mcp/src/transport/sse.rs`)
- [ ] Test: build a `SessionMap`, insert a session id, construct a `CleanupStream`, drop it; assert the session is removed synchronously (lock uncontended) without needing a spawned task. (If `CleanupStream` is private/hard to construct in a unit test, add a `#[cfg(test)]` helper or test the try_lock removal path directly on the map.)
- [ ] Run → fails / new.
- [ ] Replace the Drop body with the try_lock-then-guarded-spawn form from design 3a.5.
- [ ] Run → passes.

### Final
- [ ] `cargo test -p tt-retrieval -p tt-mcp -p tt-inspect-rules-tier1`
- [ ] `cargo fmt --check` on changed files; `cargo clippy -p tt-retrieval -p tt-mcp -p tt-inspect-rules-tier1 --all-targets -- -D warnings`
- [ ] Commit, push, PR.
