# V2 — Browser-Assisted Login Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `tt login` (no `--token`) opens the dashboard keys page in the browser and reads the pasted key; `--token`/`--token -` stay for non-interactive use.

**Architecture:** All in `crates/cli/src/account/mod.rs`: a pure `browser_command_for`, a shared `store_key` (extracted from the current body), and `login` dispatching to a `browser_login` flow. `main.rs` gains `--no-browser`.

**Tech Stack:** Rust, `dialoguer` (hidden key paste), `console` (TTY check), `std::process::Command` (browser open) — all existing deps.

---

### Task 1: `browser_command_for` (pure, test-first)

**Files:**
- Modify: `crates/cli/src/account/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/cli/src/account/mod.rs`:

```rust
    #[test]
    fn browser_command_per_os() {
        assert_eq!(
            browser_command_for("macos", "http://x"),
            Some(("open", vec!["http://x".to_string()]))
        );
        assert_eq!(
            browser_command_for("linux", "http://x"),
            Some(("xdg-open", vec!["http://x".to_string()]))
        );
        let (prog, args) = browser_command_for("windows", "http://x").unwrap();
        assert_eq!(prog, "cmd");
        assert_eq!(
            args,
            vec!["/C".to_string(), "start".to_string(), String::new(), "http://x".to_string()]
        );
        assert!(browser_command_for("plan9", "http://x").is_none());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-cli --lib account::tests::browser_command_per_os 2>&1 | tail -8`
Expected: FAIL to compile — `cannot find function browser_command_for`.

- [ ] **Step 3: Add the function**

In `crates/cli/src/account/mod.rs`, add after the `decide_token` function:

```rust
/// The OS-specific command to open `url` in the default browser. `None` for an
/// unrecognized OS (the caller then just prints the URL).
#[must_use]
pub fn browser_command_for(os: &str, url: &str) -> Option<(&'static str, Vec<String>)> {
    match os {
        "macos" => Some(("open", vec![url.to_string()])),
        "linux" => Some(("xdg-open", vec![url.to_string()])),
        // The empty title arg keeps `start` from treating a quoted URL as a title.
        "windows" => Some((
            "cmd",
            vec!["/C".into(), "start".into(), String::new(), url.to_string()],
        )),
        _ => None,
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-cli --lib account::tests 2>&1 | tail -8`
Expected: PASS (`browser_command_per_os` + the existing `decide_token` tests).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/account/mod.rs
git commit -m "feat(account): browser_command_for (per-OS open command)"
```

---

### Task 2: `store_key` + `login`/`browser_login` + `--no-browser`

**Files:**
- Modify: `crates/cli/src/account/mod.rs`
- Modify: `crates/cli/src/main.rs`

- [ ] **Step 1: Add the `DASHBOARD_KEYS_URL` constant + `store_key` + `open_browser` + `browser_login`, and replace `login_with_token` with `login`**

In `crates/cli/src/account/mod.rs`, add the constant near the top (after the `use` lines):

```rust
/// The dashboard page where a user mints an API key (the cloud dashboard; the
/// public gateway only verifies keys). One-line change if the route differs.
const DASHBOARD_KEYS_URL: &str = "https://dashboard.tokentrimmer.com/keys";
```

Replace the whole `login_with_token` function with `store_key` + `open_browser` + `browser_login` + `login`:

```rust
/// Validate + persist a raw key (and optional base URL), printing the result.
/// Shared by the `--token` and browser paths.
fn store_key(raw: &str, base_url: Option<String>) -> anyhow::Result<()> {
    let validated = tt_mcp::auth::validate_api_key(Some(raw.to_string()))
        .map_err(|e| anyhow::anyhow!("invalid key: {e}"))?;
    let dir = store::config_dir();
    store::save_credentials(&dir, &validated)?;
    if let Some(b) = base_url.filter(|s| !s.trim().is_empty()) {
        store::save_config(&dir, b.trim())?;
    }
    let base = store::load_config(&dir)?.unwrap_or_else(|| context::DEFAULT_BASE_URL.to_string());
    ui::success(&format!(
        "Logged in. Stored {} in {} (base: {}).",
        context::mask_key(&validated),
        dir.join("credentials.toml").display(),
        base,
    ));
    Ok(())
}

/// Best-effort: open `url` in the default browser. Returns whether it launched.
fn open_browser(url: &str) -> bool {
    let Some((prog, args)) = browser_command_for(std::env::consts::OS, url) else {
        return false;
    };
    std::process::Command::new(prog)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `tt login` with no `--token`: open the dashboard keys page + read the pasted
/// key (hidden). Interactive only — non-interactive callers use `--token`.
fn browser_login(base_url: Option<String>, no_browser: bool) -> anyhow::Result<()> {
    if !console::user_attended() {
        anyhow::bail!(
            "browser login needs an interactive terminal — use `tt login --token <KEY>` \
             (create a key at {DASHBOARD_KEYS_URL})"
        );
    }
    ui::info("Opening the TokenTrimmer dashboard to create an API key…");
    if !no_browser {
        open_browser(DASHBOARD_KEYS_URL);
    }
    ui::note(&format!("If your browser didn't open, visit: {DASHBOARD_KEYS_URL}"));
    let key = dialoguer::Password::new()
        .with_prompt("Paste your API key")
        .interact()
        .context("read API key")?;
    store_key(key.trim(), base_url)
}

/// `tt login`. With `--token` (or `--token -` for stdin) it stores that key;
/// without, it runs the browser-assisted flow.
pub fn login(
    token: Option<String>,
    base_url: Option<String>,
    no_browser: bool,
) -> anyhow::Result<()> {
    let Some(tok) = token else {
        return browser_login(base_url, no_browser);
    };
    let stdin = if tok == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
            .context("read token from stdin")?;
        Some(s)
    } else {
        None
    };
    let raw = decide_token(Some(tok), stdin)?;
    store_key(&raw, base_url)
}
```

- [ ] **Step 2: Wire `main.rs` (`--no-browser` + dispatch to `login`)**

In `crates/cli/src/main.rs`, in the `Login { … }` command, add after `base_url`:

```rust
        /// Don't open a browser; just print the URL to visit (headless/SSH).
        #[arg(long)]
        no_browser: bool,
```

Change the dispatch arm:

```rust
        Command::Login {
            token,
            base_url,
            no_browser,
        } => {
            tt_cli::account::login(token, base_url, no_browser)?;
        }
```

- [ ] **Step 3: Build + tests**

Run: `cargo build -p tt-cli 2>&1 | grep -E "^error|never used" | head` then `cargo test -p tt-cli --lib account 2>&1 | tail -8`
Expected: no errors / no dead-code; account tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/account/mod.rs crates/cli/src/main.rs
git commit -m "feat(account): browser-assisted tt login (paste flow) + --no-browser"
```

---

### Task 3: Gates + smoke + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt -p tt-cli && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings.

- [ ] **Step 2: Full tt-cli tests**

Run: `cargo test -p tt-cli 2>&1 | grep -E "test result|error\[" | tail -8`
Expected: all pass.

- [ ] **Step 3: Smoke (piped → non-TTY bail; --token path intact; --help)**

Run:
```bash
cargo build -q -p tt-cli --bin tt
echo "--- no token + piped stdin (non-TTY) → interactive-terminal bail ---"
printf '' | target/debug/tt login 2>&1 | tail -2
echo "--- --token path still stores (then logout to clean up) ---"
target/debug/tt login --token tt_live_smoketestkey000000 2>&1 | tail -1
target/debug/tt logout 2>&1 | tail -1
echo "--- --help shows --no-browser ---"
target/debug/tt login --help 2>&1 | grep -A1 "no-browser"
```
Expected: piped `tt login` → "browser login needs an interactive terminal — use `tt login --token`"; `--token` stores then logout removes; `--help` lists `--no-browser`. (Browser open + hidden paste is a manual check.)

- [ ] **Step 4: cargo-deny**

Run: `cargo deny check advisories 2>&1 | tail -3`
Expected: `advisories ok` (no new deps).

- [ ] **Step 5: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** `browser_command_for` (T1), `store_key` extraction + `login`/`browser_login`/`open_browser` + `DASHBOARD_KEYS_URL` (T2), `--no-browser` + dispatch (T2), gates/smoke (T3). All spec items covered.
- **Placeholders:** none — full code in every step.
- **Type consistency:** `browser_command_for(&str,&str)->Option<(&'static str, Vec<String>)>`, `store_key(&str, Option<String>)->Result<()>`, `open_browser(&str)->bool`, `browser_login(Option<String>, bool)->Result<()>`, `login(Option<String>, Option<String>, bool)->Result<()>`; `main.rs` destructures `{token, base_url, no_browser}` and calls `login(token, base_url, no_browser)`.
- **Behaviour:** the `--token`/`--token -` path is byte-identical to V0 (same `decide_token` + validate + store + success line via `store_key`); only the `token is None` path changes (was a bail, now the browser flow). The non-TTY guard keeps piped/CI callers pointed at `--token`.
