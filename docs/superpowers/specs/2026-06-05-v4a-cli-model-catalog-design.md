# V4a — CLI Consumes the Live Model Catalog Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V4a (first of the V4 "live model catalog" area; V4b = server-side metadata consolidation, V4c = remote refresh).
**Depends on:** V5b-3 (#29) merged — replaces its prefix `model_window` table as the primary window source (keeps it as the offline fallback).

## Goal

Let the CLI consume the gateway's existing `GET /v1/models` catalog: a `tt models` command to list it, and `tt chat` using **real** per-model context windows for its budget instead of the V5b-3 prefix guess — with graceful fallback when the catalog is unavailable.

## Why now

`/v1/models` already returns, per model, `tokentrimmer.{provider, pricing, capabilities, max_input_tokens, max_output_tokens}` (deterministically sorted; `crates/core/src/routes/models.rs`). The CLI already authenticates against that gateway. So this is pure consumption — no server change — and it directly upgrades V5b-3's advisory budget to accurate windows.

## Architecture

A new crate-level module `crates/cli/src/catalog.rs` (used by both the `tt models` command and `tt chat`). It defines local `Deserialize` mirrors of the response (tt-core's structs are `Serialize`-only), a fetch, and small pure helpers.

### `crates/cli/src/catalog.rs`

```rust
pub struct CatalogModel {
    pub id: String,
    pub provider: String,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub capabilities: Vec<String>,
    pub input_per_million: Option<f64>,
    pub output_per_million: Option<f64>,
}
```

- **`parse_catalog(json: &str) -> anyhow::Result<Vec<CatalogModel>>`** (pure, tested): deserialize `{ data: [{ id, tokentrimmer: { provider, capabilities, max_input_tokens, max_output_tokens, pricing: { input_per_million, output_per_million }? } }] }` into `CatalogModel`s via private `Deserialize` structs. Tolerates a missing `pricing` (→ `None`).
- **`async fn fetch_catalog(http, base, key) -> anyhow::Result<Vec<CatalogModel>>`**: `GET {base}/v1/models` with bearer auth; bail on non-2xx; `parse_catalog(&resp.text().await?)`.
- **`windows_map(models: &[CatalogModel]) -> HashMap<String, u32>`** (pure, tested): `id → max_input_tokens` clamped to `u32` (`min(u32::MAX)`).
- **`format_window(tokens: u64) -> String`** (pure, tested): `>= 1_000_000 → "{}M"`, `>= 1_000 → "{}k"`, else the number. (e.g. 128_000 → `"128k"`, 1_000_000 → `"1M"`, 2_000_000 → `"2M"`.)
- **`pub async fn run(flag_key, flag_base) -> anyhow::Result<()>`** (the `tt models` command): `ResolvedContext::load` → `fetch_catalog` → a `ui::table(["MODEL","PROVIDER","CONTEXT","CAPS","$IN/1M","$OUT/1M"], colors)` with one row per model (`CONTEXT` = `format_window(max_input_tokens)`, `CAPS` = `capabilities.join(",")` or a trimmed subset, prices formatted or `-`). A muted summary line (`ui::note`) with the count. Errors surface via `ui::error`.

### `crates/cli/src/main.rs`
- New top-level command `Models { tt_api_key, tt_api_base }` → `tt_cli::catalog::run(tt_api_key, tt_api_base).await?`.

### `crates/cli/src/chat/budget.rs` — `ContextState` gains live windows
- `ContextState` holds `catalog_windows: HashMap<String, u32>` (empty when the catalog is unavailable):
  ```rust
  pub fn new(override_budget: Option<u32>, catalog_windows: HashMap<String, u32>) -> Self { … }
  pub fn budget(&self, model: &str) -> u32 {
      self.override_budget
          .or_else(|| self.catalog_windows.get(model).copied())
          .unwrap_or_else(|| model_window(model)) // V5b-3 prefix table = offline fallback
  }
  ```
- `model_window` stays exactly as-is (the fallback).

### `crates/cli/src/chat/mod.rs` — fetch at startup
- In `run`, after building `http`/`base`/`key` and before the REPL: best-effort `catalog::fetch_catalog(&http, &base, &key).await`; on `Ok` build `windows_map`, on `Err` use an empty map (silent fallback — offline/old gateway/pre-auth must not break chat). Pass the map into `ContextState::new(max_context, windows)`.
- Optional: when the catalog loaded, a dim `ui::note("(catalog: N models)")`; on failure, no output (the prefix table is a fine default). Keep it quiet.

## Testing
- **`parse_catalog`**: a representative `/v1/models` JSON (two models, one with pricing, one without) → two `CatalogModel`s with correct ids/providers/windows/caps; missing `pricing` → `None`. Malformed JSON → `Err`.
- **`windows_map`**: builds `id → window`; a huge `max_input_tokens` clamps to `u32::MAX` without panic.
- **`format_window`**: 128_000 → `"128k"`, 1_000_000 → `"1M"`, 2_000_000 → `"2M"`, 512 → `"512"`.
- **`ContextState::budget` precedence**: override wins; else catalog window for a known id; else the `model_window` prefix table for an unknown id; empty catalog → prefix table (V5b-3 behavior preserved).
- **Integration (httpmock)**: `fetch_catalog` against a mock `/v1/models` → the expected `CatalogModel`s; a 500 → `Err` (so chat falls back).
- `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo deny`; smoke (`tt models` against a mock or `--tt-api-base` to a dead port → clean error; `tt chat` still starts when the catalog fetch fails).

## Out of Scope (later V4)
- Server-side consolidation of the scattered adapter metadata + the staleness warn (**V4b**).
- True remote refresh of the gateway's catalog from an upstream API (**V4c**).
- Caching the catalog to disk / TTL refresh in the CLI (re-fetched per `tt models` and once per `tt chat` session).
- Using catalog **capabilities** in the CLI (e.g. auto-enabling `/tools` only for tool-capable models) — windows only for now.
