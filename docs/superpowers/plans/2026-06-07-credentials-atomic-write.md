# Credentials-file atomic 0600 write Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the write-then-chmod TOCTOU in `save_credentials` so the `tt_live_*` secret is never briefly world/group-readable, by writing it to a 0600 temp file and atomically renaming it into place.

**Architecture:** Rewrite `crates/cli/src/context/store.rs::save_credentials` to use `tempfile::NamedTempFile::new_in(dir)` (same filesystem → atomic rename), set the temp 0600 on unix, write the body, then `persist()` onto `credentials.toml`. Surrounding functions and the public signature are unchanged.

**Tech Stack:** Rust (`crates/cli`), `tempfile` (already a dependency), `toml`, `anyhow`.

Spec: `docs/superpowers/specs/2026-06-07-credentials-atomic-write-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`** — run it before committing (recurring miss). Single small file; do not restructure store.rs.
>
> **TDD note:** this fix closes a *race window*, which is not deterministically unit-testable. The test added below is an **end-state regression guard** (asserts the file ends at 0600, including when a pre-existing loose file is replaced). It passes on the OLD code too — `std::fs::write` preserves an existing file's mode and the old code chmods afterward — so it is NOT a failing-first test. The TOCTOU *closure* is a structural property of the atomic temp+rename, verified by reading the diff. Do not fabricate a failing-first test for the race.

---

### Task 1: Atomic 0600 credentials write

**Files:**
- Modify: `crates/cli/src/context/store.rs` (`save_credentials`; add one test to the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the end-state regression-guard test**

In `crates/cli/src/context/store.rs`, inside `#[cfg(test)] mod tests`, add (after `credentials_round_trip_and_perms`):
```rust
    #[cfg(unix)]
    #[test]
    fn save_credentials_tightens_a_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("credentials.toml");
        // Simulate a credentials file left world/group-readable by an older,
        // pre-fix write (or a hostile pre-creation).
        std::fs::write(&p, "api_key = \"stale\"\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        save_credentials(dir.path(), "tt_live_new").unwrap();

        // The new key is stored and the file is now 0600 (the atomic replace
        // carries the temp file's restrictive perms).
        assert_eq!(
            load_credentials(dir.path()).unwrap(),
            Some("tt_live_new".to_string())
        );
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials.toml must be 0600 after save");
    }
```

- [ ] **Step 2: Run the test against the CURRENT code (baseline)**

Run: `cargo test -p tt-cli save_credentials_tightens 2>&1 | tail -15`
Expected: PASS. (This confirms the end-state guard is correct. It passes on the current write-then-chmod code because `std::fs::write` keeps the existing file's bytes-handle but the subsequent `set_mode` tightens to 0600 — the guard locks in that 0600 end-state so the rewrite can't regress it. The race window it documents is not visible to this test.)

- [ ] **Step 3: Rewrite `save_credentials` to the atomic form**

In `crates/cli/src/context/store.rs`, replace the entire `save_credentials` function (currently lines ~57–67) with:
```rust
/// Write the API key to `credentials.toml` (0600). Creates the dir (0700).
///
/// Atomic: the secret is written to a 0600 temp file in the same directory and
/// renamed into place, so it is never briefly readable at umask-default perms
/// (closes the old write-then-chmod TOCTOU). On non-unix the temp file carries
/// no perms (`set_mode` is a no-op) — the key is stored unprotected there, as
/// before.
pub fn save_credentials(dir: &Path, api_key: &str) -> anyhow::Result<()> {
    use std::io::Write as _;

    ensure_dir(dir)?;
    let p = dir.join("credentials.toml");
    let body = toml::to_string(&CredentialsFile {
        api_key: Some(api_key.to_string()),
    })?;

    // Temp file in the SAME dir so `persist` is a same-filesystem atomic rename.
    // tempfile creates it 0600 on unix; the explicit set_mode is a
    // version-independent guarantee.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("create temp file in {}", dir.display()))?;
    set_mode(tmp.path(), 0o600).with_context(|| format!("chmod {}", tmp.path().display()))?;
    tmp.write_all(body.as_bytes())
        .with_context(|| format!("write {}", tmp.path().display()))?;
    tmp.persist(&p)
        .map_err(|e| e.error)
        .with_context(|| format!("persist {}", p.display()))?;
    Ok(())
}
```
Leave `ensure_dir`, `set_mode`, `load_credentials`, `delete_credentials`, `save_config`, `load_config`, and the structs unchanged. Confirm the existing imports at the top of the file already bring in `anyhow::Context as _` (they do — used by the `.with_context` calls) and `std::path::Path` (they do).

- [ ] **Step 4: Run the full store test module**

Run: `cargo test -p tt-cli context::store 2>&1 | tail -20`
Expected: PASS — `credentials_round_trip_and_perms`, `save_credentials_tightens_a_preexisting_loose_file`, `delete_credentials_reports_presence`, `corrupt_credentials_is_an_error`, `config_round_trip` all green.

- [ ] **Step 5: fmt + clippy gates (public CI parity)**

Run: `cargo fmt --check -p tt-cli 2>&1 | tail -5`
Expected: no output / clean (no diff). If it reports drift, run `cargo fmt -p tt-cli` and re-check.
Run: `cargo clippy -p tt-cli --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | head`
Expected: no warnings/errors (a benign `failed to auto-clean cache data` permission line is fine; ignore it).

- [ ] **Step 6: Commit (stage only store.rs)**

```bash
git add crates/cli/src/context/store.rs
git commit -m "fix(cli): write credentials.toml atomically at 0600 (close TOCTOU)

std::fs::write created the secret at umask-default perms before chmod 0600,
leaving a brief world/group-readable window. Write to a 0600 temp file in the
same dir and atomically rename into place instead.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-cli context::store 2>&1 | tail -10
cargo fmt --check -p tt-cli
cargo clippy -p tt-cli --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | head
```
All green / no output. **Stage only `crates/cli/src/context/store.rs`** (the working tree also carries an unrelated stale `docs/reviews/...audit-checklist.md` edit + a `rust_out` junk file — do NOT stage them).

## Notes for the implementer
- `NamedTempFile::new_in(dir)` MUST use `dir` (not the OS temp dir) so the final `persist` is a same-filesystem atomic `rename(2)`. A cross-filesystem persist would fall back to copy+delete and reintroduce a window.
- `tmp.persist(&p)` returns `Result<File, tempfile::PersistError>`; `PersistError` exposes `.error: std::io::Error` — `.map_err(|e| e.error)` converts it before `.with_context`. The returned `File` is dropped (fine).
- On any `?` early-return the `NamedTempFile` drop removes the temp, so a failure never leaves a stray file containing the secret.
- `tempfile` is already in `crates/cli/Cargo.toml` (chat `/editor`) — no new dependency.
- `config.toml` / `save_config` is intentionally untouched (non-secret `base_url`).
