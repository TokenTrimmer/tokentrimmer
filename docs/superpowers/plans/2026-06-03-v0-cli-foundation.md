# V0 — Shared CLI Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the `tt` CLI one credential/config resolution seam (flag > env > `~/.tokentrimmer/` file), a configurable gateway base URL defaulting to `https://api.tokentrimmer.com`, a local credential store usable now via `tt login --token`, plus `tt whoami`/`tt logout` and `.gitignore` hardening.

**Architecture:** A new `tt_cli::context` module owns resolution (`ResolvedContext`) and the on-disk store (`context::store`, `~/.tokentrimmer/credentials.toml` 0600 + `config.toml`). A new `tt_cli::account` module holds the `login`/`logout`/`whoami` command bodies. The `Mcp` and `Proxy --mode gateway` dispatch paths in `main.rs` (the only key-needing client commands) are rewired to `ResolvedContext::load`. Pure functions take explicit inputs so tests never touch real `$HOME`/env.

**Tech Stack:** Rust (workspace crate `tt-cli`, lib name `tt_cli`), `clap` derive, `serde`+`toml`, `dirs` 5.0, `tt_shared::context::SecretString`, `tt_mcp::auth::validate_api_key`. All deps already present; **no new dependencies**.

**Repo / branch:** Work in `/Users/iansimon/Developer/TokenTrimmer/public` on branch `feat/v0-cli-foundation` (already checked out; the V0 spec is committed there). Spec: `docs/superpowers/specs/2026-06-03-v0-cli-foundation-design.md`.

**Test command note:** `cargo test --workspace` is hook-denied in this repo — always scope to `cargo test -p tt-cli`. Rust "red" = a compile error when a test references not-yet-defined items; that counts as the failing-test step.

**Verified current-state anchors:**
- `crates/cli/src/main.rs:289` (Mcp) and `:430` (Proxy) each do `tt_api_key.or_else(|| std::env::var("TT_API_KEY").ok())`.
- `Command::Mcp` has `#[arg(long, default_value = "https://tokentrimmer.fly.dev")] tt_api_base: String`. `Command::Proxy` has **no** base-URL arg. `Command::Gateway` (the server) has only `migrate_only` — it does NOT use the resolver.
- `crates/cli/src/proxy/config.rs:58` hard-codes `gateway_base_url: "https://tokentrimmer.fly.dev"`; `Config::build(port, bind, mode, tt_api_key: Option<String>, no_tui, no_preview, session_log_dir)` (no base-url param).
- `tt_mcp::auth::validate_api_key(Option<String>) -> Result<String, McpError>` (prefix check). MCP tools take `api_key: String` + `base_url: String`.
- `tt_shared::context::SecretString`: `new(impl Into<String>)`, `expose(&self) -> &str`, `Clone`, redacting `Debug`.
- `crates/cli/src/lib.rs` declares `pub mod {cost_diff, init, plan_suggest, proxy, retrieval};`.
- `crates/cli/templates/init/.gitignore.append` exists (tt init appends it to a target repo's `.gitignore`).

---

## File Structure

| File | Responsibility |
|------|----------------|
| `.gitignore` (modify) | Ignore `.tokentrimmer/` in this repo. |
| `crates/cli/templates/init/.gitignore.append` (modify) | Ignore `.tokentrimmer/` in `tt init`-scaffolded repos. |
| `crates/cli/src/lib.rs` (modify) | Declare `pub mod context;` and `pub mod account;`. |
| `crates/cli/src/context/mod.rs` (create) | `DEFAULT_BASE_URL`, `KeySource`/`BaseSource` (+`Display`), `ResolvedContext`, pure `resolve_key`/`resolve_base`, `mask_key`, `ResolvedContext::load`; `pub mod store;`. |
| `crates/cli/src/context/store.rs` (create) | On-disk `credentials.toml` (0600) + `config.toml`: `config_dir`, `load/save/delete_credentials`, `load/save_config`, unix perms. |
| `crates/cli/src/account/mod.rs` (create) | `decide_token` (pure), `login_with_token`, `whoami`, `logout`. |
| `crates/cli/src/main.rs` (modify) | Add `Login`/`Logout`/`Whoami` commands + dispatch; rewire `Mcp` + `Proxy` to `ResolvedContext`. |
| `crates/cli/src/proxy/config.rs` (modify) | Default `gateway_base_url` → `https://api.tokentrimmer.com`. |
| `GETTING_STARTED.md` (modify) | Update the documented default base URL. |

---

## Task 1: `.gitignore` hardening

**Files:**
- Modify: `.gitignore`
- Modify: `crates/cli/templates/init/.gitignore.append`

- [ ] **Step 1: Add `.tokentrimmer/` to the repo `.gitignore`**

In `.gitignore`, under the `# Local env + secrets` block (currently `.env` / `.env.*` / `!.env.example`), add the line so the block reads:

```gitignore
# Local env + secrets
.env
.env.*
!.env.example
# tt CLI credential/config store (~/.tokentrimmer is in $HOME, but guard against
# a stray repo-local copy, e.g. a session_log path pointed inside a repo).
.tokentrimmer/
```

- [ ] **Step 2: Add `.tokentrimmer/` to the `tt init` template**

In `crates/cli/templates/init/.gitignore.append`, append a final line:

```gitignore
.tokentrimmer/
```

- [ ] **Step 3: Verify**

Run: `grep -n '.tokentrimmer/' .gitignore crates/cli/templates/init/.gitignore.append`
Expected: one match in each file.

- [ ] **Step 4: Commit**

```bash
git add .gitignore crates/cli/templates/init/.gitignore.append
git commit -m "chore(cli): gitignore .tokentrimmer/ (repo + tt init template)"
```

---

## Task 2: Credential/config store (`context/store.rs`)

**Files:**
- Modify: `crates/cli/src/lib.rs`
- Create: `crates/cli/src/context/mod.rs`
- Create: `crates/cli/src/context/store.rs`

- [ ] **Step 1: Declare the module and write the failing test**

In `crates/cli/src/lib.rs`, add after the existing `pub mod` lines:

```rust
pub mod context;
```

Create `crates/cli/src/context/mod.rs` with just:

```rust
//! Credential + config resolution for the `tt` CLI.

pub mod store;
```

Create `crates/cli/src/context/store.rs` with **only the test module first** (the impl lands in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_and_perms() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_credentials(dir.path()).unwrap(), None);
        save_credentials(dir.path(), "tt_live_abc123").unwrap();
        assert_eq!(
            load_credentials(dir.path()).unwrap(),
            Some("tt_live_abc123".to_string())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("credentials.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn delete_credentials_reports_presence() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!delete_credentials(dir.path()).unwrap());
        save_credentials(dir.path(), "tt_test_x").unwrap();
        assert!(delete_credentials(dir.path()).unwrap());
        assert_eq!(load_credentials(dir.path()).unwrap(), None);
    }

    #[test]
    fn corrupt_credentials_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("credentials.toml"), "not = [valid").unwrap();
        assert!(load_credentials(dir.path()).is_err());
    }

    #[test]
    fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_config(dir.path()).unwrap(), None);
        save_config(dir.path(), "https://staging.example.com").unwrap();
        assert_eq!(
            load_config(dir.path()).unwrap(),
            Some("https://staging.example.com".to_string())
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails (does not compile)**

Run: `cargo test -p tt-cli context::store`
Expected: FAIL — compile errors (`cannot find function load_credentials` etc.).

- [ ] **Step 3: Write the store implementation**

Replace the contents of `crates/cli/src/context/store.rs` with the impl **above** the existing `#[cfg(test)] mod tests` block:

```rust
//! On-disk credential + config store at `~/.tokentrimmer/`.
//! `credentials.toml` holds the secret API key (0600 on unix); `config.toml`
//! holds non-secret settings (base_url). The directory is created 0700.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// `~/.tokentrimmer/` (falls back to `./.tokentrimmer` if HOME is unknown).
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".tokentrimmer"))
        .unwrap_or_else(|| PathBuf::from(".tokentrimmer"))
}

#[derive(Serialize, Deserialize, Default)]
struct CredentialsFile {
    api_key: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct ConfigFile {
    base_url: Option<String>,
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn ensure_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    set_mode(dir, 0o700).ok(); // best-effort; non-unix is a no-op
    Ok(())
}

/// Read the stored API key, or `None` if the file is absent / the key is blank.
/// Errors only on a present-but-unparseable file (never silently drop a key).
pub fn load_credentials(dir: &Path) -> anyhow::Result<Option<String>> {
    let p = dir.join("credentials.toml");
    if !p.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let f: CredentialsFile =
        toml::from_str(&raw).with_context(|| format!("parse {}", p.display()))?;
    Ok(f.api_key.filter(|s| !s.trim().is_empty()))
}

/// Write the API key to `credentials.toml` (0600). Creates the dir (0700).
pub fn save_credentials(dir: &Path, api_key: &str) -> anyhow::Result<()> {
    ensure_dir(dir)?;
    let p = dir.join("credentials.toml");
    let body = toml::to_string(&CredentialsFile {
        api_key: Some(api_key.to_string()),
    })?;
    std::fs::write(&p, body).with_context(|| format!("write {}", p.display()))?;
    set_mode(&p, 0o600).with_context(|| format!("chmod {}", p.display()))?;
    Ok(())
}

/// Remove `credentials.toml`. Returns `true` if a file was removed.
pub fn delete_credentials(dir: &Path) -> anyhow::Result<bool> {
    let p = dir.join("credentials.toml");
    if p.exists() {
        std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Read the persisted base URL, or `None` if absent / blank. Errors on corrupt.
pub fn load_config(dir: &Path) -> anyhow::Result<Option<String>> {
    let p = dir.join("config.toml");
    if !p.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let f: ConfigFile = toml::from_str(&raw).with_context(|| format!("parse {}", p.display()))?;
    Ok(f.base_url.filter(|s| !s.trim().is_empty()))
}

/// Persist the base URL to `config.toml`. Creates the dir (0700).
pub fn save_config(dir: &Path, base_url: &str) -> anyhow::Result<()> {
    ensure_dir(dir)?;
    let p = dir.join("config.toml");
    let body = toml::to_string(&ConfigFile {
        base_url: Some(base_url.to_string()),
    })?;
    std::fs::write(&p, body).with_context(|| format!("write {}", p.display()))?;
    Ok(())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p tt-cli context::store`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/lib.rs crates/cli/src/context/mod.rs crates/cli/src/context/store.rs
git commit -m "feat(cli): ~/.tokentrimmer credential + config store (0600/0700)"
```

---

## Task 3: Resolver (`context/mod.rs`)

**Files:**
- Modify: `crates/cli/src/context/mod.rs`

- [ ] **Step 1: Write the failing test**

Append this test module to `crates/cli/src/context/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_precedence_flag_env_file_none() {
        let (k, s) = resolve_key(Some("flagk".into()), Some("envk".into()), Some("filek".into()));
        assert_eq!(k.unwrap().expose(), "flagk");
        assert_eq!(s, KeySource::Flag);

        let (k, s) = resolve_key(None, Some("envk".into()), Some("filek".into()));
        assert_eq!(k.unwrap().expose(), "envk");
        assert_eq!(s, KeySource::Env);

        let (k, s) = resolve_key(None, None, Some("filek".into()));
        assert_eq!(k.unwrap().expose(), "filek");
        assert_eq!(s, KeySource::File);

        let (k, s) = resolve_key(None, None, None);
        assert!(k.is_none());
        assert_eq!(s, KeySource::None);
    }

    #[test]
    fn blanks_are_treated_as_absent() {
        let (k, s) = resolve_key(Some("   ".into()), Some("envk".into()), None);
        assert_eq!(k.unwrap().expose(), "envk");
        assert_eq!(s, KeySource::Env);
    }

    #[test]
    fn base_precedence_and_default() {
        let (b, s) = resolve_base(Some("https://flag".into()), Some("https://env".into()), None);
        assert_eq!(b, "https://flag");
        assert_eq!(s, BaseSource::Flag);

        let (b, s) = resolve_base(None, None, Some("https://file".into()));
        assert_eq!(b, "https://file");
        assert_eq!(s, BaseSource::File);

        let (b, s) = resolve_base(None, None, None);
        assert_eq!(b, DEFAULT_BASE_URL);
        assert_eq!(b, "https://api.tokentrimmer.com");
        assert_eq!(s, BaseSource::Default);
    }

    #[test]
    fn mask_hides_the_secret() {
        let masked = mask_key("tt_live_abcd1234efgh");
        assert_eq!(masked, "tt_live_abcd…");
        assert!(!masked.contains("1234efgh"));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails (does not compile)**

Run: `cargo test -p tt-cli context::tests`
Expected: FAIL — compile errors (`resolve_key`, `KeySource`, etc. undefined).

- [ ] **Step 3: Write the resolver implementation**

In `crates/cli/src/context/mod.rs`, replace the top (keep `pub mod store;`) so the file begins with:

```rust
//! Credential + config resolution for the `tt` CLI.
//!
//! Precedence (both key and base URL): flag > env > ~/.tokentrimmer file >
//! built-in default. The `resolve_*` functions are pure (explicit inputs) so
//! tests never read real env / $HOME; `ResolvedContext::load` is the thin
//! real-world wrapper.

pub mod store;

use tt_shared::context::SecretString;

/// Built-in default gateway base URL (the canonical custom domain; SDKs use it).
pub const DEFAULT_BASE_URL: &str = "https://api.tokentrimmer.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Flag,
    Env,
    File,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSource {
    Flag,
    Env,
    File,
    Default,
}

impl std::fmt::Display for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            KeySource::Flag => "--tt-api-key",
            KeySource::Env => "TT_API_KEY env",
            KeySource::File => "credentials.toml",
            KeySource::None => "none",
        })
    }
}

impl std::fmt::Display for BaseSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BaseSource::Flag => "--tt-api-base",
            BaseSource::Env => "TT_API_BASE env",
            BaseSource::File => "config.toml",
            BaseSource::Default => "default",
        })
    }
}

/// The resolved client context every gateway-touching command consumes.
pub struct ResolvedContext {
    pub api_key: Option<SecretString>,
    pub key_source: KeySource,
    pub base_url: String,
    pub base_source: BaseSource,
}

fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Pure: pick the API key by precedence flag > env > file.
pub fn resolve_key(
    flag: Option<String>,
    env: Option<String>,
    file: Option<String>,
) -> (Option<SecretString>, KeySource) {
    if let Some(k) = nonempty(flag) {
        return (Some(SecretString::new(k)), KeySource::Flag);
    }
    if let Some(k) = nonempty(env) {
        return (Some(SecretString::new(k)), KeySource::Env);
    }
    if let Some(k) = nonempty(file) {
        return (Some(SecretString::new(k)), KeySource::File);
    }
    (None, KeySource::None)
}

/// Pure: pick the base URL by precedence flag > env > file > built-in default.
pub fn resolve_base(
    flag: Option<String>,
    env: Option<String>,
    file: Option<String>,
) -> (String, BaseSource) {
    if let Some(b) = nonempty(flag) {
        return (b, BaseSource::Flag);
    }
    if let Some(b) = nonempty(env) {
        return (b, BaseSource::Env);
    }
    if let Some(b) = nonempty(file) {
        return (b, BaseSource::File);
    }
    (DEFAULT_BASE_URL.to_string(), BaseSource::Default)
}

/// Mask a key for display: keep the `tt_live_`/`tt_test_` prefix + a few chars.
pub fn mask_key(key: &str) -> String {
    let n = key.len().min(12);
    format!("{}…", &key[..n])
}

impl ResolvedContext {
    /// Resolve from CLI flags + real env (`TT_API_KEY`/`TT_API_BASE`) + the
    /// `~/.tokentrimmer/` files. Errors only if a stored file is corrupt.
    pub fn load(flag_key: Option<String>, flag_base: Option<String>) -> anyhow::Result<Self> {
        let dir = store::config_dir();
        let file_key = store::load_credentials(&dir)?;
        let file_base = store::load_config(&dir)?;
        let (api_key, key_source) =
            resolve_key(flag_key, std::env::var("TT_API_KEY").ok(), file_key);
        let (base_url, base_source) =
            resolve_base(flag_base, std::env::var("TT_API_BASE").ok(), file_base);
        Ok(Self {
            api_key,
            key_source,
            base_url,
            base_source,
        })
    }

    /// The API key as a plain `String` for the (String-typed) consumers.
    pub fn api_key_string(&self) -> Option<String> {
        self.api_key.as_ref().map(|s| s.expose().to_string())
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p tt-cli context::tests`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/context/mod.rs
git commit -m "feat(cli): credential/base-url resolver (flag>env>file>default)"
```

---

## Task 4: Account commands (`account/mod.rs`)

**Files:**
- Modify: `crates/cli/src/lib.rs`
- Create: `crates/cli/src/account/mod.rs`

- [ ] **Step 1: Declare the module and write the failing test**

In `crates/cli/src/lib.rs`, add:

```rust
pub mod account;
```

Create `crates/cli/src/account/mod.rs` with **only the test module first**:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_from_arg() {
        assert_eq!(decide_token(Some("tt_live_x".into()), None).unwrap(), "tt_live_x");
    }

    #[test]
    fn token_trimmed_from_stdin() {
        let t = decide_token(Some("-".into()), Some("tt_test_y\n".into())).unwrap();
        assert_eq!(t, "tt_test_y");
    }

    #[test]
    fn empty_token_is_rejected() {
        assert!(decide_token(Some("   ".into()), None).is_err());
        assert!(decide_token(Some("-".into()), Some("\n".into())).is_err());
        assert!(decide_token(Some("-".into()), None).is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails (does not compile)**

Run: `cargo test -p tt-cli account`
Expected: FAIL — compile errors (`decide_token` undefined).

- [ ] **Step 3: Write the account implementation**

Add the impl **above** the test module in `crates/cli/src/account/mod.rs`:

```rust
//! `tt login` / `tt logout` / `tt whoami` command bodies. The credential store
//! and resolver live in `crate::context`.

use anyhow::Context as _;

use crate::context::{self, store};

/// Pure decision: resolve the token text from the `--token` arg and (when the
/// arg is `-`) the stdin contents. Errors on a missing / blank token.
pub fn decide_token(arg: Option<String>, stdin: Option<String>) -> anyhow::Result<String> {
    match arg.as_deref() {
        None => anyhow::bail!("no token provided"),
        Some("-") => {
            let s = stdin.unwrap_or_default();
            let t = s.trim();
            if t.is_empty() {
                anyhow::bail!("no token on stdin");
            }
            Ok(t.to_string())
        }
        Some(other) => {
            let t = other.trim();
            if t.is_empty() {
                anyhow::bail!("empty --token");
            }
            Ok(t.to_string())
        }
    }
}

/// `tt login --token <KEY>` (browser login lands in V2). `--token -` reads the
/// key from stdin (keeps it out of shell history). Optionally persists base URL.
pub fn login_with_token(token: Option<String>, base_url: Option<String>) -> anyhow::Result<()> {
    if token.is_none() {
        anyhow::bail!(
            "browser login arrives in V2 — use `tt login --token <KEY>` for now \
             (get a key at app.tokentrimmer.com)"
        );
    }
    let stdin = if token.as_deref() == Some("-") {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
            .context("read token from stdin")?;
        Some(s)
    } else {
        None
    };
    let raw = decide_token(token, stdin)?;
    let validated = tt_mcp::auth::validate_api_key(Some(raw))
        .map_err(|e| anyhow::anyhow!("invalid key: {e}"))?;

    let dir = store::config_dir();
    store::save_credentials(&dir, &validated)?;
    if let Some(b) = base_url.filter(|s| !s.trim().is_empty()) {
        store::save_config(&dir, b.trim())?;
    }
    let base = store::load_config(&dir)?.unwrap_or_else(|| context::DEFAULT_BASE_URL.to_string());
    println!(
        "Logged in. Stored {} in {} (base: {}).",
        context::mask_key(&validated),
        dir.join("credentials.toml").display(),
        base,
    );
    Ok(())
}

/// `tt whoami` — local only (no network in V0). Exit 1 when no key is configured.
pub fn whoami() -> anyhow::Result<()> {
    let ctx = context::ResolvedContext::load(None, None)?;
    match &ctx.api_key {
        Some(k) => {
            println!("Logged in.");
            println!(
                "  key:    {} (source: {})",
                context::mask_key(k.expose()),
                ctx.key_source
            );
            println!("  base:   {} (source: {})", ctx.base_url, ctx.base_source);
            println!("  config: {}", store::config_dir().display());
            Ok(())
        }
        None => {
            println!("Not logged in. Run `tt login --token <KEY>` or set TT_API_KEY.");
            println!("  base: {} (source: {})", ctx.base_url, ctx.base_source);
            std::process::exit(1);
        }
    }
}

/// `tt logout` — remove the local key only (does NOT revoke server-side).
pub fn logout() -> anyhow::Result<()> {
    let dir = store::config_dir();
    if store::delete_credentials(&dir)? {
        println!("Logged out — removed {}.", dir.join("credentials.toml").display());
        println!(
            "Note: this only clears the local key; it does not revoke it server-side. \
             Revoke in the dashboard if it may be compromised."
        );
    } else {
        println!("Not logged in (nothing to remove).");
    }
    Ok(())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p tt-cli account`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/lib.rs crates/cli/src/account/mod.rs
git commit -m "feat(cli): tt login --token / whoami / logout"
```

---

## Task 5: Wire `Login`/`Logout`/`Whoami` into `main.rs`

**Files:**
- Modify: `crates/cli/src/main.rs`

- [ ] **Step 1: Add the command variants**

In `crates/cli/src/main.rs`, in the `enum Command { … }` (after the `Mcp { … }` variant, before `Init`), add:

```rust
    /// Store a TokenTrimmer API key for this machine (browser login lands in V2).
    Login {
        /// The tt_live_/tt_test_ key. Use `-` to read it from stdin.
        #[arg(long)]
        token: Option<String>,
        /// Persist a gateway base URL alongside the key.
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Remove the locally stored API key (does not revoke it server-side).
    Logout,
    /// Show the resolved API key (masked), its source, and the gateway base URL.
    Whoami,
```

- [ ] **Step 2: Add the dispatch arms**

In the main `match` over `Command` (the same `match` that contains `Command::Proxy { … } => { … }`), add three arms — e.g. immediately before `Command::Proxy`:

```rust
        Command::Login { token, base_url } => {
            tt_cli::account::login_with_token(token, base_url)?;
        }
        Command::Logout => {
            tt_cli::account::logout()?;
        }
        Command::Whoami => {
            tt_cli::account::whoami()?;
        }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p tt-cli`
Expected: SUCCESS (no errors).

- [ ] **Step 4: Smoke-test the new commands against a temp HOME**

Run:
```bash
HOME=$(mktemp -d) ./target/debug/tt login --token tt_test_smoke123 && \
HOME="$HOME" ./target/debug/tt whoami; echo "exit=$?"
```
Expected: `login` prints `Logged in. Stored tt_test_smok… …`; `whoami` prints the masked key with `source: credentials.toml`, base `https://api.tokentrimmer.com (source: default)`, `exit=0`. (Note: build first with `cargo build -p tt-cli`; the same `HOME` must be reused across the two commands — run them in one shell.)

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/main.rs
git commit -m "feat(cli): wire tt login/logout/whoami commands"
```

---

## Task 6: Rewire `Mcp` + `Proxy` to the resolver; fix the default + docs

**Files:**
- Modify: `crates/cli/src/main.rs`
- Modify: `crates/cli/src/proxy/config.rs`
- Modify: `GETTING_STARTED.md`

- [ ] **Step 1: Make `Mcp`'s base URL flag optional**

In `crates/cli/src/main.rs`, in the `Command::Mcp { … }` variant, change the base-URL arg from a defaulted `String` to an `Option<String>`:

```rust
        #[arg(long)]
        tt_api_base: Option<String>,
```
(Remove the `default_value = "https://tokentrimmer.fly.dev"`.)

- [ ] **Step 2: Resolve the Mcp key + base via `ResolvedContext`**

In the `Command::Mcp { … } => { … }` arm, replace the two resolution lines:

```rust
            let api_key = tt_api_key.or_else(|| std::env::var("TT_API_KEY").ok());
            let api_key = auth::validate_api_key(api_key)?;
```
with:

```rust
            let ctx = tt_cli::context::ResolvedContext::load(tt_api_key, tt_api_base)?;
            let api_key = auth::validate_api_key(ctx.api_key_string())?;
            let tt_api_base = ctx.base_url;
```
(`auth` here is `tt_mcp::auth`, already imported in this arm. The shadowed `tt_api_base: String` is then used by the existing `tt_api_base.clone()` tool constructors unchanged.)

- [ ] **Step 3: Add a base-URL flag to `Proxy`**

In the `Command::Proxy { … }` variant, add after `tt_api_key`:

```rust
        #[arg(long)]
        tt_api_base: Option<String>,
```
and add `tt_api_base,` to the destructure in the `Command::Proxy { … } => {` arm's pattern.

- [ ] **Step 4: Resolve the Proxy key + base via `ResolvedContext`**

In the `Command::Proxy { … } => { … }` arm, replace:

```rust
            let api_key = tt_api_key.or_else(|| std::env::var("TT_API_KEY").ok());
            if mode == Mode::Gateway && api_key.is_none() {
                anyhow::bail!("--mode gateway requires --tt-api-key or TT_API_KEY env");
            }
            let cfg = Config::build(
                port,
                bind_addr,
                mode,
                api_key,
                no_tui,
                no_preview,
                session_log.map(std::path::PathBuf::from),
            );
```
with:

```rust
            let ctx = tt_cli::context::ResolvedContext::load(tt_api_key, tt_api_base)?;
            let api_key = ctx.api_key_string();
            if mode == Mode::Gateway && api_key.is_none() {
                anyhow::bail!(
                    "--mode gateway requires a key — run `tt login --token <KEY>`, \
                     pass --tt-api-key, or set TT_API_KEY"
                );
            }
            let mut cfg = Config::build(
                port,
                bind_addr,
                mode,
                api_key,
                no_tui,
                no_preview,
                session_log.map(std::path::PathBuf::from),
            );
            cfg.gateway_base_url = ctx.base_url;
```

- [ ] **Step 5: Change the proxy default base URL**

In `crates/cli/src/proxy/config.rs`, change line 58:

```rust
            gateway_base_url: "https://api.tokentrimmer.com".into(),
```
(The existing `build_sets_defaults` test asserts only `contains("tokentrimmer")`, which still holds.)

- [ ] **Step 6: Update the docs**

In `GETTING_STARTED.md`, the line that reads (around line 279):

```
**Requires:** a `tt_live_…` key (via `--tt-api-key` or `TT_API_KEY`). The hosted API base defaults to `https://tokentrimmer.fly.dev` (override with `--tt-api-base`).
```
Replace with:

```
**Requires:** a `tt_live_…` key — run `tt login --token <KEY>`, or pass `--tt-api-key` / set `TT_API_KEY`. The hosted API base defaults to `https://api.tokentrimmer.com` (override with `--tt-api-base` or `TT_API_BASE`).
```

- [ ] **Step 7: Verify build + existing tests still pass**

Run: `cargo build -p tt-cli && cargo test -p tt-cli`
Expected: SUCCESS; all tests pass (including `proxy::config::tests::build_sets_defaults`).

- [ ] **Step 8: Commit**

```bash
git add crates/cli/src/main.rs crates/cli/src/proxy/config.rs GETTING_STARTED.md
git commit -m "feat(cli): resolve Mcp+Proxy key/base via context; default api.tokentrimmer.com"
```

---

## Task 7: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt -p tt-cli`
Then: `git diff --quiet || git commit -am "style: cargo fmt (tt-cli)"`
Expected: no unrelated churn; commit only if fmt changed something.

- [ ] **Step 2: Clippy (the repo gates on `-D warnings`)**

Run: `cargo clippy -p tt-cli --all-targets -- -D warnings`
Expected: no warnings/errors.

- [ ] **Step 3: Build + full crate test**

Run: `cargo build -p tt-cli && cargo test -p tt-cli`
Expected: SUCCESS; all tests pass.

- [ ] **Step 4: End-to-end manual smoke (temp HOME — no real `$HOME` writes)**

Run, in one shell:
```bash
export TTHOME=$(mktemp -d)
cargo build -p tt-cli
HOME="$TTHOME" ./target/debug/tt whoami; echo "logged-out exit=$?"        # expect exit=1
HOME="$TTHOME" ./target/debug/tt login --token tt_test_v0smoke
HOME="$TTHOME" ./target/debug/tt whoami; echo "logged-in exit=$?"          # expect exit=0, masked key, base api.tokentrimmer.com
echo 'tt_test_fromstdin' | HOME="$TTHOME" ./target/debug/tt login --token -  # stdin path
HOME="$TTHOME" ./target/debug/tt logout
HOME="$TTHOME" ./target/debug/tt whoami; echo "after-logout exit=$?"        # expect exit=1
test "$(ls -ld "$TTHOME/.tokentrimmer" | cut -c1-10)" = "drwx------" && echo "dir 0700 OK"
```
Expected: the exit codes/labels as annotated; the masked key never prints in full; `credentials.toml` is `-rw-------` (0600) and `~/.tokentrimmer` is `drwx------` (0700) on unix.

- [ ] **Step 5: Confirm clean tree + commits**

```bash
git status
git log --oneline -8
```
Expected: clean tree; the Task 1–6 feature commits present on `feat/v0-cli-foundation`.

---

## Self-Review (completed by plan author)

**1. Spec coverage** — every spec goal maps to a task:
- One resolver seam, precedence flag>env>file (key) and flag>env>file>default (base) → `resolve_key`/`resolve_base` + `ResolvedContext::load` (Task 3); consumed by Mcp+Proxy (Task 6).
- Default `https://api.tokentrimmer.com` → `DEFAULT_BASE_URL` (Task 3) + `proxy/config.rs` (Task 6 Step 5).
- Local store usable now (`tt login --token`, `-`=stdin, optional `--base-url`) → `account::login_with_token` (Task 4) + command (Task 5).
- `tt whoami` (masked, source, base; exit 1 if none) → Task 4 + Task 5; `mask_key` (Task 3).
- `tt logout` (local only; states no server-side revoke) → Task 4 + Task 5.
- Files `credentials.toml` 0600 / `config.toml` / dir 0700; corrupt → loud error → `context/store.rs` (Task 2).
- Wire Mcp (`tt_api_base`→Option, default owned by Context) + Proxy (new `--tt-api-base`, base into Config, key bail → `tt login`) → Task 6.
- `.gitignore` + `tt init` template → Task 1. Docs → Task 6 Step 6. `SecretString` for the in-memory key → Task 3.

**2. Placeholder scan** — no TBD/TODO/"handle errors"; every code step has complete code; every command has expected output.

**3. Type consistency** — `ResolvedContext { api_key: Option<SecretString>, key_source: KeySource, base_url: String, base_source: BaseSource }` defined once (Task 3) and consumed via `api_key_string()`/`base_url` in Task 6. `resolve_key`/`resolve_base`/`mask_key`/`DEFAULT_BASE_URL` names stable across Tasks 3–6. Store fns (`config_dir`/`load_credentials`/`save_credentials`/`delete_credentials`/`load_config`/`save_config`) defined in Task 2, used unchanged in Tasks 3–4. `validate_api_key(Option<String>)→Result<String>` consumed with `ctx.api_key_string()` (Task 6) and `Some(raw)` (Task 4), matching its signature. MCP tools keep their `String` `api_key`/`base_url` fields (fed by the shadowed `tt_api_base: String` + `api_key: String`).

**Deviation from spec (noted):** the command module is `crate::account` (not `crate::auth`) to avoid confusion with `tt_mcp::auth` (imported as `auth` in the Mcp arm). Behavior is identical to the spec.
