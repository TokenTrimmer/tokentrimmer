# V5b-1 — `tt chat` Ergonomics Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V5b-1 (first of three V5b sub-slices; V5b-2 = agentic tool-calling, V5b-3 = context management).
**Depends on:** V5a (#25/#26) merged.

## Goal

Make `tt chat` pleasant for real use: multi-line composition, re-running a turn, and copying the last reply. Keep the current raw live streaming (no markdown rendering).

## Design

All additions live in `crates/cli/src/chat/mod.rs`. New `Command` variants + small tested helpers; the turn logic is factored into a shared `do_turn`.

### Shared turn helper (refactor)
```rust
/// Stream the current conversation, push the assistant reply, update the ledger.
/// Returns true on success. The caller decides whether to drop the user turn on failure.
async fn do_turn(http, base, key, conv: &mut Conversation, ledger: &mut Ledger) -> bool {
    match stream_turn(http, base, key, conv).await {
        Ok((reply, usage)) => { conv.push_assistant(reply); if let Some(u) = usage { ledger.add(&u); } true }
        Err(e) => { ui::error(&format!("{e:#}")); false }
    }
}
```
The normal `Chat(t)` arm becomes: `conv.push_user(t); if !do_turn(…).await { conv.messages.pop(); }`.

### `/editor` — multi-line input
- `compose_in_editor() -> Result<Option<String>>`: resolve `$VISUAL` → `$EDITOR` → `vi`; write an empty temp file (`std::env::temp_dir()/tt-chat-<pid>.md`); spawn the editor on it (`Command::status()`); read it back; delete it; `trim()`. `None` when empty (cancel).
- The `/editor` command runs the composed text as a normal turn (`push_user` + `do_turn`). Integration-only (spawns a process) — smoke-tested.

### `/retry` — re-run the last turn
- `prepare_retry(conv: &mut Conversation) -> bool` (pure, tested): if the last message is an `Assistant`, pop it; return `true` iff the last message is now a `User` (something to retry). 
- `/retry`: `if prepare_retry(&mut conv) { do_turn(…).await; }` (do **not** pop the user on failure — keep it so the user can retry again). When nothing to retry → `ui::warn("nothing to retry")`.

### `/copy` — last reply → clipboard (OSC52)
- `last_assistant_text(conv) -> Option<String>` (pure, tested): the most recent assistant message text.
- `osc52_copy(text) -> String` (pure, tested): `format!("\x1b]52;c;{}\x07", base64_standard(text))` — the terminal OSC52 clipboard escape (works locally **and over SSH**, no platform clipboard dep).
- `/copy`: emit the escape to stdout + `ui::info("(copied last reply to clipboard)")`. No reply → `ui::warn("nothing to copy")`.

### Commands + help
- `Command` gains `Editor`, `Retry`, `Copy`; `parse` maps `editor`/`e`, `retry`/`r`, `copy`/`y`.
- `print_help` lists `/editor`, `/retry`, `/copy`.

## Dependencies
- `base64` for OSC52 (already in the tree transitively; add `base64 = "0.22"` to `crates/cli/Cargo.toml` — confirm/`cargo deny`).

## Testing
- `Command::parse`: `/editor`→Editor, `/retry`→Retry, `/copy`→Copy.
- `prepare_retry`: `[user, assistant]` → pops assistant, returns true; `[user]` → true (no assistant to pop); `[]` → false; `[user, assistant, user(empty? n/a)]`.
- `last_assistant_text`: returns the latest assistant text; `None` when none.
- `osc52_copy("hi")` contains the base64 of `"hi"` (`aGk=`) and the `\x1b]52;c;` / `\x07` framing.
- Smoke: `/copy` with no reply → warn; `/retry` with no turn → warn (piped REPL, no network). `/editor` is manual (`$EDITOR`).
- `cargo clippy --workspace --all-targets -D warnings`; `cargo fmt`; `cargo deny`.

## Out of Scope (later V5b)
- Markdown / code-block rendering (chosen: keep raw streaming).
- Agentic tool-calling — **V5b-2**. Context-window management — **V5b-3**.
- `"""` inline heredoc multiline (`/editor` covers multi-line); `/edit` (edit a prior message); clipboard *paste*.
