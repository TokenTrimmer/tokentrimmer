# V4b-1 — Server-Side Model-Metadata Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move native-adapter (openai/anthropic/gemini) model metadata (windows + capabilities) into one embedded `models.toml` + `ModelCatalog`, migrate those adapters to read from it, and wire the pricing-staleness startup warning.

**Architecture:** New `crates/shared/data/models.toml` + `crates/shared/src/model_catalog.rs` mirror `pricing.toml`/`PricingCatalog` (embedded `include_str!`, `OnceLock`). The 3 native adapters delegate `all_models()` to `model_catalog().for_provider(id)`. A pure `pricing::is_stale` powers a `tracing::warn!` at gateway startup. Rates stay in `pricing.toml`.

**Tech Stack:** Rust, `toml` + `serde` (already tt-shared deps), `chrono` (already a dep), `tracing`.

---

### Task 1: `models.toml` + `ModelCatalog` + `ModelInfo: PartialEq`

**Files:**
- Modify: `crates/shared/src/pricing.rs` (add `PartialEq, Eq` to `ModelInfo`)
- Create: `crates/shared/data/models.toml`
- Create: `crates/shared/src/model_catalog.rs`
- Modify: `crates/shared/src/lib.rs` (export the module)

- [ ] **Step 1: Add `PartialEq, Eq` to `ModelInfo`**

In `crates/shared/src/pricing.rs`, change the `ModelInfo` derive:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
```

- [ ] **Step 2: Create `crates/shared/data/models.toml`**

```toml
# Model metadata catalog — per-(provider, model) context windows + capabilities.
# RATES live in pricing.toml; this file is METADATA ONLY. Embedded at build time
# (see model_catalog.rs) and parsed once into the ModelCatalog — the single
# source of truth for ModelInfo across provider adapters and GET /v1/models.
#
# Schema — one [[model]] per (provider, model):
#   provider          = registry provider id ("openai", "anthropic", "gemini", …)
#   model             = exact model id the provider matches on
#   max_input_tokens  = context window (input) upper bound
#   max_output_tokens = completion upper bound (0 for embedding models)
#   capabilities      = snake_case Capability names: text, vision, audio, tools,
#                       json_mode, streaming, reasoning, prompt_caching
#
# V4b-1 covers the native adapters (openai, anthropic, gemini); compat providers
# (mistral, groq, together, openrouter) are added in V4b-2.

# ── OpenAI ──────────────────────────────────────────────────────────────────
[[model]]
provider = "openai"
model = "gpt-5.5"
max_input_tokens = 200000
max_output_tokens = 16000
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

[[model]]
provider = "openai"
model = "gpt-5.4"
max_input_tokens = 200000
max_output_tokens = 16000
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

[[model]]
provider = "openai"
model = "gpt-4o"
max_input_tokens = 128000
max_output_tokens = 16000
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

[[model]]
provider = "openai"
model = "gpt-4o-mini"
max_input_tokens = 128000
max_output_tokens = 16000
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

[[model]]
provider = "openai"
model = "o3"
max_input_tokens = 200000
max_output_tokens = 100000
capabilities = ["text", "tools", "json_mode", "reasoning", "streaming"]

[[model]]
provider = "openai"
model = "o4-mini"
max_input_tokens = 200000
max_output_tokens = 100000
capabilities = ["text", "tools", "json_mode", "reasoning", "streaming"]

[[model]]
provider = "openai"
model = "text-embedding-3-small"
max_input_tokens = 8191
max_output_tokens = 0
capabilities = ["text"]

[[model]]
provider = "openai"
model = "text-embedding-3-large"
max_input_tokens = 8191
max_output_tokens = 0
capabilities = ["text"]

# ── Anthropic ───────────────────────────────────────────────────────────────
[[model]]
provider = "anthropic"
model = "claude-haiku-4-5"
max_input_tokens = 200000
max_output_tokens = 8192
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

[[model]]
provider = "anthropic"
model = "claude-sonnet-4-6"
max_input_tokens = 200000
max_output_tokens = 8192
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

[[model]]
provider = "anthropic"
model = "claude-opus-4-7"
max_input_tokens = 200000
max_output_tokens = 8192
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

# ── Gemini ──────────────────────────────────────────────────────────────────
[[model]]
provider = "gemini"
model = "gemini-3.1-flash-lite"
max_input_tokens = 1000000
max_output_tokens = 8192
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

[[model]]
provider = "gemini"
model = "gemini-3.5-flash"
max_input_tokens = 1000000
max_output_tokens = 8192
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]

[[model]]
provider = "gemini"
model = "gemini-3.1-pro"
max_input_tokens = 2000000
max_output_tokens = 8192
capabilities = ["text", "vision", "tools", "json_mode", "streaming", "prompt_caching"]
```

- [ ] **Step 3: Create `crates/shared/src/model_catalog.rs`**

```rust
//! Model METADATA catalog — per-(provider, model) context windows + capabilities.
//! Rates live in `pricing.rs`/`pricing.toml`; this is metadata only. Embedded at
//! build time and parsed once (mirroring `PricingCatalog`) — the single source of
//! truth for `ModelInfo` across provider adapters and `GET /v1/models`.

use std::sync::OnceLock;

use serde::Deserialize;

use crate::pricing::{Capability, ModelInfo};

const MODELS_TOML: &str = include_str!("../data/models.toml");

#[derive(Debug, Deserialize)]
struct RawModel {
    provider: String,
    model: String,
    max_input_tokens: u64,
    max_output_tokens: u64,
    #[serde(default)]
    capabilities: Vec<Capability>,
}

#[derive(Debug, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    model: Vec<RawModel>,
}

/// In-memory model-metadata catalog, built once from the embedded TOML.
#[derive(Debug)]
pub struct ModelCatalog {
    models: Vec<ModelInfo>,
}

impl ModelCatalog {
    /// Parse a catalog from TOML text (exposed for tests).
    pub fn parse(toml_text: &str) -> Result<Self, toml::de::Error> {
        let raw: RawCatalog = toml::from_str(toml_text)?;
        let models = raw
            .model
            .into_iter()
            .map(|m| ModelInfo {
                id: m.model,
                provider: m.provider,
                capabilities: m.capabilities,
                max_input_tokens: m.max_input_tokens,
                max_output_tokens: m.max_output_tokens,
            })
            .collect();
        Ok(Self { models })
    }

    /// All models for `provider`, in file order.
    #[must_use]
    pub fn for_provider(&self, provider: &str) -> Vec<ModelInfo> {
        self.models
            .iter()
            .filter(|m| m.provider == provider)
            .cloned()
            .collect()
    }

    /// Metadata for an exact `(provider, model)`.
    #[must_use]
    pub fn model_info(&self, provider: &str, model: &str) -> Option<ModelInfo> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == model)
            .cloned()
    }

    #[must_use]
    pub fn all(&self) -> &[ModelInfo] {
        &self.models
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// The process-wide model-metadata catalog, parsed once from the embedded
/// `data/models.toml`. A unit test guards the bundled file's validity.
pub fn model_catalog() -> &'static ModelCatalog {
    static CATALOG: OnceLock<ModelCatalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| ModelCatalog::parse(MODELS_TOML).expect("embedded data/models.toml must be valid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_parses_native_providers() {
        let c = model_catalog();
        assert_eq!(c.len(), 14, "native model count");
        assert_eq!(c.for_provider("openai").len(), 8);
        assert_eq!(c.for_provider("anthropic").len(), 3);
        assert_eq!(c.for_provider("gemini").len(), 3);
        assert!(c.for_provider("nonesuch").is_empty());
        assert!(!c.is_empty());
    }

    #[test]
    fn spot_check_known_models() {
        let c = model_catalog();
        let haiku = c.model_info("anthropic", "claude-haiku-4-5").unwrap();
        assert_eq!(haiku.max_input_tokens, 200_000);
        assert_eq!(haiku.max_output_tokens, 8192);
        assert_eq!(
            haiku.capabilities,
            vec![
                Capability::Text,
                Capability::Vision,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
                Capability::PromptCaching,
            ]
        );
        let o3 = c.model_info("openai", "o3").unwrap();
        assert_eq!(o3.max_input_tokens, 200_000);
        assert_eq!(o3.max_output_tokens, 100_000);
        assert!(o3.capabilities.contains(&Capability::Reasoning));
        let pro = c.model_info("gemini", "gemini-3.1-pro").unwrap();
        assert_eq!(pro.max_input_tokens, 2_000_000);
        assert!(c.model_info("openai", "nope").is_none());
    }
}
```

- [ ] **Step 4: Export the module**

In `crates/shared/src/lib.rs`, add `pub mod model_catalog;` (after `pub mod messages;`) and extend the re-exports:

```rust
pub use model_catalog::{model_catalog, ModelCatalog};
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p tt-shared model_catalog 2>&1 | tail -15`
Expected: PASS (`embedded_catalog_parses_native_providers`, `spot_check_known_models`).

- [ ] **Step 6: Commit**

```bash
git add crates/shared/src/pricing.rs crates/shared/data/models.toml crates/shared/src/model_catalog.rs crates/shared/src/lib.rs
git commit -m "feat(shared): model-metadata catalog (models.toml + ModelCatalog)"
```

---

### Task 2: Migrate the native adapters to the catalog

**Files:**
- Modify: `crates/providers/anthropic/src/pricing.rs`
- Modify: `crates/providers/openai/src/pricing.rs`
- Modify: `crates/providers/gemini/src/pricing.rs`

- [ ] **Step 1: Anthropic — delegate `all_models()`**

Replace the whole `all_models()` body (the `let capabilities = …; vec![ … ]`) with:

```rust
pub fn all_models() -> Vec<ModelInfo> {
    tt_shared::model_catalog::model_catalog().for_provider("anthropic")
}
```

Then fix the import (Capability is no longer used here):

```rust
use tt_shared::pricing::{catalog, ModelInfo, ModelPricing};
```

- [ ] **Step 2: OpenAI — delegate `all_models()`**

Replace the `all_models()` body with:

```rust
pub fn all_models() -> Vec<ModelInfo> {
    tt_shared::model_catalog::model_catalog().for_provider("openai")
}
```

Fix the import in `crates/providers/openai/src/pricing.rs` so `Capability` is dropped if now unused (keep `ModelInfo`, `ModelPricing`, `catalog` as used). Run clippy to confirm.

- [ ] **Step 3: Gemini — delegate `all_models()`**

Replace the `all_models()` body with:

```rust
pub fn all_models() -> Vec<ModelInfo> {
    tt_shared::model_catalog::model_catalog().for_provider("gemini")
}
```

Drop the now-unused `Capability` import.

- [ ] **Step 4: Build + the adapter + gateway suites (the real equivalence guard)**

Run: `cargo test -p tt-provider-anthropic -p tt-provider-openai -p tt-provider-gemini -p tt-core 2>&1 | grep -E "test result|error\[|FAILED" | tail -20`
Expected: all pass — the adapters now return the catalog data, and the existing adapter/`/v1/models` tests (which pin model ids/windows) still hold. If an adapter test asserts a model **order** that differs from the file order, align `models.toml` order to it (do not weaken the test).

- [ ] **Step 5: Clippy (unused imports)**

Run: `cargo clippy -p tt-provider-anthropic -p tt-provider-openai -p tt-provider-gemini --all-targets -- -D warnings 2>&1 | grep -E "^warning|^error" | grep -v rgb | head`
Expected: no warnings (fix any leftover unused `Capability`/`ModelInfo` import).

- [ ] **Step 6: Commit**

```bash
git add crates/providers/anthropic/src/pricing.rs crates/providers/openai/src/pricing.rs crates/providers/gemini/src/pricing.rs
git commit -m "refactor(providers): native adapters read models from the catalog"
```

---

### Task 3: Pricing-staleness startup warning

**Files:**
- Modify: `crates/shared/src/pricing.rs` (`is_stale` helper + test)
- Modify: `crates/cli/src/main.rs` (`run_gateway` wiring)

- [ ] **Step 1: Write the failing test for `is_stale`**

In `crates/shared/src/pricing.rs`, inside the existing `#[cfg(test)] mod catalog_tests` (or add one), add:

```rust
    #[test]
    fn is_stale_thresholds() {
        use chrono::Duration;
        let now: DateTime<Utc> = "2026-06-05T00:00:00Z".parse().unwrap();
        assert!(!is_stale(None, now, 90)); // empty catalog: not stale
        assert!(!is_stale(Some(now - Duration::days(10)), now, 90));
        assert!(is_stale(Some(now - Duration::days(100)), now, 90));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p tt-shared is_stale_thresholds 2>&1 | tail -10`
Expected: FAIL to compile — `cannot find function is_stale`.

- [ ] **Step 3: Add `is_stale`**

In `crates/shared/src/pricing.rs`, add (a free `pub fn`, near `catalog()`):

```rust
/// Whether `newest` (the catalog's max `effective_at`) is more than `max_days`
/// before `now`. An empty catalog (`None`) is treated as not stale.
#[must_use]
pub fn is_stale(newest: Option<DateTime<Utc>>, now: DateTime<Utc>, max_days: i64) -> bool {
    match newest {
        Some(d) => (now - d).num_days() > max_days,
        None => false,
    }
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p tt-shared is_stale_thresholds 2>&1 | tail -6`
Expected: PASS.

- [ ] **Step 5: Wire the warning in `run_gateway`**

In `crates/cli/src/main.rs`, locate `run_gateway` and (after provider registration / near where `AppState` is built, before serving) add:

```rust
    // Surface a stale embedded pricing catalog (the dormant freshness signal).
    const PRICING_STALE_DAYS: i64 = 90;
    let newest = tt_shared::pricing::catalog().catalog_max_effective_at();
    if tt_shared::pricing::is_stale(newest, chrono::Utc::now(), PRICING_STALE_DAYS) {
        if let Some(d) = newest {
            tracing::warn!(
                newest_effective_at = %d,
                "pricing catalog is over {PRICING_STALE_DAYS} days old — rates may be stale; refresh data/pricing.toml"
            );
        }
    }
```

(If `chrono` is not already imported in `main.rs`, use the fully-qualified `chrono::Utc::now()` as written — no `use` needed.)

- [ ] **Step 6: Build**

Run: `cargo build -p tt-cli 2>&1 | grep -E "^error" | head`
Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add crates/shared/src/pricing.rs crates/cli/src/main.rs
git commit -m "feat(gateway): warn at startup when the pricing catalog is stale"
```

---

### Task 4: Gates + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy (workspace)**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings. Commit any fmt diff.

- [ ] **Step 2: Full workspace tests**

Run: `cargo test --workspace 2>&1 | grep -E "test result: FAILED|error\[|^failures:" | head`
Then: `cargo test --workspace 2>&1 | grep -cE "test result: ok"`
Expected: no failures; the gateway `/v1/models` and adapter suites pass unchanged.

- [ ] **Step 3: cargo-deny**

Run: `cargo deny check advisories 2>&1 | tail -3`
Expected: `advisories ok` (no new deps).

- [ ] **Step 4: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** `models.toml` + `ModelCatalog` (T1), native-adapter migration (T2), staleness `is_stale` + startup warn (T3), gates (T4). All spec items covered.
- **Placeholders:** none — the data file and code are complete.
- **Type consistency:** `ModelCatalog::{parse, for_provider(&str)->Vec<ModelInfo>, model_info(&str,&str)->Option<ModelInfo>, all, len, is_empty}` and `model_catalog()->&'static ModelCatalog`; `is_stale(Option<DateTime<Utc>>, DateTime<Utc>, i64)->bool`. `RawCatalog{ model: Vec<RawModel> }` matches the `[[model]]` TOML array; `RawModel.capabilities: Vec<Capability>` deserializes the snake_case names. `ModelInfo` gains `PartialEq, Eq` (T1 Step 1) used by the catalog tests.
- **Equivalence guard:** the 14 `models.toml` rows are transcribed verbatim from the current `all_models()` (openai 8 / anthropic 3 / gemini 3); the spot-check tests pin ids/windows/caps, and the unchanged adapter + `/v1/models` suites fail if any row is wrong. Rates (`pricing.toml`/`pricing_for`) are untouched.
