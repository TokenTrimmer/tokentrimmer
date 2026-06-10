# Design: V0 — Shared CLI Foundation

_Date: 2026-06-03 · Status: approved design, pre-implementation · Repo: `public` (the `tt` CLI)_

## Problem

Every `tt` command that needs the hosted API resolves its credentials and base URL ad hoc.
The API key is read in two duplicated spots — `crates/cli/src/main.rs:289` and `:430`, both
`tt_api_key.or_else(|| std::env::var("TT_API_KEY").ok())` — with no persistent store, so users
must pass `--tt-api-key` or export `TT_API_KEY` on every invocation. The gateway base URL is
hard-coded to `https://tokentrimmer.fly.dev` in two places (`main.rs:98` Mcp default and
`proxy/config.rs:58`), even though every customer-facing surface (README, GETTING_STARTED SDK
section, the TS SDK's `DEFAULT_BASE_URL`, `SECURITY.md`, `docs/04-gateway-api-reference.md`) uses
the canonical custom domain `https://api.tokentrimmer.com/v1`. `tt proxy` cannot override the base
URL at all. And `~/.tokentrimmer/` (already used for proxy session logs) is not in the repo
`.gitignore`.

V0 builds the **one credential/config resolution seam** that every later CLI feature plugs into
(V2 browser login, V5 `tt chat`, V7 AI client), makes the base URL configurable with the correct
default, ships a local credential store usable immediately, and closes the gitignore gap.

## Current state (verified)

- **Key resolution duplicated:** `main.rs:289` (Mcp) and `main.rs:430` (Proxy/Gateway), each
  `tt_api_key.or_else(|| env::var("TT_API_KEY"))`. `validate_api_key` (`crates/mcp/src/auth.rs`,
  already called by the CLI at `main.rs:290`) only checks the `tt_live_`/`tt_test_` prefix.
- **Base URL hard-coded:** `main.rs:98` `#[arg(long, default_value = "https://tokentrimmer.fly.dev")]`
  on `Mcp` only; `proxy/config.rs:58` `gateway_base_url: "https://tokentrimmer.fly.dev"`. `tt proxy`
  exposes **no** base-URL flag. Base URL is consumed in `proxy/preview.rs`, `proxy/routes/models.rs`,
  `proxy/routes/anthropic.rs`, `proxy/routes/openai.rs`.
- **No persistence:** nothing reads/writes a credentials file. `~/.tokentrimmer/` is used only for
  session logs (`proxy/config.rs:51`, via `dirs::home_dir()`).
- **`.gitignore`** ignores `.env`/`.env.*` only — no `.tokentrimmer/`. `tt init` appends to a target
  repo's `.gitignore` via `crates/cli/templates/init/.gitignore.append`.
- **Deps present:** `reqwest`, `serde`, `serde_json`, `toml`, `dirs` 5.0. No new crates needed
  (`keyring`/`open` are deferred to V2). `SecretString` exists at `tt_shared::context::SecretString`.
- **Commands** (`main.rs` enum): Gateway, Inspect, Plan, Audit, Mcp, Init, Retrieval, Proxy, DocAdd,
  Search, Verify. No Login/Logout/Whoami.

## Goals / non-goals

**Goals:** one resolver seam (`Context::resolve`) with precedence flag > env > file for both key and
base URL; configurable base URL defaulting to `https://api.tokentrimmer.com`; a local credential
store (`~/.tokentrimmer/credentials.toml`, 0600) writable now via `tt login --token`; `tt whoami` and
`tt logout`; `.gitignore` hardening in this repo and the `tt init` template.

**Non-goals (later sub-projects):** browser `tt login` (V2); OS keychain / `keyring` (V2);
server-side key revoke on logout (V2+); networked `whoami` identity (org/email — needs a gateway
identity endpoint, V2); a general `tt config` subcommand; multi-profile / per-environment credential
sets.

## Architecture

A focused context module plus three small command handlers:

```
crates/cli/src/context/
  mod.rs    — Context::resolve(overrides) -> ResolvedContext; KeySource/BaseSource; DEFAULT_BASE_URL
  store.rs  — on-disk store: credentials.toml (0600) + config.toml; load/save/delete; dir 0700
crates/cli/src/auth/
  mod.rs    — login_with_token(), whoami(), logout()
```

`ResolvedContext` is the single value every gateway-touching command consumes:

```rust
pub const DEFAULT_BASE_URL: &str = "https://api.tokentrimmer.com";

pub enum KeySource { Flag, Env, File, None }
pub enum BaseSource { Flag, Env, File, Default }

pub struct ResolvedContext {
    pub api_key: Option<SecretString>,   // tt_shared::context::SecretString — no Debug leak
    pub key_source: KeySource,
    pub base_url: String,
    pub base_source: BaseSource,
}
```

### Resolution precedence (the core seam)

- **API key:** `--tt-api-key` flag → `TT_API_KEY` env → `credentials.toml` → none
- **Base URL:** `--tt-api-base` flag → `TT_API_BASE` env → `config.toml` `base_url` → `DEFAULT_BASE_URL`

`resolve()` is a **pure function** over explicit inputs — `(flag_key, flag_base, env_snapshot,
config_dir)` — with a thin real-world wrapper that reads the actual env and `~/.tokentrimmer/`.
This keeps tests off real `$HOME`/env (the `crates/config` PORT-env test was recently flaky precisely
because it touched global env; V0 avoids that by construction).

### On-disk files (`~/.tokentrimmer/`, directory mode 0700)

```toml
# credentials.toml   (mode 0600 on unix; best-effort / skipped on Windows via cfg(unix))
api_key = "tt_live_…"

# config.toml        (mode 0644)
base_url = "https://api.tokentrimmer.com"
```

Serde structs: `CredentialsFile { api_key: Option<String> }`, `ConfigFile { base_url: Option<String> }`.
The directory is created (0700) on first write.

## Components / commands

- **`tt login --token <KEY>`** — validates the key with the existing `validate_api_key`
  (`tt_mcp::auth`, the `tt_live_`/`tt_test_` prefix check; the CLI already depends on it), writes
  `credentials.toml` at 0600 (creating `~/.tokentrimmer/` at
  0700 if needed). `--token -` reads the key from **stdin** (keeps it out of shell history/argv).
  Optional `--base-url <URL>` persists to `config.toml`. Prints a masked confirmation (prefix + base).
  Bare `tt login` (no `--token`) errors: *"browser login arrives in V2 — use `tt login --token <KEY>`
  for now (get a key at dashboard.tokentrimmer.com)."* — reserving the name for V2.
- **`tt whoami`** — local only, no network in V0. Resolves via env + file and prints: key presence +
  **masked** prefix (e.g. `tt_live_a1b2…`, never the full key) + source; effective base URL + source;
  config dir. Exit `0` if a key is configured, `1` if not (scriptable, like `gh auth status`).
- **`tt logout`** — deletes `credentials.toml` only (leaves `config.toml` and session logs). States
  clearly it is **local only — does not revoke server-side** (revoke in the dashboard if compromised;
  server-side revoke is V2+). No-ops cleanly (exit 0) when no file is present.

Command surface: top-level `tt login` / `tt logout` / `tt whoami` (matches the flat command list and
is friendlier than a `gh`-style `tt auth …` namespace).

### Wiring existing commands (the dedup)

- Replace the duplicated resolution at `main.rs:289` and `:430` with `Context::resolve`.
- `Mcp`: change `tt_api_base: String` (hard-coded `default_value`) → `Option<String>`; the default is
  now owned by `Context` (`api.tokentrimmer.com`).
- `Proxy`: **add** a `--tt-api-base` flag (none today) and source `gateway_base_url` from `Context`
  instead of the hard-coded `proxy/config.rs:58`.
- `Gateway`: still requires a key, but a logged-in user need not export `TT_API_KEY`; the "missing
  key" bail message points at `tt login`.
- Docs: update `GETTING_STARTED.md:279` and the `proxy/config.rs` default so the documented/real
  default is `api.tokentrimmer.com`, not `fly.dev`.

### Hardening (security quick win)

- Repo `.gitignore`: add `.tokentrimmer/`.
- `tt init` template `crates/cli/templates/init/.gitignore.append`: add `.tokentrimmer/`.
  *(Belt-and-suspenders: the real store lives in `$HOME`, not the repo, so this guards against a
  misconfigured `session_log` path or a stray copy — cheap insurance.)*

## Error handling

- **Missing key where required** (gateway mode) → actionable error naming `tt login --token` and
  `TT_API_KEY`.
- **Invalid key on login** → rejected via `validate_api_key` with a clear message.
- **Corrupt `credentials.toml`** → fail **loudly**: a key the user believes is set must never be
  silently treated as absent. The error names the file + remediation (`tt login` again, or delete the
  file). `whoami` surfaces the parse error rather than crashing. (`config.toml` parse errors are also
  surfaced; base URL falls back to default only when the file is *absent*, not when it is corrupt.)
- **Permission / write failures** → error with path + cause. `0600`/`0700` enforced under `cfg(unix)`;
  on Windows the perms step is skipped (tt targets unix primarily).

## Testing

- **`store.rs`:** tempdir round-trip for both files; assert `0600` bits on `credentials.toml` (unix);
  `delete` removes the file; load-missing → `None`; load-corrupt → `Err`.
- **`mod.rs`:** precedence matrix (flag > env > file > default) for **both** key and base URL;
  `key_source` / `base_source` correctness; default base == `api.tokentrimmer.com` when nothing is
  set. All via the pure-function inputs — no real env / `$HOME`.
- **`auth`:** masking never emits the full key (assert the plaintext substring is absent from output);
  `login --token -` reads from stdin; `logout` no-ops cleanly when the file is absent.

## Success criteria

- A user runs `tt login --token tt_live_…` once and subsequent `tt gateway`/`tt proxy`/`tt mcp`
  commands work with no `--tt-api-key` and no `TT_API_KEY` export.
- `tt whoami` shows the masked key, its source, and the effective base URL; `tt logout` removes the
  stored key locally.
- All gateway-touching commands resolve the base URL via `Context` with the precedence above and the
  `api.tokentrimmer.com` default; `tt proxy` can now override it.
- The full key never appears in any command output; `credentials.toml` is `0600` on unix.
- `.tokentrimmer/` is gitignored in this repo and in `tt init`-scaffolded repos.

## Out of scope (restated)

Browser login, keychain, server-side revoke, networked identity, `tt config`, and multi-profile
credentials all land in later sub-projects (primarily V2).
