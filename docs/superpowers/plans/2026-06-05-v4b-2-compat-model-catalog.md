# V4b-2 — Compat-Adapter Model-Metadata Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Append the 4 compat providers' 18 models to `models.toml` and make each adapter's `models()` delegate to `model_catalog().for_provider(id)` — completing the V4b consolidation (catalog → 32 models).

**Architecture:** Pure extension of the V4b-1 pattern: data into the existing `crates/shared/data/models.toml`, adapters delegate to the existing `ModelCatalog`. No new code/infra.

**Tech Stack:** Rust, the V4b-1 `ModelCatalog`.

---

### Task 1: Add the 18 compat models (test-first)

**Files:**
- Modify: `crates/shared/src/model_catalog.rs` (extend the tests)
- Modify: `crates/shared/data/models.toml` (append compat rows)

- [ ] **Step 1: Update the count test + add compat spot-checks (red)**

In `crates/shared/src/model_catalog.rs`, replace the `embedded_catalog_parses_native_providers` test with an all-providers version, and add compat spot-checks:

```rust
    #[test]
    fn embedded_catalog_parses_all_providers() {
        let c = model_catalog();
        assert_eq!(c.len(), 32, "native (14) + compat (18)");
        assert_eq!(c.for_provider("openai").len(), 8);
        assert_eq!(c.for_provider("anthropic").len(), 3);
        assert_eq!(c.for_provider("gemini").len(), 3);
        assert_eq!(c.for_provider("mistral").len(), 5);
        assert_eq!(c.for_provider("groq").len(), 4);
        assert_eq!(c.for_provider("together").len(), 4);
        assert_eq!(c.for_provider("openrouter").len(), 5);
        assert!(c.for_provider("nonesuch").is_empty());
    }

    #[test]
    fn spot_check_compat_models() {
        let c = model_catalog();
        let codestral = c.model_info("mistral", "codestral-latest").unwrap();
        assert_eq!(codestral.max_input_tokens, 256_000);
        let pixtral = c.model_info("mistral", "pixtral-large-latest").unwrap();
        assert!(pixtral.capabilities.contains(&Capability::Vision));
        let deepseek = c.model_info("groq", "deepseek-r1-distill-llama-70b").unwrap();
        assert!(deepseek.capabilities.contains(&Capability::Reasoning));
        // namespaced ids are distinct (provider, model) keys
        let or_gemini = c.model_info("openrouter", "google/gemini-3.1-pro").unwrap();
        assert_eq!(or_gemini.max_input_tokens, 1_000_000);
        let together_v3 = c.model_info("together", "deepseek-ai/DeepSeek-V3").unwrap();
        assert_eq!(together_v3.max_input_tokens, 64_000);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-shared model_catalog 2>&1 | tail -12`
Expected: FAIL — `embedded_catalog_parses_all_providers` asserts `len()==32` but the toml still has 14; `spot_check_compat_models` finds no compat models.

- [ ] **Step 3: Append the compat rows to `models.toml`**

Append to the end of `crates/shared/data/models.toml`:

```toml

# ── Mistral (compat) ──────────────────────────────────────────────────────────
[[model]]
provider = "mistral"
model = "mistral-large-latest"
max_input_tokens = 128000
max_output_tokens = 4096
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "mistral"
model = "mistral-medium-latest"
max_input_tokens = 128000
max_output_tokens = 4096
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "mistral"
model = "mistral-small-latest"
max_input_tokens = 128000
max_output_tokens = 4096
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "mistral"
model = "codestral-latest"
max_input_tokens = 256000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "mistral"
model = "pixtral-large-latest"
max_input_tokens = 128000
max_output_tokens = 4096
capabilities = ["text", "vision", "tools", "json_mode", "streaming"]

# ── Groq (compat) ─────────────────────────────────────────────────────────────
[[model]]
provider = "groq"
model = "llama-3.3-70b-versatile"
max_input_tokens = 128000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "groq"
model = "llama-3.1-8b-instant"
max_input_tokens = 128000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "groq"
model = "deepseek-r1-distill-llama-70b"
max_input_tokens = 128000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming", "reasoning"]

[[model]]
provider = "groq"
model = "mixtral-8x7b-32768"
max_input_tokens = 32768
max_output_tokens = 4096
capabilities = ["text", "tools", "json_mode", "streaming"]

# ── Together (compat) ─────────────────────────────────────────────────────────
[[model]]
provider = "together"
model = "meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo"
max_input_tokens = 128000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "together"
model = "meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo"
max_input_tokens = 128000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "together"
model = "Qwen/Qwen2.5-72B-Instruct-Turbo"
max_input_tokens = 32768
max_output_tokens = 4096
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "together"
model = "deepseek-ai/DeepSeek-V3"
max_input_tokens = 64000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

# ── OpenRouter (compat; provider-namespaced ids) ──────────────────────────────
[[model]]
provider = "openrouter"
model = "anthropic/claude-sonnet-4-6"
max_input_tokens = 200000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "openrouter"
model = "openai/gpt-5.5"
max_input_tokens = 200000
max_output_tokens = 16000
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "openrouter"
model = "google/gemini-3.1-pro"
max_input_tokens = 1000000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "openrouter"
model = "meta-llama/llama-3.3-70b-instruct"
max_input_tokens = 128000
max_output_tokens = 8192
capabilities = ["text", "tools", "json_mode", "streaming"]

[[model]]
provider = "openrouter"
model = "mistralai/mistral-large"
max_input_tokens = 128000
max_output_tokens = 4096
capabilities = ["text", "tools", "json_mode", "streaming"]
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-shared model_catalog 2>&1 | tail -12`
Expected: PASS (all `model_catalog` tests, incl. the updated count + compat spot-checks). The dup-rejection in `parse` also confirms no `(provider, model)` collisions.

- [ ] **Step 5: Commit**

```bash
git add crates/shared/data/models.toml crates/shared/src/model_catalog.rs
git commit -m "feat(shared): add compat providers to the model catalog"
```

---

### Task 2: Migrate the compat adapters to the catalog

**Files:**
- Modify: `crates/providers/mistral/src/lib.rs`
- Modify: `crates/providers/groq/src/lib.rs`
- Modify: `crates/providers/together/src/lib.rs`
- Modify: `crates/providers/openrouter/src/lib.rs`

- [ ] **Step 1: Delegate each `models()` + drop the `Capability` import**

For each of the four files, (a) replace the free `fn models() -> Vec<ModelInfo>` body (the hardcoded `vec![…]`) with the one-line delegation, using the provider id, and (b) change `pricing::{Capability, ModelInfo, ModelPricing},` → `pricing::{ModelInfo, ModelPricing},`.

- `mistral`: `fn models() -> Vec<ModelInfo> { tt_shared::model_catalog::model_catalog().for_provider("mistral") }`
- `groq`: `…for_provider("groq")`
- `together`: `…for_provider("together")`
- `openrouter`: `…for_provider("openrouter")`

- [ ] **Step 2: Build + the compat adapter + gateway suites**

Run: `cargo test -p tt-provider-mistral -p tt-provider-groq -p tt-provider-together -p tt-provider-openrouter -p tt-core 2>&1 | grep -E "test result|error\[|FAILED" | tail -20`
Expected: all pass — the adapters now return the catalog data; the compat-adapter + `/v1/models` tests (which pin ids/windows) still hold. If any adapter test asserts a model **order** that differs from the file order, align the `models.toml` block order to it (do not weaken the test).

- [ ] **Step 3: Clippy (unused imports)**

Run: `cargo clippy -p tt-provider-mistral -p tt-provider-groq -p tt-provider-together -p tt-provider-openrouter --all-targets -- -D warnings 2>&1 | grep -E "^warning|^error" | grep -v rgb | head`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/providers/mistral/src/lib.rs crates/providers/groq/src/lib.rs crates/providers/together/src/lib.rs crates/providers/openrouter/src/lib.rs
git commit -m "refactor(providers): compat adapters read models from the catalog"
```

---

### Task 3: Gates + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy (workspace)**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings. Commit any fmt diff.

- [ ] **Step 2: Full workspace tests**

Run: `cargo test --workspace 2>&1 | grep -E "test result: FAILED|error\[|^failures:" | head` then `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed" | awk '{s+=$1} END{print s" passed"}'`
Expected: no failures; the gateway `/v1/models` + compat-adapter suites pass unchanged.

- [ ] **Step 3: cargo-deny**

Run: `cargo deny check advisories 2>&1 | tail -3`
Expected: `advisories ok` (no new deps).

- [ ] **Step 4: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** 18 compat rows (T1), 4 adapter migrations (T2), gates (T3). All spec items covered.
- **Placeholders:** none — the data and code are complete.
- **Type consistency:** each `fn models() -> Vec<ModelInfo>` delegates to `model_catalog().for_provider("<id>")`; imports drop `Capability` (keep `ModelInfo`, `ModelPricing`). The catalog count becomes 32 (14 native + 18 compat).
- **Equivalence guard:** the 18 rows are transcribed verbatim from each adapter's current `models()` (mistral 5 / groq 4 / together 4 / openrouter 5); the count + spot-check tests pin values, the dup-rejection in `parse` catches any `(provider, model)` collision, and the unchanged compat-adapter + `/v1/models` suites fail if a row is wrong. Rates (`pricing_table`) untouched.
