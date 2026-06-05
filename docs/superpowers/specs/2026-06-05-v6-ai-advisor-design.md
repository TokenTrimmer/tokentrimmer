# V6 — AI Cost/Routing Advisor (`tt advise`) Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V6 (first AI slice). Composes the V5b-2 tool-calling loop into a one-shot advisor.
**Depends on:** V5b-2 (`chat::tools::{build_registry, run_tool_turn}`) + V5a (`Conversation`/`Ledger`) — merged.

## Goal

`tt advise` — point it at a repo and it prints a grounded cost/routing recommendation. It scans for model usage, then asks a model (with TokenTrimmer's own tools available) to recommend cheaper routes, flag risky prompts, and suggest config. **Read-only** (no file writes). On-brand: "point `tt` at your code, it tells you how to save money."

## Why this shape

The model-call + tool infra already exists (V5b-2: `find_route_for`/`preview_cost`/`inspect_diff` behind `run_tool_turn`). V6 reuses it one-shot: seed a `Conversation` (advisor system prompt + scanned context), run the tool loop once, print. The model **grounds** its advice by calling `preview_cost`/`find_route_for` itself — not hand-waving. No new model-calling code.

## Architecture

New crate-level module `crates/cli/src/advise.rs`.

### `crates/cli/src/advise.rs`
- **`scan_text_for_models(text: &str) -> Vec<String>`** (pure, tested): extract known model-id mentions via one regex — `gpt-[\w.\-]+`, `claude-[\w.\-]+`, `gemini-[\w.\-]+`, `o[1-9]-[\w.\-]+`, `mistral[\w.\-]*`, `llama[\w.\-]*`, `text-embedding-[\w.\-]+` (case-insensitive). De-duplicated, preserving first-seen order.
- **`struct ModelUsage { id: String, count: usize, example_file: String }`**.
- **`detect_models(root: &Path) -> Vec<ModelUsage>`** (tested over a tempdir): `walkdir` the repo, skip `.git`/`node_modules`/`target`/`dist`/`build`/`.venv`, read source files (by extension: `py js ts tsx jsx rs go rb java kt php cs`), run `scan_text_for_models`, aggregate id → (count, first file). Sorted by count desc. Caps total bytes/files scanned so a huge repo can't hang.
- **`const ADVISOR_SYSTEM: &str`** — the persona: "You are a TokenTrimmer cost-optimization advisor. Use the provided tools (preview_cost, find_route_for, inspect_diff) to ground every recommendation in real numbers. Be concrete and brief: list concrete routing/cost changes with the $ impact, and flag risky prompts. Do not invent prices — call the tools."
- **`build_context_message(detected: &[ModelUsage], describe: Option<&str>) -> String`** (pure, tested): a compact brief — the detected models + counts + example files, and the optional `--describe` text; ends asking for grounded recommendations. When `detected` is empty, says so and leans on `--describe`.
- **`pub async fn run(path, describe, model, flag_key, flag_base) -> Result<()>`**:
  - `ResolvedContext::load` → key (bail if none, like `tt chat`) + base; `reqwest::Client`.
  - `let detected = detect_models(path_or_cwd);` print a one-line `ui::note` of what was found.
  - `let mut conv = Conversation::new(model.unwrap_or(DEFAULT_CHAT_MODEL), Some(ADVISOR_SYSTEM));` `conv.push_user(build_context_message(&detected, describe.as_deref()));`
  - `let reg = chat::tools::build_registry(); let mut ledger = Ledger::default();`
  - `chat::tools::run_tool_turn(&http, &base, &key, &mut conv, &reg, &mut ledger).await;` (prints the grounded report + the per-turn cost footer).

### `crates/cli/src/lib.rs` + `main.rs`
- `pub mod advise;`
- New command `Advise { path: Option<String>, describe: Option<String>, model: Option<String>, tt_api_key, tt_api_base }` → `tt_cli::advise::run(path, describe, model, tt_api_key, tt_api_base).await`.
- Make `DEFAULT_CHAT_MODEL` / `Conversation`/`Ledger` reachable: they're `pub` in `chat` (`chat::DEFAULT_CHAT_MODEL` is private — expose it or use a local default const in `advise`). Use a local `const ADVISOR_MODEL: &str = "gpt-4o-mini"` if `chat::DEFAULT_CHAT_MODEL` isn't `pub`.

## Testing
- **`scan_text_for_models`**: a snippet with `"gpt-4o-mini"`, `model="claude-3-5-sonnet"`, `o3`, and a non-model word → exactly the model ids, de-duped; empty text → `[]`.
- **`detect_models`** (tempdir): write two files referencing `gpt-4o`/`claude-…`; a `node_modules/…` file that must be skipped; assert the aggregated ids/counts/example_file and that the skipped dir contributed nothing.
- **`build_context_message`**: includes each detected id + count + the `--describe` text; the empty-detected case mentions "no model usage detected".
- **Integration (httpmock)**: seed a conversation and run `run_tool_turn` against a mock gateway that returns a final answer (reuse the V5b-2 mock pattern) — confirms the advisor wiring drives the tool loop and prints. (Optional, if cheap; otherwise a smoke.)
- **Smoke**: `tt advise --help`; `tt advise` with no key → the same "no API key" bail as `tt chat`; a dry `detect_models` over this repo prints found models. The full model call is a manual check with a real key.
- `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo deny`; `cargo test -p tt-cli`.

## Out of Scope (later V6)
- Writing/applying any config (AI-assisted `tt init` that generates a tailored `.tokentrimmer` — a later slice; this one is read-only).
- Pulling live telemetry/plan data as context (this slice grounds via the tools + a code scan; telemetry context is a later enhancement).
- Streaming the advisor output (uses the non-streamed tool loop, like `/tools` in `tt chat`).
- Auto-applying suggested routes (`route add`/the dashboard remain the apply path).
