# V5b-3 — `tt chat` Context Management Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V5b-3 (third and final V5b sub-slice; V5b-1 ergonomics #27, V5b-2 tool-calling #28 merged).
**Depends on:** V5a + V5b-1 + V5b-2 merged.

## Goal

Keep long `tt chat` sessions from silently blowing the model's context window: track estimated token usage, warn as it fills, and auto-trim the oldest turns before the limit — with a `/context` view and a manual `/trim`.

## Decisions (confirmed)

- **Behavior:** warn once at **75%** of budget; **auto-trim** the oldest whole turns once a turn would exceed **95%** (trim down to ~70% for headroom), with a clear notice; plus a manual `/trim`.
- **Budget:** a small built-in **model→window table** (prefix-keyed, e.g. gpt-4o ~128k, claude ~200k, o-series ~200k) with a **128k fallback**, overridable by **`--max-context`** and a **`/context <n>`** setter. (Exact live windows are V4's job; this is a best-effort default.)

## Architecture

Token sizing reuses `tt_tokenize::estimate_tokens` (cl100k + heuristic). The gateway already returns authoritative `input_tokens` per turn, but proactive management needs an estimate *before* sending, so estimation drives the logic. All context logic lives in a new submodule `crates/cli/src/chat/budget.rs`; `mod.rs` adds `/context` + `/trim`, a `ContextState`, and calls `ContextState::manage` at the top of `dispatch_turn`.

### `crates/cli/src/chat/budget.rs`

- `const DEFAULT_CONTEXT_BUDGET: u32 = 128_000;`
- `const WARN_FRAC: f64 = 0.75; const TRIM_FRAC: f64 = 0.95; const TRIM_TARGET_FRAC: f64 = 0.70;`
- `const ESTIMATE_PROVIDER: &str = "openai";` — cl100k is a high-quality general estimator for all major models; the chat doesn't reliably know the routed provider.
- **`model_window(model: &str) -> u32`** (pure, tested): lowercase-prefix match → window. Table (best-effort, documented as approximate):
  - `gpt-5*` → 256_000 · `gpt-4o*` / `gpt-4.1*` / `gpt-4-turbo*` → 128_000 · `o1*`/`o3*`/`o4*` → 200_000 · `claude*` → 200_000 · `gemini*` → 1_000_000 · else → `DEFAULT_CONTEXT_BUDGET`.
- **`conversation_text(conv: &Conversation) -> String`** (private): concatenates the system prompt + each message's `MessageContent::Text` (Parts/None skipped — the chat only emits Text).
- **`estimate_conversation_tokens(conv, provider) -> u32`** (tested): `tt_tokenize::estimate_tokens(provider, &conversation_text(conv))`.
- **`trim_to_budget(conv: &mut Conversation, target: u32, provider) -> usize`** (tested): while `estimate > target` and `messages.len() > 1`, `remove(0)`; then while the front message is `Assistant`/`Tool`, keep removing (so the kept window starts at a `User` — never orphans an `Assistant{tool_calls}` + `Tool` group). The system prompt lives in `conv.system`, so it is always preserved. Returns messages removed.
- **`ContextState`**:
  ```rust
  pub struct ContextState { pub override_budget: Option<u32>, warned: bool }
  impl ContextState {
      pub fn new(override_budget: Option<u32>) -> Self { … warned: false }
      pub fn budget(&self, model: &str) -> u32 { self.override_budget.unwrap_or_else(|| model_window(model)) }
      /// Warn at 75%, auto-trim at 95%. Call after the user turn is pushed,
      /// before sending. Prints via ui; mutates conv on trim.
      pub fn manage(&mut self, conv: &mut Conversation) { … }
      pub fn estimate(&self, conv: &Conversation) -> u32 { estimate_conversation_tokens(conv, ESTIMATE_PROVIDER) }
  }
  ```
  `manage`: `let budget = self.budget(&conv.model); let est = self.estimate(conv);`
  - `est > budget*TRIM_FRAC` → `trim_to_budget(conv, (budget*TRIM_TARGET_FRAC) as u32, …)`; if dropped > 0 → `ui::note("(context ~{est}/{budget} tok — trimmed {dropped} old message(s))")`; `warned = false`.
  - else `est > budget*WARN_FRAC` and `!warned` → `ui::warn("context ~{est}/{budget} tok ({pct}%) — oldest turns auto-trim near the limit")`; `warned = true`.
  - else → `warned = false` (re-arms after dropping below 75%, e.g. via `/clear`/`/trim`).

### `crates/cli/src/chat/mod.rs`

- `pub mod budget;`
- `Command` gains `Context(Option<u32>)` and `Trim`; `parse`: `"context"` → `Context(arg.and_then(|a| a.parse().ok()))`, `"trim"` → `Trim`.
- `dispatch_turn` takes `ctx: &mut budget::ContextState` and calls `ctx.manage(conv)` first (covers Chat/Editor/Retry, tool + streamed paths). This pushes `dispatch_turn` to 8 params → add `#[allow(clippy::too_many_arguments)]` (a session-bundle refactor is out of scope).
- `run`:
  - signature gains `max_context: Option<u32>` (after `tools`).
  - `let mut ctx = budget::ContextState::new(max_context);`
  - thread `&mut ctx` into the three `dispatch_turn` calls.
  - `/context` arm: `Context(Some(n))` → `ctx.override_budget = Some(n)`, `ui::info("context budget → {n} tokens")`; `Context(None)` → show `ui::info("context: ~{est} / {budget} tokens ({pct}%)  [{model}]")`.
  - `/trim` arm: `let dropped = budget::trim_to_budget(&mut conv, (ctx.budget(&conv.model) as f64 * TRIM_TARGET_FRAC) as u32, ESTIMATE_PROVIDER); ui::info("trimmed {dropped} old message(s)")`. (Expose `TRIM_TARGET_FRAC`/`ESTIMATE_PROVIDER` as `pub` from `budget`, or a `budget::manual_trim(&mut conv, &ctx)` helper — prefer the helper.)
- `print_help`: add `/context [n]` and `/trim` rows.

### `crates/cli/src/main.rs`
- `Chat` gains `--max-context <u32>` (`Option<u32>`); threaded into `chat::run(..., max_context, …)`.

## Cargo
- Add `tt-tokenize.workspace = true` to `crates/cli/Cargo.toml` (`[dependencies]`). No other new deps.

## Testing
- **`model_window`**: prefixes → expected windows; unknown → 128_000; case-insensitive.
- **`estimate_conversation_tokens`**: empty conv → small/0; adding messages strictly increases the estimate; non-zero for real text.
- **`trim_to_budget`**: a multi-turn conv with a tiny target → fewer messages, estimate ≤ target (or floor of 1), the system prompt (in `conv.system`) intact, the last message preserved.
- **trim coherence with tools**: `[User, Assistant{tool_calls}, Tool, Assistant, User, Assistant]` + tiny target → the kept window's first message is a `User` (never `Tool`/`Assistant{tool_calls}`).
- **`ContextState::budget`**: `override_budget=Some(n)` → n regardless of model; `None` → `model_window(model)`.
- **`ContextState::manage`** (color disabled): a conv pushed over 95% of a small override budget → trims (messages drop, no panic); between 75–95% → `warned` set true and not re-warned on a second call.
- `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo deny`; smoke (`/context`, `/context 50000`, `/trim`, `/help`, `/exit` — piped, no network).

## Out of Scope (later)
- Displaying the gateway's *actual* last-turn `input_tokens` in `/context` (would require threading `UsageInfo` back out of `do_turn`/`run_tool_turn`) — estimate-based for now.
- Counting the advertised `tools` array / tool-call argument tokens in the estimate (minor undercount when `/tools` is on).
- Summarizing trimmed history instead of dropping it; per-model live windows (V4 live catalog).

## Risk
The estimate uses cl100k for all models (high-quality but not exact for non-OpenAI tokenizers) and ignores tool/schema tokens, so it can mildly under/over-count; the gateway remains the authoritative gate. Budgets from the static table are approximate and user-overridable. Both are acceptable for an advisory trim that errs toward keeping the session under the limit.
