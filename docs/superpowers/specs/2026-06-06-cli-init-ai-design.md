# `tt init --ai` — AI-tailored init artifacts (F11) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** F11. Adds an opt-in AI pass to `tt init` that tailors the artifacts init already writes, grounded in a repo scan.

## Goal

`tt init --ai` runs the normal deterministic init, then makes **one grounded model call** (reusing `advise`'s repo scan) and tailors two existing artifacts:
- `.claude/budget.toml` — the daily/weekly cost caps.
- `AGENTS.md` — a marker-delimited section with detected models + recommended cheaper-model routes + notes.

AI is additive and opt-in; the default `tt init` is unchanged. There is intentionally **no new enforced-routing config** (no `/public` consumer exists — that's cloud/F12).

## Background (current state)

- `init::run(opts: RunOptions) -> Result<RunReport, InitError>` (`init/mod.rs:60`) is **sync**, deterministic. It writes `<root>/AGENTS.md`, `<root>/.claude/budget.toml` (`daily_cap_usd = 10` / `weekly_cap_usd = 50`, with comments), hooks, workflows; idempotent via `.tt-init.lock`. `RunOptions` has no AI flag.
- `Init` clap command + dispatch (`main.rs:192`, `:570`) is sync (`run(opts)?`); `main()` is async (so the arm can `.await`).
- `advise` (`advise.rs`) exposes `pub fn detect_models(&Path) -> Vec<ModelUsage>`, `pub struct ModelUsage { id, count, example_file }`, `pub fn build_context_message(...)`. The cost-cap hook (`.claude/hooks/cost-cap-check.sh`) reads `budget.toml` with bash, so caps must stay whole-number integers.
- `tt_client::Client` + `.chat().…​.send()` (one-shot, returns `ChatOutcome` with `.text()`) is the non-interactive call. `ResolvedContext::load(flag_key, flag_base)` resolves key/base.

## Architecture

New file `crates/cli/src/init/ai.rs` (declared `pub mod ai;` in `init/mod.rs`, re-export `ai_tailor`). Pure helpers are unit-tested; the one async fn orchestrates.

### Command surface (`main.rs`)
Add to `Init`:
```rust
        /// Tailor the generated config with an AI pass over the repo (needs an API key).
        #[arg(long)]
        ai: bool,
        /// Model for the --ai pass (default: gpt-4o-mini).
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
```
Dispatch (after the existing `run(opts)?`):
```rust
    run(opts).context("tt init failed")?;
    if ai {
        tt_cli::init::ai_tailor(&root, model, tt_api_key, tt_api_base)
            .await
            .context("tt init --ai pass failed")?;
    }
```
(`root` is the resolved init root; reuse the same `PathBuf`.)

### `ai_tailor` (async)
```rust
pub async fn ai_tailor(
    root: &Path,
    model: Option<String>,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()>
```
1. `ResolvedContext::load(flag_key, flag_base)?` → `api_key_string().context("no API key — run `tt login` or set TT_API_KEY")?` → `tt_client::Client::new(base, key)`.
2. `let detected = crate::advise::detect_models(root);` (read-only scan).
3. One-shot call:
   ```rust
   let out = client.chat()
       .model(model.unwrap_or_else(|| "gpt-4o-mini".to_string()))
       .message(tt_client::system(INIT_AI_SYSTEM))
       .message(tt_client::user(build_init_context(&detected)))
       .send().await?;
   ```
   `INIT_AI_SYSTEM` instructs **JSON-only** output of:
   ```jsonc
   { "daily_cap_usd": <number>, "weekly_cap_usd": <number>,
     "routes": [{"from":"…","to":"…","reason":"…"}], "notes": "…" }
   ```
4. `let Some(cfg) = parse_ai_config(out.text().unwrap_or_default()) else { ui::warn("AI pass: could not parse model output as JSON — skipped (deterministic init is intact)"); return Ok(()); };`
5. **Tailor `.claude/budget.toml`** (`<root>/.claude/budget.toml`): if it exists, `let updated = apply_budget_caps(&content, &cfg);` write it back. (`apply_budget_caps` sets the `daily_cap_usd`/`weekly_cap_usd` lines to the rounded-integer AI values when present; preserves comments + any other lines.)
6. **Tailor `AGENTS.md`** (`<root>/AGENTS.md`): if it exists, `let updated = upsert_marked_section(&content, &render_ai_section(&detected, &cfg));` write it back.
7. `ui` summary (caps set, N routes recommended).

### Pure helpers (the unit-tested surface)
```rust
struct AiConfig {
    daily_cap_usd: Option<f64>,
    weekly_cap_usd: Option<f64>,
    routes: Vec<AiRoute>,   // #[serde(default)]
    notes: Option<String>,  // #[serde(default)]
}
struct AiRoute { from: String, to: String, reason: Option<String> } // reason #[serde(default)]

/// Strip ```json fences / whitespace, then serde-parse. None on any failure.
fn parse_ai_config(text: &str) -> Option<AiConfig>;

/// Replace `daily_cap_usd`/`weekly_cap_usd` value lines with the rounded-integer
/// AI values (only those present); preserve every other line + comments. Caps are
/// written as whole-dollar integers so the bash cost-cap hook keeps parsing them.
fn apply_budget_caps(content: &str, cfg: &AiConfig) -> String;

/// One-line-per-model + routes + notes, wrapped in the AI markers.
fn render_ai_section(detected: &[ModelUsage], cfg: &AiConfig) -> String;

/// Insert (append) or, when the markers already exist, replace the block between
/// `<!-- tt:ai:start -->` and `<!-- tt:ai:end -->`. Idempotent across re-runs.
fn upsert_marked_section(content: &str, section: &str) -> String;
```

## Behavior / safety
- AI runs only under `--ai` (opt-in). Default `tt init` byte-for-byte unchanged.
- Writes are **surgical** — cap value-lines and a marker-delimited AGENTS section — so they don't clobber user content even if those files were user-modified.
- Caps are **advisory defaults** to review (whole-dollar integers).
- Graceful degradation: missing API key → clear error (the `--ai` opt-in implies the caller wants the pass); unparseable model output / missing artifact / empty scan → `ui::warn` + skip the AI writes (deterministic init already succeeded). Re-running `tt init --ai` is idempotent (marker replace; cap value re-set).

## Testing

Unit (`init/ai.rs` `#[cfg(test)]`):
- `parse_ai_config`: plain JSON; ```json-fenced; malformed → None; missing optional fields default.
- `apply_budget_caps`: replaces `daily_cap_usd = 10` → `= 25`, leaves comments + `weekly_cap_usd` untouched when not provided; both when provided; rounds `24.7` → `25`.
- `upsert_marked_section`: appends when no markers; **replaces** (no duplication) when markers present.
- `render_ai_section`: contains each detected model id, each route `from→to`, the notes, and is wrapped in the markers.

Integration (`crates/cli/tests/init_ai_smoke.rs`, mirroring `init_smoke.rs` + tt-client httpmock):
- temp git dir + a `Cargo.toml` and a source file referencing `gpt-4o`; `run(opts)` (skip_baseline) writes the deterministic artifacts.
- `httpmock` gateway whose `/v1/chat/completions` returns a `ChatCompletionResponse` whose `choices[0].message.content` is the JSON config.
- `ai_tailor(dir, None, Some("k"), Some(server.base_url())).await.unwrap();`
- Assert `.claude/budget.toml` now has the AI caps; `AGENTS.md` contains the `tt:ai` section with the recommended route. Run `ai_tailor` again → AGENTS.md has exactly one AI section (idempotent).

Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p tt-cli`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-cli --no-deps` (note: tt-cli has pre-existing crate-wide `<…>`-as-HTML doc warnings, not a CI gate).

## Out of scope
- Any *enforced* routing/policy config file (no `/public` consumer — cloud/F12).
- An interactive review/confirm step (writes are surgical + opt-in, so applied directly).
- `tt audit`.
- Tailoring artifacts beyond `budget.toml` + `AGENTS.md`.
