# V4b-2 — Model-Metadata Catalog for Compat Adapters Design

**Status:** approved (design — continues the V4b-1 pattern)
**Date:** 2026-06-05
**Slice:** V4b-2 (completes V4b consolidation). Extends the V4b-1 `ModelCatalog` to the compat adapters.
**Depends on:** V4b-1 (#31) merged.

## Goal

Finish consolidating model metadata: add the 4 OpenAI-compatible providers' models to the shared `models.toml`, and have each adapter's `models()` delegate to `model_catalog().for_provider(id)` — so **all** hosted providers read from one source (native already do, via V4b-1). No new infrastructure; this is the V4b-1 transcribe-and-delegate pattern applied to compat.

## Scope

### `crates/shared/data/models.toml` — append 18 compat models (verbatim from each adapter's current `models()`)
- **mistral** (5): `mistral-large-latest` 128000/4096, `mistral-medium-latest` 128000/4096, `mistral-small-latest` 128000/4096, `codestral-latest` 256000/8192, `pixtral-large-latest` 128000/4096 — all `[text, tools, json_mode, streaming]`; pixtral adds `vision`.
- **groq** (4): `llama-3.3-70b-versatile` 128000/8192, `llama-3.1-8b-instant` 128000/8192, `deepseek-r1-distill-llama-70b` 128000/8192 (+`reasoning`), `mixtral-8x7b-32768` 32768/4096 — base caps `[text, tools, json_mode, streaming]`.
- **together** (4): `meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo` 128000/8192, `meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo` 128000/8192, `Qwen/Qwen2.5-72B-Instruct-Turbo` 32768/4096, `deepseek-ai/DeepSeek-V3` 64000/8192 — `[text, tools, json_mode, streaming]`.
- **openrouter** (5): `anthropic/claude-sonnet-4-6` 200000/8192, `openai/gpt-5.5` 200000/16000, `google/gemini-3.1-pro` 1000000/8192, `meta-llama/llama-3.3-70b-instruct` 128000/8192, `mistralai/mistral-large` 128000/4096 — `[text, tools, json_mode, streaming]`.

Total catalog after this slice: 14 native + 18 compat = **32** models (matches the `pricing.toml` 36-pair note minus local/embedding-only deltas; the equivalence tests below pin the actual counts). The namespaced ids (e.g. `openrouter`/`anthropic/claude-sonnet-4-6`) are distinct `(provider, model)` keys from the native rows — no collision (and `ModelCatalog::parse` now rejects dups anyway).

### Adapter migration (`crates/providers/{mistral,groq,together,openrouter}/src/lib.rs`)
- Replace each free `fn models() -> Vec<ModelInfo>` body (the hardcoded `vec![…]`) with:
  ```rust
  fn models() -> Vec<ModelInfo> {
      tt_shared::model_catalog::model_catalog().for_provider("<id>")
  }
  ```
- Drop the now-unused `Capability` import in each (keep `ModelInfo`, `ModelPricing`, `catalog`). `pricing_table()` (rates) untouched.

## Equivalence / regression safety
- Extend the `model_catalog` unit tests: `len()` == 32; `for_provider("mistral").len()==5`, `groq`==4, `together`==4, `openrouter`==5; spot-check a few (e.g. `codestral-latest` 256000 input; `deepseek-r1-distill-llama-70b` has `Reasoning`; `openrouter`/`google/gemini-3.1-pro` 1_000_000 input).
- The existing compat-adapter + gateway `/v1/models` suites pass unchanged (the catalog reproduces each adapter's prior `models()` exactly).

## Testing
- `model_catalog` count + spot-check assertions (above).
- `cargo test --workspace` (compat adapter + `/v1/models` suites are the real guard); `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo deny`.

## Out of Scope
- Local providers (dynamic, empty `models()`) — unchanged.
- True remote refresh of the catalog — **V4c**.
- Moving compat **rates** (`CompatConfig.pricing_table` already delegates to the shared `pricing` catalog) — unchanged.
