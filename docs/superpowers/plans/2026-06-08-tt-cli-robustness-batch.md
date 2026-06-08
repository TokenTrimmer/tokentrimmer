# tt-cli low-severity robustness batch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix three latent `crates/cli` bugs — a `mask_key` panic on non-ASCII keys, a missing HTTP timeout on the `tt route` client, and an unencoded user-supplied route id in the URL path.

**Architecture:** Char-safe `mask_key` (no dep); a 30s total timeout on the `tt route` reqwest client; percent-encode the route-id path segment via a new `percent-encoding` workspace dep + an `enc_segment` helper used at both `routes/{id}` sites.

**Tech Stack:** Rust (`crates/cli` = `tt-cli`), reqwest, `percent-encoding` (already vendored, added as a direct dep).

Spec: `docs/superpowers/specs/2026-06-08-tt-cli-robustness-batch-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`.** No public-signature change → no workspace ripple; scope gates to `tt-cli`. `percent-encoding` 2.3.2 is already in the lockfile (transitive), so adding it as a direct dep introduces no new crate / advisory.

---

### Task 1: mask_key panic + route timeout + route-id encoding

**Files:**
- Modify: `crates/cli/src/context/mod.rs` (`mask_key` + test)
- Modify: `crates/cli/src/route/mod.rs` (client timeout, `enc_segment` + 2 sites, tests)
- Modify: `Cargo.toml` (root — add `percent-encoding` to `[workspace.dependencies]`)
- Modify: `crates/cli/Cargo.toml` (add `percent-encoding.workspace = true`)

- [ ] **Step 1: Write the failing `mask_key` test**

In `crates/cli/src/context/mod.rs`, in the `#[cfg(test)] mod tests` block (next to the existing `mask_key("tt_live_abcd1234efgh")` test ~line 188), add:
```rust
    #[test]
    fn mask_key_handles_non_ascii_without_panicking() {
        // `é` (2 bytes) straddles byte 12, so byte-slicing `&key[..12]` panics.
        let masked = mask_key("tt_live_caféxyz_more");
        assert!(masked.ends_with('…'));
        assert!(masked.starts_with("tt_live_caf"));
    }
```

- [ ] **Step 2: Run to confirm it panics (fails)**

Run: `cargo test -p tt-cli mask_key_handles_non_ascii 2>&1 | tail -15`
Expected: FAIL — the test panics with `byte index 12 is not a char boundary` inside `mask_key`.

- [ ] **Step 3: Fix `mask_key` to be char-safe**

In `crates/cli/src/context/mod.rs`, replace `mask_key`:
```rust
/// Mask a key for display: keep the `tt_live_`/`tt_test_` prefix + a few chars.
pub fn mask_key(key: &str) -> String {
    let shown: String = key.chars().take(12).collect();
    format!("{shown}…")
}
```

- [ ] **Step 4: Run to confirm it passes**

Run: `cargo test -p tt-cli mask_key 2>&1 | tail -10`
Expected: PASS — both the new non-ASCII test and the existing ASCII `mask_key` test green.

- [ ] **Step 5: Add the `tt route` client timeout**

In `crates/cli/src/route/mod.rs`, replace `let http = reqwest::Client::new();` (~line 113) with:
```rust
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
```

- [ ] **Step 6: Add the `percent-encoding` dependency**

In the root `Cargo.toml`, under `[workspace.dependencies]`, add a line (alongside the others):
```toml
percent-encoding = "2"
```
In `crates/cli/Cargo.toml`, under `[dependencies]`, add:
```toml
percent-encoding.workspace = true
```

- [ ] **Step 7: Write the failing `enc_segment` tests**

In `crates/cli/src/route/mod.rs`, in the existing `#[cfg(test)] mod tests` (~line 227), add:
```rust
    #[test]
    fn enc_segment_encodes_path_breakers() {
        let e = super::enc_segment("a/b?c#d e");
        assert!(!e.contains('/') && !e.contains('?') && !e.contains('#') && !e.contains(' '));
        assert!(e.contains("%2F") && e.contains("%3F") && e.contains("%23") && e.contains("%20"));
    }

    #[test]
    fn enc_segment_uuid_round_trips() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let e = super::enc_segment(id);
        assert!(!e.contains('/'));
        let decoded = percent_encoding::percent_decode_str(&e)
            .decode_utf8()
            .unwrap();
        assert_eq!(decoded, id);
    }
```

- [ ] **Step 8: Run to confirm they fail to compile**

Run: `cargo test -p tt-cli enc_segment 2>&1 | tail -15`
Expected: FAIL — `enc_segment` is undefined (and possibly `percent_encoding` unresolved until step 6's dep is wired — which it is).

- [ ] **Step 9: Add `enc_segment` + wire the two URL sites**

In `crates/cli/src/route/mod.rs`, add the helper (near `send` / the other free fns):
```rust
/// Percent-encode a user-supplied path segment (route id) so `/ ? # %`, spaces,
/// etc. can't break or traverse the URL path. Over-encodes harmless chars (e.g.
/// a UUID's `-`); the gateway percent-decodes the path param.
fn enc_segment(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}
```
Update the `Show` arm (~line 126). NOTE: `id` is an owned `String` (the `match cmd` moves it), so pass `&id` (deref-coerces to `&str`):
```rust
                send(http.get(format!("{base}/v1/routes/{}", enc_segment(&id))).bearer_auth(&key))
                    .await?;
```
Update the `Rm` arm (~line 134):
```rust
                http.delete(format!("{base}/v1/routes/{}", enc_segment(&id)))
                    .bearer_auth(&key),
```
Leave the `id` usage in the `Show` arm's display (`route["name"].as_str().unwrap_or(&id)`) and the `Rm` success message (`Removed route {id}.`) unchanged — those show the raw id, which is correct.

- [ ] **Step 10: Run the route tests**

Run: `cargo test -p tt-cli enc_segment 2>&1 | tail -10`
Expected: PASS — both `enc_segment` tests green.
Run: `cargo test -p tt-cli route 2>&1 | tail -10`
Expected: PASS — existing route tests still green.

- [ ] **Step 11: Full gates**

Run: `cargo test -p tt-cli 2>&1 | tail -10` → all pass.
Run: `cargo fmt --check -p tt-cli 2>&1 | tail -3` → no diff (if drift: `cargo fmt -p tt-cli`, re-check).
Run: `cargo clippy -p tt-cli --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean | head` → none.

- [ ] **Step 12: Commit (stage the four files)**

```bash
git add crates/cli/src/context/mod.rs crates/cli/src/route/mod.rs Cargo.toml crates/cli/Cargo.toml Cargo.lock
git commit -m "fix(cli): char-safe mask_key, tt route timeout, percent-encode route id

mask_key byte-sliced the key (panic on a non-ASCII char straddling byte 12) →
chars().take. The tt route client had no timeout (hung gateway hangs forever) →
30s total. A user-supplied route id was interpolated into the URL path unencoded
→ percent-encode the segment.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```
(`Cargo.lock` will gain the `percent-encoding` direct-dep edge — stage it.)

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-cli 2>&1 | tail -10
cargo fmt --check -p tt-cli
cargo clippy -p tt-cli --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean
```
All green / empty. **Stage only the listed files** (the working tree also carries a `rust_out` junk file — do NOT stage it).

## Notes for the implementer
- `chars().take(12)` truncates on a codepoint boundary — for ASCII keys the masked output is byte-identical to before; for non-ASCII it no longer panics.
- The `tt route` 30s timeout is a TOTAL timeout — fine here because route ops are quick non-streaming admin calls (no streams to cap, unlike tt-client's stream path).
- `NON_ALPHANUMERIC` over-encodes (e.g. a UUID's `-` → `%2D`); that's correct — axum/the gateway percent-decodes the path param. Do not hand-roll a narrower set.
- `enc_segment` is a private fn; the tests reach it via `super::enc_segment`.
- `tt models` is NOT touched (its `fetch_catalog` already applies a per-request 5s timeout).
