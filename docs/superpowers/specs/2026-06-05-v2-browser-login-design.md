# V2 — Browser-Assisted Login Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V2 (browser login). Builds on the V0 credential store/resolver.
**Depends on:** V0 (`tt login`/`store`/`context`) — merged.

## Goal

`tt login` (no `--token`) opens the TokenTrimmer dashboard's API-keys page in the browser, then reads the pasted key and stores it — a real UX win over "find the URL yourself, then `--token`". Fully deliverable in this (public) repo: no backend change. (A full OAuth/device flow needs cloud-side auth endpoints that don't exist; the public gateway only verifies `tt_live_*` keys, which the cloud dashboard mints — so browser-assisted paste is the pragmatic V2. `--token`/`--token -` stay for non-interactive use.)

## Architecture

All in `crates/cli/src/account/mod.rs` (the store/validate seam already exists from V0). The current `login_with_token` (which bails "browser login arrives in V2") becomes `login(token, base_url, no_browser)`.

### `crates/cli/src/account/mod.rs`
- `const DASHBOARD_KEYS_URL: &str = "https://dashboard.tokentrimmer.com/keys";` — the dashboard page that mints keys (the code's existing "get a key at dashboard.tokentrimmer.com" message). One-line constant if the route differs.
- **`browser_command_for(os: &str, url: &str) -> Option<(&'static str, Vec<String>)>`** (pure, tested): `macos → ("open", [url])`, `linux → ("xdg-open", [url])`, `windows → ("cmd", ["/C", "start", "", url])` (the empty title arg keeps `start` happy with a URL), else `None`.
- **`fn open_browser(url) -> bool`**: `browser_command_for(std::env::consts::OS, url)` → spawn (`Command::status`/`spawn`); `true` iff it launched. Integration.
- **`fn store_key(raw: &str, base_url: Option<String>) -> Result<()>`** (extracted from the current body): `tt_mcp::auth::validate_api_key` → `store::save_credentials` → optional `store::save_config` → the `ui::success("Logged in. Stored …")` line. Shared by both login paths.
- **`pub fn login(token: Option<String>, base_url: Option<String>, no_browser: bool) -> Result<()>`**:
  - `token` is `Some` → the existing path: `decide_token` (reading stdin when `-`) → `store_key`.
  - `token` is `None` → `browser_login(base_url, no_browser)`.
- **`fn browser_login(base_url, no_browser) -> Result<()>`**:
  - Require an interactive terminal: if `!console::user_attended()` → bail with "browser login needs an interactive terminal — use `tt login --token <KEY>`".
  - Print a short intro (what's about to happen).
  - If `!no_browser`: `open_browser(DASHBOARD_KEYS_URL)`; if it returns `false`, fall through to printing the URL. Always print "If your browser didn't open, visit: {URL}" so headless still works.
  - Read the key **hidden** via `dialoguer::Password::new().with_prompt("Paste your API key").interact()` (it's a secret; dialoguer is already a dep).
  - `store_key(&key, base_url)`.

### `crates/cli/src/main.rs`
- `Login { token, base_url }` gains `--no-browser` (`no_browser: bool`); dispatch → `tt_cli::account::login(token, base_url, no_browser)`.

## Testing
- **`browser_command_for`**: `"macos"`→`("open",[url])`, `"linux"`→`("xdg-open",[url])`, `"windows"`→`("cmd",["/C","start","",url])`, `"plan9"`→`None`; the url is carried through verbatim.
- **`decide_token`** (existing) stays green; **`store_key`** round-trips via a tempdir-backed store (validate a `tt_live_…`-shaped test key → saved + masked), reusing the existing store tests' pattern.
- **Smoke** (piped, no TTY): `printf '' | tt login` → bails "needs an interactive terminal — use `tt login --token`"; `tt login --token tt_live_… ` still stores (token path unchanged); `tt login --help` shows `--no-browser`. The browser-open + hidden paste is a manual interactive check.
- `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo test -p tt-cli`.

## Out of Scope
- OAuth 2.0 / device-authorization / local-callback flows (need cloud-side `/v1/auth/*` endpoints the public gateway lacks) — a later cross-repo slice.
- Auto-refreshing/rotating keys, multi-account profiles.
- A `--keys-url` override (hardcoded constant; adjust if the dashboard route changes).
