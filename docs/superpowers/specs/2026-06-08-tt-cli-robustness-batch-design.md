# tt-cli low-severity robustness batch — Design

**Status:** approved (design)
**Date:** 2026-06-08
**Slice:** Audit-remediation (public repo, `crates/cli`). Closes three low-severity `pub-cli` findings together: `mask_key` byte-slice panic; `tt route` client has no HTTP timeout; route id interpolated into the URL path without encoding.

## Background (verified against current code)
1. **`mask_key` panic** (`crates/cli/src/context/mod.rs:102-105`): `format!("{}…", &key[..key.len().min(12)])` byte-indexes the key. A multibyte UTF-8 char straddling byte 12 panics (mid-codepoint slice), crashing `whoami`/`login`. Latent — `tt_live_*` keys are ASCII today.
2. **`tt route` no timeout** (`crates/cli/src/route/mod.rs:113`): `let http = reqwest::Client::new();` (bare) serves `List`/`Show`/`Rm`/`Add` — a hung gateway hangs the command forever. (`tt models` is already covered: `catalog::fetch_catalog` applies a per-request 5s timeout, catalog.rs:96; only `tt route` is bare.)
3. **Route id unencoded in URL** (`crates/cli/src/route/mod.rs:126` Show, `:134` Rm): `format!("{base}/v1/routes/{id}")` with a user-supplied `id` (`RouteCmd::Show(String)`/`Rm(String)`). An id containing `/ ? # %` or spaces breaks the URL or alters the path. (`List` GETs `/v1/routes`, `Add` POSTs `/v1/routes` — no id, unaffected.)

`crates/cli` already deps `reqwest.workspace`. The repo uses `[workspace.dependencies]` (root `Cargo.toml:39`). `percent-encoding` 2.3.2 is already in the lockfile (transitive via url/reqwest). `route/mod.rs` has a `#[cfg(test)] mod tests` (line 227); `context/mod.rs` has a `mask_key` test (line 188).

## Decision (user-approved)
Fix all three in one cohesive CLI-hardening slice.

## Architecture

### 1. `mask_key` — char-safe truncation (`crates/cli/src/context/mod.rs`)
```rust
/// Mask a key for display: keep the `tt_live_`/`tt_test_` prefix + a few chars.
pub fn mask_key(key: &str) -> String {
    let shown: String = key.chars().take(12).collect();
    format!("{shown}…")
}
```
(`chars().take(12)` truncates on a codepoint boundary — never panics. For ASCII keys the output is identical to before.)

### 2. `tt route` client timeout (`crates/cli/src/route/mod.rs`)
Replace `let http = reqwest::Client::new();` (line 113) with:
```rust
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
```
A 30s **total** timeout is fine here — route ops are quick, non-streaming admin calls (no streams to cap). Infallible fallback keeps `run` unchanged otherwise.

### 3. Route id percent-encoded (`crates/cli/src/route/mod.rs` + Cargo deps)
- Root `Cargo.toml` `[workspace.dependencies]`: add `percent-encoding = "2"`.
- `crates/cli/Cargo.toml` `[dependencies]`: add `percent-encoding.workspace = true`.
- In `route/mod.rs`, add a helper (near the other free fns / `send`):
```rust
/// Percent-encode a user-supplied path segment (route id) so `/ ? # %`, spaces,
/// etc. can't break or traverse the URL path. Over-encodes harmless chars (e.g.
/// a UUID's `-`); the gateway percent-decodes the path param.
fn enc_segment(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}
```
- Use it at the two `routes/{id}` sites:
  - `Show`: `send(http.get(format!("{base}/v1/routes/{}", enc_segment(id))).bearer_auth(&key)).await?`
  - `Rm`: `http.delete(format!("{base}/v1/routes/{}", enc_segment(id)))`
  (`id` is `&String`; `enc_segment(id)` borrows it. The `Show` arm also uses `id` later for display — leave that as-is.)

## Error handling
- `mask_key` is now total (no panic path).
- `tt route` client build falls back to the plain client on the rare builder failure.
- A request that hits the 30s timeout surfaces through the existing `send(...)` error path (anyhow), same as any transport error.
- `enc_segment` is infallible.

## Testing (`crates/cli`)
- **`context/mod.rs`** (unit, next to the existing `mask_key` test): `mask_key("tt_live_caféxyz_more")` (a multibyte `é` within the first 12 bytes) does NOT panic and returns a non-empty `…`-suffixed string; the existing ASCII test stays green.
- **`route/mod.rs`** (unit, in the existing `mod tests`):
  - `enc_segment("a/b?c#d e")` produces a string with no raw `/`, `?`, `#`, or space (assert it contains `%2F`/`%3F`/`%23`/`%20` and no raw delimiter).
  - `enc_segment("550e8400-e29b-41d4-a716-446655440000")` (a UUID) → contains no `/` and the value, once you imagine the server percent-decoding it, round-trips (assert the encoded form has no `/` and decoding it via `percent_encoding::percent_decode_str(&enc).decode_utf8().unwrap()` equals the original).
- The `tt route` timeout is client config (no clean unit test; reqwest exposes no timeout getter) — verified by compile + existing route tests staying green.

Gates (public repo, scoped per ADR-012): `cargo test -p tt-cli`; **`cargo fmt --check -p tt-cli`**; `cargo clippy -p tt-cli --all-targets -- -D warnings` clean; `cargo deny check` is NOT run locally but `percent-encoding` is already vendored (no new advisory). No public-signature change — no workspace ripple; scope to `tt-cli` (+ the root Cargo.toml dep line).

## Out of scope
- `tt models` (already has a per-request timeout via `fetch_catalog`).
- The `tt login` "no shape check beyond non-empty + validate_api_key" finding — `validate_api_key` already enforces the `tt_live_`/`tt_test_` prefix, so it's largely addressed; re-verified separately, not in this batch.
- Retry/backoff on the route client (out of scope, as for tt-client).
