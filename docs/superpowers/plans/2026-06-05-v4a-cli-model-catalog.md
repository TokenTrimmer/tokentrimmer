# V4a — CLI Consumes the Live Model Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `tt models` command and make `tt chat` use real `/v1/models` context windows for its budget, falling back to the V5b-3 prefix table when the catalog is unavailable.

**Architecture:** New crate-level `crates/cli/src/catalog.rs` (Deserialize mirrors of `/v1/models`, fetch, pure helpers, the `tt models` renderer). `ContextState` gains a `catalog_windows` map and a 3-tier `budget` precedence. `tt chat` best-effort-fetches the catalog at startup.

**Tech Stack:** Rust, `reqwest` (GET /v1/models), `serde` (Deserialize), `httpmock` (dev-dep), existing `ui`.

---

### Task 1: `catalog.rs` — parse + helpers

**Files:**
- Create: `crates/cli/src/catalog.rs`
- Modify: `crates/cli/src/lib.rs` (`pub mod catalog;`)

- [ ] **Step 1: Create the module with the pure pieces + tests**

Create `crates/cli/src/catalog.rs`:

```rust
//! The TokenTrimmer model catalog as served by the gateway's `GET /v1/models`.
//! Consumed by `tt models` and by `tt chat` (for real context-window budgets).

use std::collections::HashMap;

use anyhow::Context as _;
use serde::Deserialize;

use crate::context::ResolvedContext;
use crate::ui;

/// One model from the gateway catalog (flattened from the `/v1/models` shape).
pub struct CatalogModel {
    pub id: String,
    pub provider: String,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub capabilities: Vec<String>,
    pub input_per_million: Option<f64>,
    pub output_per_million: Option<f64>,
}

#[derive(Deserialize)]
struct RawResponse {
    data: Vec<RawEntry>,
}
#[derive(Deserialize)]
struct RawEntry {
    id: String,
    tokentrimmer: RawMeta,
}
#[derive(Deserialize)]
struct RawMeta {
    provider: String,
    #[serde(default)]
    capabilities: Vec<String>,
    max_input_tokens: u64,
    max_output_tokens: u64,
    #[serde(default)]
    pricing: Option<RawPricing>,
}
#[derive(Deserialize)]
struct RawPricing {
    input_per_million: f64,
    output_per_million: f64,
}

/// Parse a `/v1/models` response body into `CatalogModel`s.
pub fn parse_catalog(json: &str) -> anyhow::Result<Vec<CatalogModel>> {
    let resp: RawResponse = serde_json::from_str(json).context("parse /v1/models response")?;
    Ok(resp
        .data
        .into_iter()
        .map(|e| CatalogModel {
            id: e.id,
            provider: e.tokentrimmer.provider,
            max_input_tokens: e.tokentrimmer.max_input_tokens,
            max_output_tokens: e.tokentrimmer.max_output_tokens,
            capabilities: e.tokentrimmer.capabilities,
            input_per_million: e.tokentrimmer.pricing.as_ref().map(|p| p.input_per_million),
            output_per_million: e.tokentrimmer.pricing.as_ref().map(|p| p.output_per_million),
        })
        .collect())
}

/// Map of model id → input context window (for `tt chat`'s budget).
#[must_use]
pub fn windows_map(models: &[CatalogModel]) -> HashMap<String, u32> {
    models
        .iter()
        .map(|m| (m.id.clone(), u32::try_from(m.max_input_tokens).unwrap_or(u32::MAX)))
        .collect()
}

/// Compact token-count display: `128000 → "128k"`, `1000000 → "1M"`.
#[must_use]
pub fn format_window(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "object": "list",
        "data": [
            { "id": "gpt-4o-mini", "object": "model", "owned_by": "openai",
              "tokentrimmer": { "provider": "openai",
                "pricing": { "input_per_million": 0.15, "output_per_million": 0.6, "effective_at": "2026-05-01T00:00:00Z" },
                "capabilities": ["text","tools"], "max_input_tokens": 128000, "max_output_tokens": 16000 } },
            { "id": "claude-haiku-4-5", "object": "model", "owned_by": "anthropic",
              "tokentrimmer": { "provider": "anthropic", "pricing": null,
                "capabilities": ["text","vision"], "max_input_tokens": 200000, "max_output_tokens": 8192 } }
        ]
    }"#;

    #[test]
    fn parse_catalog_extracts_models() {
        let m = parse_catalog(SAMPLE).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "gpt-4o-mini");
        assert_eq!(m[0].provider, "openai");
        assert_eq!(m[0].max_input_tokens, 128_000);
        assert_eq!(m[0].capabilities, vec!["text", "tools"]);
        assert_eq!(m[0].input_per_million, Some(0.15));
        assert_eq!(m[1].input_per_million, None); // pricing: null
        assert!(parse_catalog("not json").is_err());
    }

    #[test]
    fn windows_map_and_format() {
        let m = parse_catalog(SAMPLE).unwrap();
        let w = windows_map(&m);
        assert_eq!(w.get("gpt-4o-mini").copied(), Some(128_000));
        assert_eq!(w.get("claude-haiku-4-5").copied(), Some(200_000));
        assert_eq!(format_window(128_000), "128k");
        assert_eq!(format_window(1_000_000), "1M");
        assert_eq!(format_window(2_000_000), "2M");
        assert_eq!(format_window(512), "512");
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/cli/src/lib.rs`, add (keeping alpha-ish order, after `pub mod cost_diff;`):

```rust
pub mod catalog;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p tt-cli --lib catalog 2>&1 | tail -12`
Expected: PASS (`parse_catalog_extracts_models`, `windows_map_and_format`).

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/catalog.rs crates/cli/src/lib.rs
git commit -m "feat(catalog): parse /v1/models + window helpers"
```

---

### Task 2: `fetch_catalog` + httpmock integration test

**Files:**
- Modify: `crates/cli/src/catalog.rs`

- [ ] **Step 1: Add `fetch_catalog`**

In `catalog.rs`, add after `format_window`:

```rust
/// Fetch the catalog from the gateway's `GET /v1/models`.
pub async fn fetch_catalog(
    http: &reqwest::Client,
    base: &str,
    key: &str,
) -> anyhow::Result<Vec<CatalogModel>> {
    let resp = http
        .get(format!("{base}/v1/models"))
        .bearer_auth(key)
        .send()
        .await
        .context("request /v1/models")?;
    if !resp.status().is_success() {
        anyhow::bail!("gateway returned {} for /v1/models", resp.status());
    }
    let body = resp.text().await.context("read /v1/models body")?;
    parse_catalog(&body)
}
```

- [ ] **Step 2: Write the integration tests**

Add to `catalog.rs` `mod tests`:

```rust
    use httpmock::prelude::*;

    #[tokio::test]
    async fn fetch_catalog_parses_mock() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(200)
                .header("content-type", "application/json")
                .body(SAMPLE);
        });
        let http = reqwest::Client::new();
        let models = fetch_catalog(&http, &server.base_url(), "k").await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[1].id, "claude-haiku-4-5");
    }

    #[tokio::test]
    async fn fetch_catalog_errors_on_5xx() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(503).body("nope");
        });
        let http = reqwest::Client::new();
        assert!(fetch_catalog(&http, &server.base_url(), "k").await.is_err());
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p tt-cli --lib catalog 2>&1 | tail -12`
Expected: PASS (4 catalog tests).

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/catalog.rs
git commit -m "feat(catalog): fetch_catalog (GET /v1/models) + httpmock tests"
```

---

### Task 3: `tt models` command

**Files:**
- Modify: `crates/cli/src/catalog.rs` (the `run` renderer)
- Modify: `crates/cli/src/main.rs` (the `Models` command + dispatch)

- [ ] **Step 1: Add the `run` renderer**

In `catalog.rs`, add after `fetch_catalog`:

```rust
/// `tt models` — fetch and print the gateway model catalog as a table.
pub async fn run(flag_key: Option<String>, flag_base: Option<String>) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login --token <KEY>` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();
    let models = fetch_catalog(&http, &base, &key).await?;

    let mut table = ui::table(
        &["MODEL", "PROVIDER", "CONTEXT", "CAPS", "$IN/1M", "$OUT/1M"],
        console::colors_enabled(),
    );
    for m in &models {
        let price = |p: Option<f64>| p.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        table.add_row(vec![
            m.id.clone(),
            m.provider.clone(),
            format_window(m.max_input_tokens),
            m.capabilities.join(","),
            price(m.input_per_million),
            price(m.output_per_million),
        ]);
    }
    println!("{table}");
    ui::note(&format!("{} models", models.len()));
    Ok(())
}
```

- [ ] **Step 2: Add the `Models` command to `main.rs`**

In the `Command` enum, after the `Chat { … }` variant's closing `},` add:

```rust
    /// List the gateway's model catalog (context windows, capabilities, pricing).
    Models {
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
    },
```

- [ ] **Step 3: Add the dispatch arm**

After the `Command::Chat { … } => { … }` arm, add:

```rust
        Command::Models {
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::catalog::run(tt_api_key, tt_api_base).await?;
        }
```

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p tt-cli 2>&1 | grep -E "^error" | head` then `cargo clippy -p tt-cli --all-targets -- -D warnings 2>&1 | grep -E "^warning|^error" | grep -v rgb | head`
Expected: no errors / no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/catalog.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt models — list the gateway model catalog"
```

---

### Task 4: `tt chat` uses live windows

**Files:**
- Modify: `crates/cli/src/chat/budget.rs` (`ContextState` + tests)
- Modify: `crates/cli/src/chat/mod.rs` (fetch at startup + pass windows)

- [ ] **Step 1: Add `catalog_windows` to `ContextState`**

In `budget.rs`, add the import at the top (with the other `use`s):

```rust
use std::collections::HashMap;
```

Change the struct + `new` + `budget`:

```rust
pub struct ContextState {
    pub override_budget: Option<u32>,
    /// Model id → input window from the live gateway catalog (`/v1/models`);
    /// empty when the catalog is unavailable.
    catalog_windows: HashMap<String, u32>,
    warned: bool,
}

impl ContextState {
    #[must_use]
    pub fn new(override_budget: Option<u32>, catalog_windows: HashMap<String, u32>) -> Self {
        Self {
            override_budget: override_budget.filter(|&n| n > 0),
            catalog_windows,
            warned: false,
        }
    }

    /// Effective budget for `model`: explicit override → live catalog window →
    /// the per-model prefix table (offline fallback).
    #[must_use]
    pub fn budget(&self, model: &str) -> u32 {
        self.override_budget
            .or_else(|| self.catalog_windows.get(model).copied())
            .unwrap_or_else(|| model_window(model))
    }
```

(Leave `estimate`, `manage`, and the rest unchanged.)

- [ ] **Step 2: Update the existing `ContextState::new` call sites in tests**

In `budget.rs` `mod tests`, update the four `ContextState::new(...)` calls to pass an empty map:

- `ContextState::new(Some(0))` → `ContextState::new(Some(0), HashMap::new())`
- `ContextState::new(Some(64_000))` → `ContextState::new(Some(64_000), HashMap::new())`
- `ContextState::new(None)` → `ContextState::new(None, HashMap::new())`
- `ContextState::new(Some((f64::from(est) / 0.80) as u32))` → `…, HashMap::new())`

- [ ] **Step 3: Add a precedence test**

Add to `budget.rs` `mod tests`:

```rust
    #[test]
    fn budget_precedence_override_catalog_prefix() {
        let mut cat = HashMap::new();
        cat.insert("custom-model".to_string(), 333_000u32);
        let st = ContextState::new(None, cat.clone());
        assert_eq!(st.budget("custom-model"), 333_000); // live catalog window
        assert_eq!(st.budget("gpt-4o-mini"), 128_000); // not in catalog → prefix table
        let ov = ContextState::new(Some(50_000), cat);
        assert_eq!(ov.budget("custom-model"), 50_000); // override beats catalog
    }
```

- [ ] **Step 4: Fetch the catalog at chat startup (`mod.rs`)**

In `run`, replace the line `let mut ctx = budget::ContextState::new(max_context);` with:

```rust
    // Best-effort: real per-model windows from the gateway catalog. On any
    // failure (offline / old gateway / pre-auth) fall back to the prefix table.
    let catalog_windows = match crate::catalog::fetch_catalog(&http, &base, &key).await {
        Ok(models) => crate::catalog::windows_map(&models),
        Err(_) => std::collections::HashMap::new(),
    };
    let mut ctx = budget::ContextState::new(max_context, catalog_windows);
```

- [ ] **Step 5: Build + chat/budget tests**

Run: `cargo test -p tt-cli --lib 'chat::budget' 2>&1 | tail -12` then `cargo build -p tt-cli 2>&1 | grep -E "^error" | head`
Expected: all budget tests pass (incl. the new precedence test); no build errors.

- [ ] **Step 6: Commit**

```bash
git add crates/cli/src/chat/budget.rs crates/cli/src/chat/mod.rs
git commit -m "feat(chat): use live /v1/models context windows (prefix-table fallback)"
```

---

### Task 5: Gates + smoke + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt -p tt-cli && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings.

- [ ] **Step 2: Full tests**

Run: `cargo test -p tt-cli 2>&1 | grep -E "test result|error\[" | tail -8`
Expected: all pass.

- [ ] **Step 3: cargo-deny**

Run: `cargo deny check advisories 2>&1 | tail -3`
Expected: `advisories ok` (no new deps).

- [ ] **Step 4: Smoke (dead port → clean error; chat still starts on catalog-fetch failure)**

Run:
```bash
cargo build -q -p tt-cli --bin tt
echo "--- tt models against a dead port → clean error, no panic ---"
TT_API_KEY=test TT_API_BASE=http://127.0.0.1:1 target/debug/tt models 2>&1 | tail -3
echo "--- tt chat starts despite catalog fetch failing (dead port) ---"
printf '/context\n/exit\n' | TT_API_KEY=test TT_API_BASE=http://127.0.0.1:1 target/debug/tt chat 2>&1 | grep -E "tt chat|context:"
```
Expected: `tt models` prints a clean error (request/connection refused), no panic; `tt chat` still starts and `/context` shows the prefix-table budget (128k for gpt-4o-mini) — catalog failure is silent.

- [ ] **Step 5: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** `parse_catalog`/`fetch_catalog`/`windows_map`/`format_window` (T1/T2), `tt models` (T3), `ContextState` catalog windows + precedence + chat startup fetch (T4), gates/smoke (T5). All spec items covered.
- **Placeholders:** none — every step has complete code.
- **Type consistency:** `parse_catalog(&str)->Result<Vec<CatalogModel>>`, `fetch_catalog(&Client,&str,&str)->Result<Vec<CatalogModel>>`, `windows_map(&[CatalogModel])->HashMap<String,u32>`, `format_window(u64)->String`, `ContextState::new(Option<u32>, HashMap<String,u32>)`, `budget(&str)->u32` used consistently; `Models` is a leaf command (plain `#[arg(long)]`, like `Mcp`).
- **Regression guard:** `model_window` and all V5b-3 behavior are unchanged; the catalog is strictly additive (empty map ⇒ identical to V5b-3). The four existing `ContextState::new` test calls are updated in T4 Step 2 so the signature change compiles.
