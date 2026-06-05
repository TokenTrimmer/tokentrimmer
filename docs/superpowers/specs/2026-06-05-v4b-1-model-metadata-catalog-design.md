# V4b-1 — Server-Side Model-Metadata Catalog (native adapters) Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V4b-1 (first of the V4b consolidation; V4b-2 = compat adapters). Part of the V4 "live model catalog" area (V4a CLI-consume merged #30).
**Depends on:** nothing new (tt-shared is already a universal dep).

## Goal

Make per-model **metadata** (context windows + capabilities) a single embedded data source instead of hardcoded Rust in each provider adapter — mirroring the proven `pricing.toml`/`PricingCatalog` pattern. This slice covers the 3 native adapters (openai, anthropic, gemini); compat adapters follow in V4b-2. Also wire the dormant pricing-staleness warning at gateway startup.

## Why this shape

Rates already live in one embedded data file (`pricing.toml` → `catalog()`), but windows + capabilities are hardcoded `Vec<ModelInfo>` in ~7 adapters. Consolidating metadata the same way: (1) one place to add/update a model's window or caps, (2) sets up V4c (a refresh just swaps the data source), (3) consistent with the existing, trusted pattern. Rates stay in `pricing.toml` (this is metadata only).

## Architecture

### `crates/shared/data/models.toml` (new)
Embedded metadata, one `[[model]]` per (provider, model):
```toml
[[model]]
provider = "anthropic"
model = "claude-haiku-4-5"
max_input_tokens = 200000
max_output_tokens = 8192
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]
```
- `capabilities` are the snake_case `Capability` serde names (`json_mode`, `prompt_caching`, …) — they deserialize straight into `Vec<Capability>`.
- This slice contains exactly the openai + anthropic + gemini models currently returned by their `all_models()` (transcribed verbatim from the adapters; the equivalence tests below guarantee no drift).

### `crates/shared/src/model_catalog.rs` (new module, mirrors `pricing.rs`)
```rust
pub struct ModelCatalog { models: Vec<ModelInfo> }

impl ModelCatalog {
    pub fn parse(toml_text: &str) -> Result<Self, toml::de::Error>; // via a Raw{provider,model,...} row
    pub fn for_provider(&self, provider: &str) -> Vec<ModelInfo>;   // filtered + cloned, input order
    pub fn model_info(&self, provider: &str, model: &str) -> Option<ModelInfo>;
    pub fn all(&self) -> &[ModelInfo];
    pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool;
}

/// Process-wide catalog, parsed once from the embedded `models.toml`.
pub fn model_catalog() -> &'static ModelCatalog; // OnceLock + include_str!("../data/models.toml")
```
- Private `RawModel { provider: String, model: String, max_input_tokens: u64, max_output_tokens: u64, capabilities: Vec<Capability> }` and `RawCatalog { model: Vec<RawModel> }`; `parse` maps each `RawModel` → `ModelInfo { id: model, provider, capabilities, max_input_tokens, max_output_tokens }`.
- Re-export from `crates/shared/src/lib.rs` (`pub mod model_catalog;` + `pub use model_catalog::{model_catalog, ModelCatalog};`).

### Adapter migration (behavior-preserving)
- `crates/providers/anthropic/src/pricing.rs::all_models()` → `tt_shared::model_catalog::model_catalog().for_provider("anthropic")`.
- Same for `openai` (`"openai"`) and `gemini` (`"gemini"`).
- The hardcoded `Vec<ModelInfo>` literals are deleted; `pricing_for()` (rates) is untouched.

### Staleness warning
- At gateway startup (`run_gateway` in `crates/cli/src/main.rs`), after providers are registered, compute the pricing catalog age from `tt_shared::pricing::catalog().catalog_max_effective_at()` and `chrono::Utc::now()`; if older than `STALE_AFTER_DAYS` (90), `tracing::warn!` with the age and the newest `effective_at` (the dormant infra the docs already reference). No behavior change beyond the log line.

## Equivalence / regression safety
`ModelInfo` and `Capability` are compared by value (confirm `PartialEq`; add the derive if missing — additive). Tests:
- **`model_catalog` unit tests** (in tt-shared): `model_catalog()` parses; `len()` == expected native count; `for_provider("anthropic")` has 3 models, `claude-haiku-4-5` → 200_000 input / 8192 output and exactly the 6 caps; `model_info("openai","gpt-4o-mini")` window matches; unknown provider/model → empty/None.
- **Per-adapter tests**: each adapter keeps/﻿gains a test asserting `all_models()` returns the same ids + windows + caps it did before (the values are pinned in the test, so a wrong `models.toml` row fails). Existing adapter tests that exercise `all_models()` also guard this.
- **Gateway `/v1/models`**: the existing `routes/models.rs` test (and any snapshot) must still pass unchanged — the catalog reproduces the prior data exactly.

## Testing
- `model_catalog` parse + lookups (above).
- Adapter equivalence assertions (anthropic/openai/gemini).
- Staleness: a pure helper `is_stale(newest: Option<DateTime<Utc>>, now, days) -> bool` unit-tested (None → false; 100 days → true; 10 days → false), so the startup wiring is a thin call.
- `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo deny`; `cargo test --workspace` (the gateway/adapter suites are the real guard).

## Out of Scope (V4b-2 / later)
- Compat adapters (mistral, groq, together, openrouter) + `CompatConfig.models` — migrate to the catalog in **V4b-2**.
- Moving **rates** out of `pricing.toml` (they already are consolidated there).
- Local providers (dynamic, empty `models()`) — unchanged.
- True remote refresh of either catalog — **V4c**.
