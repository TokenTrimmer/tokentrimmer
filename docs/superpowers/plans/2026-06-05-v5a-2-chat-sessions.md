# V5a-2 `tt chat` Sessions + Cost Ledger Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use checkbox (`- [ ]`).

**Goal:** Persist conversations (save/resume/list) + a per-session cost ledger.

**Architecture:** `chat/session.rs` (pure name helpers + dir-parameterized save/load/list, unit-tested with tempdirs); `Conversation` gains serde; a tested `Ledger`; new REPL commands + `--resume`.

**Tech Stack:** `tt-cli`. Spec: `docs/superpowers/specs/2026-06-05-v5a-2-chat-sessions-design.md`.

---

## Task 1: `session.rs` (persistence) + `Conversation` serde

**Files:** Modify `crates/cli/src/chat/mod.rs` (serde on `Conversation`, `mod session;`); Create `crates/cli/src/chat/session.rs`

- [ ] **Step 1: `Conversation` serde** — in `chat/mod.rs`, change `pub struct Conversation {` to derive serde:
```rust
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Conversation {
```
And add `pub mod session;` near the top of `chat/mod.rs` (after the `use` lines).

- [ ] **Step 2: Write the failing tests** — create `crates/cli/src/chat/session.rs` with the test module first:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Conversation;

    #[test]
    fn sanitize_blocks_traversal_and_seps() {
        let s = sanitize_name("../../etc/passwd");
        assert!(!s.contains('/') && !s.contains('.'), "got {s:?}");
        assert_eq!(sanitize_name("My Chat!"), "my-chat");
        assert_eq!(sanitize_name(""), "chat");
        assert_eq!(sanitize_name("rust-help"), "rust-help");
    }

    #[test]
    fn auto_name_from_first_message() {
        let mut c = Conversation::new("m".into(), None);
        c.push_user("Explain the borrow checker please".into());
        assert_eq!(auto_name(&c), "explain-the-borrow-checker-please");
        assert_eq!(auto_name(&Conversation::new("m".into(), None)), "chat");
    }

    #[test]
    fn save_load_roundtrip_and_path_stays_in_dir() {
        let dir = std::env::temp_dir().join(format!("tt-sess-{}", std::process::id()));
        let mut c = Conversation::new("gpt-4o".into(), Some("be brief".into()));
        c.push_user("hi".into());
        c.push_assistant("yo".into());
        let p = save(&dir, "../../escape", &c).unwrap();
        assert_eq!(p.parent().unwrap(), dir, "path must stay in sessions dir");
        let loaded = load(&dir, "../../escape").unwrap();
        assert_eq!(loaded.model, "gpt-4o");
        assert_eq!(loaded.messages.len(), 2);
        let metas = list(&dir).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].turns, 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 3: Run → fail** `cargo test -p tt-cli --lib chat::session` (missing items).

- [ ] **Step 4: Implement** (prepend above the test module in `session.rs`):
```rust
//! On-disk chat sessions under `~/.tokentrimmer/sessions/`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::chat::Conversation;
use crate::context::store;

/// Sessions directory (`~/.tokentrimmer/sessions/`).
#[must_use]
pub fn sessions_dir() -> PathBuf {
    store::config_dir().join("sessions")
}

/// Sanitize a user-supplied session name to a safe file stem. Drops `.` and any
/// non-`[a-z0-9_-]` char (→ `-`), so the result can never contain a path
/// separator or `..` — `/resume ../../x` stays inside the sessions dir.
#[must_use]
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "chat".to_string()
    } else {
        s
    }
}

/// Derive a session name from the first user message (≤5 words), else `"chat"`.
#[must_use]
pub fn auto_name(conv: &Conversation) -> String {
    use tt_shared::messages::{Message, MessageContent};
    let first = conv.messages.iter().find_map(|m| match m {
        Message::User {
            content: MessageContent::Text(t),
            ..
        } => Some(t.as_str()),
        _ => None,
    });
    match first {
        Some(t) => {
            let words: Vec<&str> = t.split_whitespace().take(5).collect();
            sanitize_name(&words.join("-"))
        }
        None => "chat".to_string(),
    }
}

fn path_for(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.json", sanitize_name(name)))
}

/// Save `conv` as `<dir>/<sanitized name>.json`. Returns the path written.
pub fn save(dir: &Path, name: &str, conv: &Conversation) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = path_for(dir, name);
    let json = serde_json::to_string_pretty(conv).context("serialize session")?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Load a session by name.
pub fn load(dir: &Path, name: &str) -> anyhow::Result<Conversation> {
    let path = path_for(dir, name);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("no session `{}` ({})", sanitize_name(name), path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// One saved session's metadata for `/sessions`.
pub struct SessionMeta {
    pub name: String,
    pub model: String,
    pub turns: usize,
    pub modified: std::time::SystemTime,
}

/// List saved sessions, newest first. Missing dir → empty.
pub fn list(dir: &Path) -> anyhow::Result<Vec<SessionMeta>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(conv) = serde_json::from_str::<Conversation>(&raw) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        out.push(SessionMeta {
            name: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string(),
            model: conv.model,
            turns: conv.messages.len() / 2,
            modified,
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(out)
}
```

- [ ] **Step 5: Run → green** `cargo test -p tt-cli --lib chat::session`.
- [ ] **Step 6: Commit** `git add crates/cli/src/chat && git commit -m "feat(cli): tt chat session persistence (save/load/list, sanitized names)"`

---

## Task 2: Ledger + commands + `--resume`

**Files:** Modify `crates/cli/src/chat/mod.rs`, `crates/cli/src/main.rs`

- [ ] **Step 1: Ledger test** (in `chat/mod.rs` tests):
```rust
    #[test]
    fn ledger_accumulates() {
        console::set_colors_enabled(false);
        let mut l = Ledger::default();
        l.add(&UsageInfo { cost_usd: 0.001, baseline_cost_usd: 0.004, saved_usd: 0.003, input_tokens: 1, output_tokens: 1, cached_tokens: 0 });
        l.add(&UsageInfo { cost_usd: 0.001, baseline_cost_usd: 0.004, saved_usd: 0.003, input_tokens: 1, output_tokens: 1, cached_tokens: 0 });
        assert_eq!(l.turns, 2);
        let s = l.summary();
        assert!(s.contains("2 turn"), "{s}");
        assert!(s.contains("75%"), "{s}");
    }
```
- [ ] **Step 2: Implement `Ledger`** (in `chat/mod.rs`, near `Conversation`):
```rust
/// Running cost/savings totals for the current chat session.
#[derive(Default)]
pub struct Ledger {
    pub turns: u32,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub baseline_usd: f64,
}
impl Ledger {
    pub fn add(&mut self, u: &UsageInfo) {
        self.turns += 1;
        self.cost_usd += u.cost_usd;
        self.saved_usd += u.saved_usd;
        self.baseline_usd += u.baseline_cost_usd;
    }
    #[must_use]
    pub fn summary(&self) -> String {
        let pct = if self.baseline_usd > 0.0 {
            (self.saved_usd / self.baseline_usd * 100.0).round()
        } else {
            0.0
        };
        format!(
            "session: {} turn(s) · ${:.4} spent · saved ${:.4} ({pct:.0}%)",
            self.turns, self.cost_usd, self.saved_usd
        )
    }
}
```
- [ ] **Step 3: `stream_turn` returns usage** — change its return to `anyhow::Result<(String, Option<UsageInfo>)>`; at the end `Ok((reply, usage))` (the footer still prints inside). Update the caller (Task 2 Step 5).
- [ ] **Step 4: New `Command` variants** — add to the enum: `Save(Option<String>)`, `Resume(String)`, `Sessions`, `Cost`. In `Command::parse`, add arms: `"save" => Command::Save(arg)`, `"resume" | "load" => match arg { Some(n) => Command::Resume(n), None => Command::Unknown("resume".into()) }`, `"sessions" => Command::Sessions`, `"cost" => Command::Cost`. (A `/resume` with no name → Unknown / usage hint.)
- [ ] **Step 5: Wire into `run`** — `run` gains a `resume: Option<String>` param. Before the loop, if `resume` is `Some(n)`, `conv = session::load(&session::sessions_dir(), &n)?` (error → `ui::error`, keep fresh). Add `let mut ledger = Ledger::default();`. In the Chat arm, on `Ok((reply, usage))`: `conv.push_assistant(reply); if let Some(u) = usage { ledger.add(&u); }`. Add the command arms:
```rust
    Command::Save(name) => {
        let n = name.unwrap_or_else(|| session::auto_name(&conv));
        match session::save(&session::sessions_dir(), &n, &conv) {
            Ok(p) => ui::success(&format!("saved session → {}", p.display())),
            Err(e) => ui::error(&format!("{e:#}")),
        }
    }
    Command::Resume(name) => match session::load(&session::sessions_dir(), &name) {
        Ok(c) => { conv = c; ui::info(&format!("(resumed · {} messages)", conv.messages.len())); }
        Err(e) => ui::error(&format!("{e:#}")),
    },
    Command::Sessions => {
        let metas = session::list(&session::sessions_dir()).unwrap_or_default();
        if metas.is_empty() {
            ui::info("no saved sessions");
        } else {
            let mut t = ui::table(&["NAME", "MODEL", "TURNS"], console::colors_enabled());
            for m in metas {
                t.add_row(vec![m.name, m.model, m.turns.to_string()]);
            }
            println!("{t}");
        }
    }
    Command::Cost => ui::info(&ledger.summary()),
```
Update `print_help` to list `/save`, `/resume`, `/sessions`, `/cost`.
- [ ] **Step 6: `--resume` flag** — `main.rs` `Chat { … }` gains `#[arg(long)] resume: Option<String>,`; pass it into `chat::run(model, system, resume, tt_api_key, tt_api_base)`. Update `run`'s signature + the call.
- [ ] **Step 7: Run** `cargo test -p tt-cli` → green.
- [ ] **Step 8: Commit** `git add crates/cli/src/chat/mod.rs crates/cli/src/main.rs && git commit -m "feat(cli): tt chat /save /resume /sessions /cost + --resume"`

---

## Task 3: Verification

- [ ] **Step 1:** `cargo fmt -p tt-cli && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p tt-cli` → clean + green.
- [ ] **Step 2: Smoke** — `tt chat --help` shows `--resume`; (with a key) `/save x` → file under `~/.tokentrimmer/sessions/`, `/sessions` lists it, `/resume x` loads it, `/cost` prints a ledger.
- [ ] **Step 3: Commit** any fmt: `git commit -am "style: cargo fmt (v5a-2)" || echo none`

---

## Self-review notes
- `sanitize_name` drops `.` and path separators → built paths provably stay in the sessions dir (tested with `../../escape`).
- save/load/list take a `dir` param → unit-tested with a tempdir; the REPL uses `sessions_dir()`.
- `stream_turn` now returns the `UsageInfo` so `run` can feed the `Ledger`.
- `/sessions` uses the V1 `ui::table` (plain when piped).
