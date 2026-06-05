# V5b-1 — `tt chat` Ergonomics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `/editor` (multi-line via `$EDITOR`), `/retry` (re-run last turn), and `/copy` (OSC52 clipboard) to `tt chat`, factoring the turn flow into a shared `do_turn`.

**Architecture:** All changes in `crates/cli/src/chat/mod.rs` (+ one dep line in `crates/cli/Cargo.toml`). Pure helpers (`osc52_copy`, `prepare_retry`, `last_assistant_text`) are unit-tested; the REPL wiring (`do_turn`, `compose_in_editor`, `run` arms) is verified by build + a piped smoke test. Raw live streaming is unchanged.

**Tech Stack:** Rust, `base64` (OSC52 encoding), `std::process::Command` (editor launch), existing `ui`/`rustyline`/`reqwest` plumbing.

---

### Task 1: `base64` dep + `osc52_copy` helper

**Files:**
- Modify: `crates/cli/Cargo.toml` (add `base64`)
- Modify: `crates/cli/src/chat/mod.rs` (add `osc52_copy` + test)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/cli/src/chat/mod.rs`:

```rust
    #[test]
    fn osc52_wraps_base64() {
        let s = osc52_copy("hi");
        assert!(s.starts_with("\x1b]52;c;"), "{s:?}");
        assert!(s.ends_with('\x07'), "{s:?}");
        assert!(s.contains("aGk="), "base64 of 'hi' missing: {s:?}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-cli --lib chat::tests::osc52_wraps_base64 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find function osc52_copy`.

- [ ] **Step 3: Add the dependency**

In `crates/cli/Cargo.toml`, under `[dependencies]`, add after the `dirs = "5.0"` line:

```toml
base64 = "0.22"
```

- [ ] **Step 4: Add the helper**

In `crates/cli/src/chat/mod.rs`, add immediately after the `format_turn_footer` function (after its closing `}`):

```rust
/// Build the OSC52 terminal escape that copies `text` to the system clipboard.
/// Works locally and over SSH — no platform clipboard dependency. Best-effort:
/// terminals that don't support OSC52 simply ignore it.
#[must_use]
pub fn osc52_copy(text: &str) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{b64}\x07")
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p tt-cli --lib chat::tests::osc52_wraps_base64 2>&1 | tail -20`
Expected: PASS (1 passed).

- [ ] **Step 6: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/src/chat/mod.rs
git commit -m "feat(chat): osc52_copy helper for /copy"
```

---

### Task 2: `prepare_retry` + `last_assistant_text` helpers

**Files:**
- Modify: `crates/cli/src/chat/mod.rs` (two private helpers + tests)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
    #[test]
    fn prepare_retry_drops_assistant_keeps_user() {
        let mut c = Conversation::new("m".into(), None);
        assert!(!prepare_retry(&mut c), "empty conv: nothing to retry");
        c.push_user("hi".into());
        c.push_assistant("yo".into());
        assert!(prepare_retry(&mut c), "should drop assistant, user remains");
        assert_eq!(c.messages.len(), 1);
        assert!(prepare_retry(&mut c), "user still present, no assistant to pop");
        assert_eq!(c.messages.len(), 1);
    }

    #[test]
    fn last_assistant_text_finds_latest() {
        let mut c = Conversation::new("m".into(), None);
        assert!(last_assistant_text(&c).is_none());
        c.push_user("hi".into());
        c.push_assistant("first".into());
        c.push_user("more".into());
        c.push_assistant("second".into());
        assert_eq!(last_assistant_text(&c).as_deref(), Some("second"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tt-cli --lib chat::tests::prepare_retry 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find function prepare_retry` / `last_assistant_text`.

- [ ] **Step 3: Add the helpers**

In `crates/cli/src/chat/mod.rs`, add after the `Ledger` `impl` block (after its closing `}`):

```rust
/// Prepare the conversation to re-run the last turn: drop a trailing assistant
/// reply if present. Returns true iff the conversation now ends with a user
/// message (something to retry).
fn prepare_retry(conv: &mut Conversation) -> bool {
    if matches!(conv.messages.last(), Some(Message::Assistant { .. })) {
        conv.messages.pop();
    }
    matches!(conv.messages.last(), Some(Message::User { .. }))
}

/// The text of the most recent assistant reply, if any.
#[must_use]
fn last_assistant_text(conv: &Conversation) -> Option<String> {
    conv.messages.iter().rev().find_map(|m| match m {
        Message::Assistant {
            content: Some(MessageContent::Text(t)),
            ..
        } => Some(t.clone()),
        _ => None,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p tt-cli --lib chat::tests 2>&1 | tail -20`
Expected: PASS (all chat tests green).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/chat/mod.rs
git commit -m "feat(chat): prepare_retry + last_assistant_text helpers"
```

---

### Task 3: Factor the turn flow into `do_turn` (behavior-preserving refactor)

**Files:**
- Modify: `crates/cli/src/chat/mod.rs` (add `do_turn`; rewrite the `Command::Chat` arm in `run`)

- [ ] **Step 1: Add `do_turn`**

In `crates/cli/src/chat/mod.rs`, add immediately after the `stream_turn` function (after its closing `}`):

```rust
/// Stream the current conversation: print live, push the assistant reply, and
/// update the ledger. Returns true on success. The caller decides whether to
/// drop the pending user turn on failure.
async fn do_turn(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    conv: &mut Conversation,
    ledger: &mut Ledger,
) -> bool {
    match stream_turn(http, base, key, conv).await {
        Ok((reply, usage)) => {
            conv.push_assistant(reply);
            if let Some(u) = usage {
                ledger.add(&u);
            }
            true
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            false
        }
    }
}
```

- [ ] **Step 2: Rewrite the `Command::Chat` arm to use it**

In `run`, replace the existing `Command::Chat(t) => { … }` arm (the block that pushes the user, calls `stream_turn`, and matches `Ok`/`Err`) with:

```rust
                    Command::Chat(t) if t.is_empty() => {}
                    Command::Chat(t) => {
                        conv.push_user(t);
                        if !do_turn(&http, &base, &key, &mut conv, &mut ledger).await {
                            conv.messages.pop(); // drop the unanswered user turn
                        }
                    }
```

(The `Command::Chat(t) if t.is_empty() => {}` guard arm stays exactly as before — only the second `Chat` arm's body changes.)

- [ ] **Step 3: Build + run existing tests (behavior unchanged)**

Run: `cargo test -p tt-cli --lib chat::tests 2>&1 | tail -20`
Expected: PASS — all existing chat tests still green (pure refactor).

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/chat/mod.rs
git commit -m "refactor(chat): extract do_turn from the Chat arm"
```

---

### Task 4: `/editor`, `/retry`, `/copy` — variants, parse, run arms, editor launch, help

**Files:**
- Modify: `crates/cli/src/chat/mod.rs` (`Command` enum + `parse` + `compose_in_editor` + `run` arms + `print_help`)

> Variants and their `run` arms land together so the `match Command::parse(...)` in `run` stays exhaustive.

- [ ] **Step 1: Write the failing parse test**

Add to the `tests` module:

```rust
    #[test]
    fn command_parse_ergonomics() {
        assert!(matches!(Command::parse("/editor"), Command::Editor));
        assert!(matches!(Command::parse("/e"), Command::Editor));
        assert!(matches!(Command::parse("/retry"), Command::Retry));
        assert!(matches!(Command::parse("/r"), Command::Retry));
        assert!(matches!(Command::parse("/copy"), Command::Copy));
        assert!(matches!(Command::parse("/y"), Command::Copy));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-cli --lib chat::tests::command_parse_ergonomics 2>&1 | tail -20`
Expected: FAIL to compile — `no variant named Editor`/`Retry`/`Copy`.

- [ ] **Step 3: Add the enum variants**

In `crates/cli/src/chat/mod.rs`, in `enum Command`, add three variants after `Cost,`:

```rust
    Editor,
    Retry,
    Copy,
```

- [ ] **Step 4: Add the parse arms**

In `Command::parse`, add these arms before `other => Command::Unknown(other.to_string()),`:

```rust
            "editor" | "e" => Command::Editor,
            "retry" | "r" => Command::Retry,
            "copy" | "y" => Command::Copy,
```

- [ ] **Step 5: Add `compose_in_editor`**

Add after the `do_turn` function (after its closing `}`):

```rust
/// Open `$VISUAL`/`$EDITOR` (fallback `vi`) on a temp file and return the
/// composed text, or `None` when left empty / the editor exits non-zero.
fn compose_in_editor() -> anyhow::Result<Option<String>> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut path = std::env::temp_dir();
    path.push(format!("tt-chat-{}.md", std::process::id()));
    std::fs::write(&path, b"").with_context(|| format!("create {}", path.display()))?;
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launch editor `{editor}`"))?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::remove_file(&path).ok();
    if !status.success() {
        return Ok(None);
    }
    let text = text.trim().to_string();
    Ok(if text.is_empty() { None } else { Some(text) })
}
```

- [ ] **Step 6: Add the `run` arms**

In `run`, add these arms after the `Command::Cost => ui::info(&ledger.summary()),` line:

```rust
                    Command::Editor => match compose_in_editor() {
                        Ok(Some(t)) => {
                            conv.push_user(t);
                            if !do_turn(&http, &base, &key, &mut conv, &mut ledger).await {
                                conv.messages.pop();
                            }
                        }
                        Ok(None) => ui::info("(editor: nothing sent)"),
                        Err(e) => ui::error(&format!("{e:#}")),
                    },
                    Command::Retry => {
                        if prepare_retry(&mut conv) {
                            do_turn(&http, &base, &key, &mut conv, &mut ledger).await;
                        } else {
                            ui::warn("nothing to retry");
                        }
                    }
                    Command::Copy => match last_assistant_text(&conv) {
                        Some(text) => {
                            print!("{}", osc52_copy(&text));
                            use std::io::Write as _;
                            let _ = std::io::stdout().flush();
                            ui::info("(copied last reply to clipboard)");
                        }
                        None => ui::warn("nothing to copy"),
                    },
```

- [ ] **Step 7: Update `print_help`**

In `print_help`, insert three rows into the array after the `("/system [s]", "show or set the system prompt"),` line:

```rust
        ("/editor", "compose a multi-line message in $EDITOR"),
        ("/retry", "re-run the last turn"),
        ("/copy", "copy the last reply to the clipboard"),
```

- [ ] **Step 8: Run the parse test + full lib tests**

Run: `cargo test -p tt-cli --lib chat 2>&1 | tail -25`
Expected: PASS — `command_parse_ergonomics` and all other chat tests green.

- [ ] **Step 9: Commit**

```bash
git add crates/cli/src/chat/mod.rs
git commit -m "feat(chat): /editor, /retry, /copy commands"
```

---

### Task 5: Gates + smoke test + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt -p tt-cli && git diff --quiet || git commit -am "style: cargo fmt"`
Expected: no changes, or a fmt commit.

- [ ] **Step 2: Clippy (whole workspace, all targets — catches literal/exhaustiveness drift)**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -25`
Expected: no warnings.

- [ ] **Step 3: Full test run**

Run: `cargo test -p tt-cli 2>&1 | tail -25`
Expected: all pass.

- [ ] **Step 4: cargo-deny (base64 is already in the tree; confirm no new advisory)**

Run: `cargo deny check advisories 2>&1 | tail -25`
Expected: ok (no new advisory; if base64 trips one, add its RUSTSEC id to `deny.toml [advisories].ignore` with a justification — following the existing `number_prefix` precedent).

- [ ] **Step 5: Smoke test (piped REPL, no network)**

Run:
```bash
TT_API_KEY=test printf '/copy\n/retry\n/help\n/exit\n' | cargo run -q -p tt-cli -- chat 2>&1 | tail -30
```
Expected: `/copy` → "nothing to copy" (warn); `/retry` → "nothing to retry" (warn); `/help` lists `/editor`, `/retry`, `/copy`; clean exit. (None of these hit the network.)

- [ ] **Step 6: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR (`base64` dep + the three commands).

---

## Self-Review

- **Spec coverage:** `/editor` (Task 4 + `compose_in_editor`), `/retry` (Task 4 + `prepare_retry` Task 2), `/copy` (Task 4 + `osc52_copy` Task 1 + `last_assistant_text` Task 2), `do_turn` refactor (Task 3), help/parse (Task 4), base64 dep (Task 1), no-markdown/raw-streaming (untouched). All spec items covered.
- **Placeholders:** none — every code step shows complete code.
- **Type consistency:** `do_turn(&reqwest::Client, &str, &str, &mut Conversation, &mut Ledger) -> bool` used identically in the Chat/Editor/Retry arms; `prepare_retry(&mut Conversation) -> bool`, `last_assistant_text(&Conversation) -> Option<String>`, `osc52_copy(&str) -> String`, `compose_in_editor() -> Result<Option<String>>` consistent throughout. `Message`/`MessageContent` already imported at the top of the file.
- **Exhaustiveness:** the new `Command` variants (Task 4 Step 3) and their `run` arms (Step 6) land in the same task, so the `match` never goes non-exhaustive at a commit boundary.
