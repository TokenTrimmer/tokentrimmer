# Credentials-file atomic 0600 write — Design

**Status:** approved (design)
**Date:** 2026-06-07
**Slice:** Audit-remediation Wave 4 (public repo, `crates/cli`). Closes the finding *"Credentials file is briefly world/group-readable: write-then-chmod TOCTOU"* (`pub-cli`).

## Background (verified against current code)
`crates/cli/src/context/store.rs::save_credentials` (lines 58–67):
```rust
pub fn save_credentials(dir: &Path, api_key: &str) -> anyhow::Result<()> {
    ensure_dir(dir)?;
    let p = dir.join("credentials.toml");
    let body = toml::to_string(&CredentialsFile { api_key: Some(api_key.to_string()) })?;
    std::fs::write(&p, body).with_context(|| format!("write {}", p.display()))?;   // ← creates at umask default
    set_mode(&p, 0o600).with_context(|| format!("chmod {}", p.display()))?;        // ← tightens only AFTER
    Ok(())
}
```
`std::fs::write` creates the file with `0o666 & !umask` (commonly 0644/0664), so between the write and the `set_mode`, the `tt_live_*` secret is on disk readable by other local users/processes — a real window on a shared host / CI runner.

Already-correct, leave unchanged:
- `ensure_dir` (lines 38–42) `create_dir_all` + `set_mode(dir, 0o700).ok()` on **every** call — the directory is re-asserted 0700 each save (best-effort; non-unix no-op).
- `set_mode` (lines 27–36) is unix-only; a no-op returning `Ok(())` on non-unix.
- `save_config` (lines 92–100) has the same write-then-(no-)chmod shape but `config.toml` holds only the **non-secret** `base_url` (documented lines 1–3) — out of scope.
- `tempfile` is already a `crates/cli` dependency (used by the chat `/editor` command).

## Decision (user-approved)
Approach 1: **temp-file (0600) + atomic rename**. Write to a `NamedTempFile` created in the *same directory*, set it 0600 (unix), then `persist()` (atomic rename) onto `credentials.toml`. The secret never exists at the final path with loose perms, and the rename atomically *replaces* a pre-existing loose file (the new inode carries the temp's 0600). This is the finding's recommended fix.

Rejected: `OpenOptions::new().mode(0o600).create(true)` — sets perms only on create, so a pre-existing loose `credentials.toml` keeps its perms and there's no atomic replace. Rejected: `umask`-first write — non-atomic, process-global side effect.

## Architecture
Rewrite `save_credentials` only:
```rust
/// Write the API key to `credentials.toml` (0600). Creates the dir (0700).
/// Atomic: the secret is written to a 0600 temp file in the same directory and
/// renamed into place, so it is never briefly readable at umask-default perms
/// (closes the write-then-chmod TOCTOU). On non-unix the temp file carries no
/// perms (set_mode is a no-op) — the key is stored unprotected there, as before.
pub fn save_credentials(dir: &Path, api_key: &str) -> anyhow::Result<()> {
    use std::io::Write as _;

    ensure_dir(dir)?;
    let p = dir.join("credentials.toml");
    let body = toml::to_string(&CredentialsFile {
        api_key: Some(api_key.to_string()),
    })?;

    // Create the temp file in the SAME dir so the final rename is atomic
    // (same filesystem). tempfile creates it 0600 on unix; set_mode is an
    // explicit, version-independent guarantee.
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
Notes:
- `NamedTempFile::new_in(dir)` (not the OS temp dir) keeps the temp on the same filesystem so `persist` is a same-dir atomic `rename(2)`.
- `tmp.persist(&p)` returns `Result<File, PersistError>`; `PersistError` has a `.error: std::io::Error` field — map to it, then add `anyhow` context. The discarded returned `File` handle is fine.
- `NamedTempFile` auto-removes on any early `?` return, so a failure never leaves a stray temp file containing the secret.
- `use std::io::Write` is needed for `write_all` (scoped import inside the fn).

`set_mode`, `ensure_dir`, `load_credentials`, `delete_credentials`, `save_config`, the structs — all unchanged.

## Error handling
Temp-create / chmod / write / persist failures each surface as an `anyhow::Error` with path context. On any failure the `NamedTempFile` drop removes the temp file (no secret left at a predictable temp path). The function's public signature (`-> anyhow::Result<()>`) is unchanged, so callers (`account::login`/`store_key`) are unaffected.

## Testing (`crates/cli/src/context/store.rs` `#[cfg(test)]`)
- **Keep** `credentials_round_trip_and_perms` (asserts final 0600), `delete_credentials_reports_presence`, `corrupt_credentials_is_an_error`, `config_round_trip` — all must stay green (behavior-preserving).
- **Add** (unix-gated) `save_credentials_tightens_preexisting_loose_file`: pre-create `credentials.toml` at 0644 (`std::fs::write` + `set_mode(..,0o644)`), call `save_credentials`, assert the file mode is `0o600` and `load_credentials` returns the new key. This proves the atomic replace tightens perms — the case the rejected `OpenOptions.mode` approach would miss. Gate the whole test with `#[cfg(unix)]` (mode assertions are unix-only).

Gates (public repo, scoped per ADR-012): `cargo test -p tt-cli` (incl. the new test); **`cargo fmt --check -p tt-cli`** (public CI gates fmt — the recurring miss; run it before pushing); `cargo clippy -p tt-cli --all-targets -- -D warnings` clean on touched code.

## Out of scope
- `save_config` / `config.toml` (non-secret `base_url`).
- Non-unix permission protection (the temp/rename still gives atomic replace; perms are a documented no-op).
- Tightening a pre-existing directory whose perms are looser than 0700 beyond the existing best-effort `set_mode(dir, 0o700).ok()`.
- Any change to how the key is read or to callers.
