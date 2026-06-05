# V5a-2 — `tt chat` Sessions + Cost Ledger Design

**Status:** approved (design — completes the V5 "fuller" scope)
**Date:** 2026-06-05
**Slice:** V5a-2 (second V5a sub-slice). Finishes `tt chat`.
**Depends on:** V5a-1 (#25) merged.

## Goal

Persist conversations to disk (save / resume / list) and show a running cost+savings ledger for the session.

## Design

### Persistence (`crates/cli/src/chat/session.rs`)
- `Conversation` gains `#[derive(Serialize, Deserialize)]` (it's `model: String`, `system: Option<String>`, `messages: Vec<Message>` — `Message` already serde). Saved as JSON; no extra fields (the file's mtime gives "updated" for listing).
- `sessions_dir() -> PathBuf` = `context::store::config_dir().join("sessions")` (`~/.tokentrimmer/sessions/`).
- **`sanitize_name(name: &str) -> String`** (security-critical, tested): lowercase; keep only `[a-z0-9._-]`; collapse/replace others with `-`; strip leading dots; reject empty → `"chat"`. **Guarantees no path separators / `..`** so `/resume ../../etc/foo` can't traverse out of `sessions_dir`. All save/load/path-build go through it.
- **`auto_name(conv: &Conversation) -> String`**: slug of the first user message (first ~5 words, sanitized), else `"chat"`.
- `save(dir, name, conv) -> Result<PathBuf>`: `create_dir_all(dir)`; write `serde_json::to_string_pretty` to `dir.join(format!("{}.json", sanitize_name(name)))`.
- `load(dir, name) -> Result<Conversation>`: read + parse `dir.join("{sanitized}.json")`; clear error when absent.
- `list(dir) -> Result<Vec<SessionMeta>>`: read `dir`, for each `*.json` → `SessionMeta { name (file stem), model, turns (messages.len()/2 rounded), modified (mtime) }`, sorted by modified desc.

### Ledger (in `chat/mod.rs`, tested)
```rust
#[derive(Default)]
pub struct Ledger { pub turns: u32, pub cost_usd: f64, pub saved_usd: f64, pub baseline_usd: f64 }
impl Ledger {
    pub fn add(&mut self, u: &UsageInfo) { … accumulate … }
    pub fn summary(&self) -> String { format!("session: {} turn(s) · ${:.4} spent · saved ${:.4} ({:.0}%)", …) }
}
```
`stream_turn` returns `(String, Option<UsageInfo>)`; `run` calls `ledger.add(&u)` per turn.

### Commands (added to `Command` + `run`)
- `/save [name]` — `session::save(sessions_dir(), name.unwrap_or_else(|| auto_name(&conv)), &conv)`; `ui::ok("saved → {path}")` (stderr) — actually `ui::success` (stdout) per the chat voice. Confirm.
- `/resume <name>` (also `/load`) — `conv = session::load(…, name)?`; `ui::info("(resumed {name} · {n} messages)")`. Error → `ui::error`, keep current.
- `/sessions` — `session::list(…)` → a `ui::table(["NAME","MODEL","TURNS","UPDATED"], colors_enabled)`; empty → `ui::info("no saved sessions")`.
- `/cost` — `ui::info(&ledger.summary())`.
- Help text updated to include the new commands.

### Flag
- `tt chat --resume <name>` — load that session on start (before the REPL). Added to the `Chat` clap command + `run` signature.

## Testing
- **`sanitize_name`**: `"My Chat!"`→`"my-chat"`; `"../../etc/passwd"`→ no `/` or `..` (e.g. `"etc-passwd"` / safe slug); `""`→`"chat"`; a normal `"rust-help"` unchanged.
- **`auto_name`**: first user msg "Explain the borrow checker" → `"explain-the-borrow-checker"` (≤5 words); empty conv → `"chat"`.
- **save/load round-trip** (tempdir): save a Conversation, load it, assert model/system/messages equal.
- **`list`** (tempdir): two saved sessions → two `SessionMeta` with right names/turns.
- **`Ledger`**: `add` two `UsageInfo` → totals + `summary()` substring (color disabled).
- **path-traversal**: `load(dir, "../../etc/passwd")` resolves inside `dir` (the sanitized path stays under `dir`) — assert the built path's parent == dir.
- `cargo clippy --workspace --all-targets -D warnings`; `cargo fmt`; smoke (`/save`, `/sessions`, `/resume`, `/cost`).

## Out of Scope
- Multiline input, autocomplete, streaming tool calls, dashboard playground — later.
- Encrypting saved sessions (they're plaintext JSON in `~/.tokentrimmer/`, same trust level as the stored credentials).
