# V5a-1 — `tt chat` Core Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V5a-1 (first of two V5a sub-slices; V5a-2 = sessions + cost ledger).
**Part of:** V5 — interactive `tt chat` CLI that routes through the TokenTrimmer gateway, surfacing per-turn cost + savings.

## Goal

An interactive terminal chat REPL: type a message → the streamed assistant reply renders live → a per-turn footer shows the model used, tokens, cost, and **savings** → loop, keeping multi-turn history. Talks to the hosted gateway's OpenAI-compatible `/v1/chat/completions` (reusing V0 credential resolution), so TokenTrimmer's routing/caching apply and the savings are real.

## Architecture

New module `crates/cli/src/chat/` (lib): `mod.rs` (REPL + commands), `stream.rs` (SSE client + parser), `conversation.rs` (in-memory state).

### Conversation (in-memory; persistence is V5a-2)
```rust
pub struct Conversation {
    pub model: String,                 // requested model (gateway may route it)
    pub system: Option<String>,
    pub messages: Vec<Message>,        // tt_shared::Message (user/assistant/system)
}
```
On each turn: push the user `Message`, send the full `messages` (with the system message prepended when set), stream the reply, push the assistant `Message`.

### Request + streaming
- Build `ChatCompletionRequest { model, messages, stream: true, .. }`; `POST {base}/v1/chat/completions` with `Authorization: Bearer {key}` (`ResolvedContext::load`). Requires `reqwest` `stream` feature.
- Read the response: `x-tokentrimmer-model-used` header → the served model for the footer. Non-2xx → read the error body, `ui::error`, abort the turn (keep the REPL alive — pop the just-added user message so history stays consistent).
- Consume `response.bytes_stream()` (futures `StreamExt`), accumulating into an SSE frame buffer (split on `\n\n`). For each frame parse the `event:` + `data:` lines into a `StreamEvent` (pure, tested):
  ```rust
  pub enum StreamEvent {
      Delta(String),        // data: {chunk} → choices[0].delta.content
      Usage(UsageInfo),     // event: tokentrimmer.usage → {cost_usd, baseline_cost_usd, saved_usd, input_tokens, output_tokens, cached_tokens}
      Done,                 // data: [DONE]
      Ignore,               // role-only delta, keep-alive, unknown
  }
  pub fn parse_sse_frame(frame: &str) -> StreamEvent { … }
  ```
- Render: a `ui::spinner("…")` until the first `Delta`, then clear it and write deltas to stdout as they arrive (flushing). After `Done`, print the footer from the last `Usage` + the served-model header.

### Per-turn footer (the differentiator)
Pure formatter (tested), printed muted to stdout:
```
· {served_model} · {in+out} tok · ${cost:.4} · saved {pct:.0}%
```
`pct = saved_usd / baseline_cost_usd * 100` (0 when baseline is 0). `saved …%` omitted when `saved_usd ≈ 0`.

### REPL + commands
- `rustyline::Editor` for input (↑/↓ history, line editing, Ctrl-C cancels the line, Ctrl-D exits). Prompt: `ui::accent("› ")`.
- Lines starting with `/` are commands (parsed by a pure `Command::parse(line) -> Command`):
  - `/help` — list commands
  - `/clear` — reset `messages` (keep model/system)
  - `/model [M]` — print or switch the requested model
  - `/system [S]` — print or set the system prompt
  - `/exit` — quit
- Empty line → ignored. Non-command → a chat turn.
- Banner on start: `tt chat · {model} via TokenTrimmer   (/help)`.
- Flags: `tt chat [--model <M>] [--system <S>]`. Default model `gpt-4o-mini` (a `DEFAULT_CHAT_MODEL` const).

### Preconditions / errors
- No API key → `ui::error("run `tt login` first…")`, exit. (Reuse `ResolvedContext`.)
- Gateway auth/credential/upstream errors → `ui::error` with the gateway's message; REPL continues.

## Dependencies (add to `crates/cli/Cargo.toml`)
- `rustyline` (REPL input) — **run cargo-deny; add any unmaintained-transitive advisory to `deny.toml` ignore** (as done for `number_prefix`).
- `futures` (StreamExt over `bytes_stream`).
- `reqwest` `stream` feature (add to tt-cli's reqwest dep / confirm the workspace dep has it).

## Testing
- **`parse_sse_frame`**: a content `data:` chunk → `Delta("…")`; the `tokentrimmer.usage` event → `Usage{…}`; `data: [DONE]` → `Done`; role-only delta / blank → `Ignore`.
- **`Command::parse`**: `/help`→Help, `/model gpt-4o`→Model(Some), `/model`→Model(None), `/clear`/`/exit`, non-slash → Chat, `/bogus` → Unknown.
- **`format_turn_footer`**: exact string for given (model, tokens, cost, saved) with color disabled; `saved 0%` omitted.
- **Conversation**: push user/assistant; `/clear` empties messages; system prepended when set.
- The interactive REPL loop itself is not unit-tested (stdin/streaming); the pure pieces above are. A manual smoke against a live/dogfood gateway (or `tt_test_*` sandbox key, which returns a deterministic synthetic response) confirms end-to-end.
- `cargo clippy --workspace --all-targets -D warnings`; `cargo fmt`.

## Out of Scope (V5a-2 and beyond)
- **Sessions** (`/save`/`/resume`/`/sessions`, `--resume`, on-disk JSON) + **`/cost`** session ledger — V5a-2.
- Multiline input, tool/function calling, file/context attachment, slash-command autocomplete, the dashboard playground, MCP chat — later.
