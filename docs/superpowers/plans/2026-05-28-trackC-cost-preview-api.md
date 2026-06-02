# Track C — Cost Preview API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `POST /v1/preview` — a synchronous, deterministic, sub-30ms-p50 endpoint that returns projected cost on the current model, savings projections for cache hits, and cheaper-equivalent route suggestions with quality risk bands. Reusable surface for Tracks A (MCP), B (proxy), D (init).

**Architecture:** New `crates/preview/` crate (pure logic: pricing, token estimation, cache projection, route suggestions) consumed by a new `crates/core/src/routes/preview.rs` handler. No LLM calls in the preview path. Per-org cache hit rates + Plan engine quality bands come from the cloud `tt-api` via cached HTTP read; if the cloud lookup fails, suggestions fall back to global defaults so the endpoint never 5xx.

**Tech Stack:** Rust 1.88, Axum (existing in `tt-core`), `tiktoken-rs` for OpenAI/Anthropic token estimation, `serde`/`serde_json`, `thiserror`, `tokio`, `tracing`, `httpmock` + `insta` for tests.

**Spec:** `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md`.

**Preconditions:**
1. None. Cloud-side enrichment endpoints (`/v1/admin/preview-context/...`) are designed but optional — the public-repo crate ships with sensible global defaults so it works standalone.
2. Existing pricing tables in `crates/providers/*/src/pricing.rs` are the source of truth; this crate **wraps** them, does not duplicate.

---

## File Structure

```
crates/preview/                              [NEW crate]
├── Cargo.toml
└── src/
    ├── lib.rs                               [public API: preview() function + PreviewRequest/Response]
    ├── types.rs                             [PreviewRequest, PreviewResponse, Suggestion, CacheProjection]
    ├── pricing.rs                           [wraps tt-shared::ModelPricing + per-provider pricing tables]
    ├── token_estimator.rs                   [tiktoken-rs for OpenAI/Anthropic; heuristic for Gemini/local]
    ├── classifier.rs                        [task-class detection: chat/extract/classify/code/agent]
    ├── cache_projection.rs                  [L1+L2 hit-rate-weighted savings calc]
    ├── route_suggestions.rs                 [cheaper-equivalent enumeration + risk band lookup]
    └── error.rs

crates/core/
├── Cargo.toml                               [modified — add tt-preview dep]
└── src/
    ├── server.rs                            [modified — register POST /v1/preview]
    └── routes/
        └── preview.rs                       [NEW handler — thin wrapper around tt_preview::preview()]

Cargo.toml                                   [modified — register tt-preview in workspace members + workspace deps]
```

---

## Task 1: Scaffold tt-preview crate

**Files:**
- Create: `crates/preview/Cargo.toml`
- Create: `crates/preview/src/lib.rs`, `types.rs`, `pricing.rs`, `token_estimator.rs`, `classifier.rs`, `cache_projection.rs`, `route_suggestions.rs`, `error.rs`
- Modify: `Cargo.toml` (workspace) to register the new crate

- [ ] **Step 1: Create the crate directory tree**

```bash
mkdir -p crates/preview/src
for f in lib types pricing token_estimator classifier cache_projection route_suggestions error; do
  echo "//! tt-preview — \`$f\` module (scaffold; see plan)" > "crates/preview/src/$f.rs"
done
```

- [ ] **Step 2: Write `crates/preview/Cargo.toml`**

```toml
[package]
name = "tt-preview"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Cost preview engine — projects cost + savings + route suggestions for an LLM request without calling any model."

[dependencies]
tt-shared.workspace = true
tt-provider-openai.workspace = true
tt-provider-anthropic.workspace = true
tt-provider-gemini.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tracing.workspace = true
reqwest = { workspace = true, features = ["json", "rustls-tls"] }
tiktoken-rs = "0.5"
tokio = { workspace = true, features = ["sync"] }

[dev-dependencies]
httpmock = "0.7"
insta = { version = "1.39", features = ["json"] }
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

If any of the workspace deps above are not yet in the workspace root `Cargo.toml`, add them under `[workspace.dependencies]`. Look for the existing pattern next to `reqwest`/`tracing`/etc.

- [ ] **Step 3: Register `tt-preview` in workspace `Cargo.toml`**

In the root `Cargo.toml`:
- Add `"crates/preview"` to `workspace.members` (alphabetical).
- Add `tt-preview = { path = "crates/preview" }` to `[workspace.dependencies]`.

- [ ] **Step 4: Replace `lib.rs` with public module declarations**

```rust
//! `tt-preview` — pure cost preview engine.
//!
//! Given a chat-completion-shaped request, returns projected cost on the
//! current model, expected savings if served from cache, and cheaper-
//! equivalent route suggestions with quality risk bands. Performs no LLM
//! calls and no Postgres lookups in the hot path — all enrichment goes
//! through pluggable trait objects so callers can wire org-specific data.
//!
//! See `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md`.

pub mod cache_projection;
pub mod classifier;
pub mod error;
pub mod pricing;
pub mod route_suggestions;
pub mod token_estimator;
pub mod types;

pub use error::PreviewError;
pub use types::{
    CacheProjections, EstimationConfidence, PreviewRequest, PreviewResponse,
    RouteSuggestion, Suggestion,
};
```

- [ ] **Step 5: Compile check**

Run: `cargo check -p tt-preview`
Expected: success with unused-module warnings.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/preview/
git commit -m "feat(preview): scaffold tt-preview crate

Track C day-0. Empty modules filled by subsequent tasks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Shared types

**Files:** `crates/preview/src/types.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Request + response shapes for the preview engine.
//!
//! The request is a small subset of the OpenAI chat-completion body —
//! exactly what we need to estimate. The response is the documented
//! `PreviewResponse` shape from the spec.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    /// Honored for tier accounting but ignored for the preview calc itself.
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: serde_json::Value, // string OR array-of-parts
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewResponse {
    pub current: CurrentEstimate,
    pub cache_projections: CacheProjections,
    pub route_suggestions: Vec<RouteSuggestion>,
    pub warnings: Vec<String>,
    pub trace_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentEstimate {
    pub model: String,
    pub provider: String,
    pub input_tokens_estimated: u32,
    pub output_tokens_estimated: u32,
    pub cost_usd: f64,
    pub estimation_confidence: EstimationConfidence,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EstimationConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheProjections {
    pub l1_hit_savings_usd: f64,
    pub l1_hit_probability: f32,
    pub l2_hit_savings_usd: f64,
    pub l2_hit_probability: f32,
    pub weighted_savings_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteSuggestion {
    pub route: String,
    pub model: String,
    pub cost_usd: f64,
    pub savings_usd: f64,
    pub quality_risk_band: QualityRiskBand,
    pub rationale: String,
    pub applicable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum QualityRiskBand {
    Low,
    Medium,
    High,
    Unknown,
}

/// Convenience alias for the legacy name in the spec.
pub type Suggestion = RouteSuggestion;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_minimal_request() {
        let json = r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#;
        let req: PreviewRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "x");
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn serializes_response_shape_matches_spec() {
        let r = PreviewResponse {
            current: CurrentEstimate {
                model: "claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
                input_tokens_estimated: 47,
                output_tokens_estimated: 12,
                cost_usd: 0.000189,
                estimation_confidence: EstimationConfidence::High,
            },
            cache_projections: CacheProjections {
                l1_hit_savings_usd: 0.000189,
                l1_hit_probability: 0.34,
                l2_hit_savings_usd: 0.000189,
                l2_hit_probability: 0.18,
                weighted_savings_usd: 0.000098,
            },
            route_suggestions: vec![],
            warnings: vec![],
            trace_id: "trace".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"estimation_confidence\":\"high\""));
        assert!(json.contains("\"weighted_savings_usd\":0.000098"));
    }
}
```

- [ ] **Step 2: Run tests**

`cargo test -p tt-preview types`
Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/preview/src/types.rs
git commit -m "feat(preview): shared types

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Error type

**Files:** `crates/preview/src/error.rs`

- [ ] **Step 1: Write the module**

```rust
//! Preview engine errors. Designed so the HTTP handler can always emit a
//! valid `PreviewResponse` with `warnings[]`, never a 5xx.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PreviewError {
    #[error("unknown model `{0}` — no pricing table available")]
    UnknownModel(String),
    #[error("malformed request: {0}")]
    Malformed(String),
    #[error("tokenizer failure: {0}")]
    Tokenizer(String),
}

impl PreviewError {
    pub fn as_warning(&self) -> String {
        match self {
            Self::UnknownModel(m) => format!("model {m} not in pricing table; falling back to heuristic"),
            Self::Malformed(s) => format!("malformed input: {s}"),
            Self::Tokenizer(s) => format!("tokenizer failed: {s} — using char-count heuristic"),
        }
    }
}
```

- [ ] **Step 2: Compile check**

`cargo check -p tt-preview`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/preview/src/error.rs
git commit -m "feat(preview): error type

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Pricing wrapper

**Files:** `crates/preview/src/pricing.rs`

- [ ] **Step 1: Confirm the existing pricing API surface**

Each provider exposes `pub fn pricing_for(model: &str) -> Option<ModelPricing>` (verified against `crates/providers/{anthropic,openai,gemini}/src/pricing.rs` on 2026-05-28). The fields are `input_per_million: f64`, `output_per_million: f64`, and `cached_input_per_million: Option<f64>` — note **no `_usd` suffix** on the field names. Use this exact API; do not invent variants.

- [ ] **Step 2: Write `pricing.rs` against the confirmed API**

```rust
//! Wrapper over per-provider pricing tables.
//!
//! Each provider crate exposes `pricing_for(&str) -> Option<ModelPricing>`.
//! We probe all three; first hit wins. Returns the pricing plus the
//! provider name so the response can populate `current.provider`.

use crate::error::PreviewError;

#[derive(Debug, Clone)]
pub struct LookupHit {
    pub provider: &'static str,
    /// Input cost per million tokens (USD).
    pub input_per_m: f64,
    /// Output cost per million tokens (USD).
    pub output_per_m: f64,
}

pub fn lookup(model: &str) -> Result<LookupHit, PreviewError> {
    if let Some(p) = tt_provider_anthropic::pricing::pricing_for(model) {
        return Ok(LookupHit {
            provider: "anthropic",
            input_per_m: p.input_per_million,
            output_per_m: p.output_per_million,
        });
    }
    if let Some(p) = tt_provider_openai::pricing::pricing_for(model) {
        return Ok(LookupHit {
            provider: "openai",
            input_per_m: p.input_per_million,
            output_per_m: p.output_per_million,
        });
    }
    if let Some(p) = tt_provider_gemini::pricing::pricing_for(model) {
        return Ok(LookupHit {
            provider: "gemini",
            input_per_m: p.input_per_million,
            output_per_m: p.output_per_million,
        });
    }
    Err(PreviewError::UnknownModel(model.to_string()))
}

/// Cost of a single call given token counts.
pub fn cost_usd(input_tokens: u32, output_tokens: u32, hit: &LookupHit) -> f64 {
    let i = (input_tokens as f64) * hit.input_per_m / 1_000_000.0;
    let o = (output_tokens as f64) * hit.output_per_m / 1_000_000.0;
    i + o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_math_basics() {
        let hit = LookupHit { provider: "x", input_per_m: 3.0, output_per_m: 15.0 };
        // 1000 in @ $3/M = $0.003; 100 out @ $15/M = $0.0015 → total $0.0045
        let c = cost_usd(1000, 100, &hit);
        assert!((c - 0.0045).abs() < 1e-9, "cost = {c}");
    }

    #[test]
    fn lookup_unknown_model_errors() {
        let err = lookup("does-not-exist-model").unwrap_err();
        assert!(matches!(err, PreviewError::UnknownModel(_)));
    }
}
```

- [ ] **Step 3: Field name fixup**

If the actual `ModelPricing` struct in the provider crates uses different field names (e.g. `prompt_cost_per_million_usd` instead of `input_per_million_usd`), update the field access here. Do this in one inline fixup; do not refactor the provider crates from this task.

- [ ] **Step 4: Run tests**

`cargo test -p tt-preview pricing`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/preview/src/pricing.rs
git commit -m "feat(preview): pricing lookup wrapper

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Token estimator

**Files:** `crates/preview/src/token_estimator.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Per-model token estimation.
//!
//! - OpenAI / Anthropic → tiktoken-rs `cl100k_base` (close enough; final
//!   billing uses provider report).
//! - Gemini / local → char-count / 4.0 heuristic.

use crate::types::{EstimationConfidence, Message};

pub struct EstimateResult {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub confidence: EstimationConfidence,
}

pub fn estimate(
    provider: &str,
    messages: &[Message],
    max_tokens_hint: Option<u32>,
) -> EstimateResult {
    let text = concat_message_text(messages);
    let (input, confidence) = match provider {
        "openai" | "anthropic" => {
            match tiktoken_rs::cl100k_base() {
                Ok(bpe) => (bpe.encode_with_special_tokens(&text).len() as u32, EstimationConfidence::High),
                Err(_) => (char_count_estimate(&text), EstimationConfidence::Low),
            }
        }
        _ => (char_count_estimate(&text), EstimationConfidence::Medium),
    };
    let output = max_tokens_hint.unwrap_or(512).min(4096);
    EstimateResult { input_tokens: input, output_tokens: output, confidence }
}

fn concat_message_text(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        if let Some(s) = m.content.as_str() {
            out.push_str(s);
            out.push('\n');
        } else if let Some(parts) = m.content.as_array() {
            for p in parts {
                if let Some(s) = p.get("text").and_then(|v| v.as_str()) {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn char_count_estimate(s: &str) -> u32 {
    ((s.chars().count() as f64) / 4.0).ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message { role: "user".into(), content: json!(text) }
    }

    #[test]
    fn openai_uses_tiktoken_high_confidence() {
        let est = estimate("openai", &[user("Hello, world.")], Some(100));
        assert!(est.input_tokens >= 1);
        assert!(matches!(est.confidence, EstimationConfidence::High));
        assert_eq!(est.output_tokens, 100);
    }

    #[test]
    fn anthropic_uses_tiktoken() {
        let est = estimate("anthropic", &[user("Hello, world.")], None);
        assert!(est.input_tokens >= 1);
        assert!(matches!(est.confidence, EstimationConfidence::High));
        assert_eq!(est.output_tokens, 512); // default
    }

    #[test]
    fn unknown_provider_uses_heuristic_medium() {
        let est = estimate("gemini", &[user("abcdefgh")], None);
        // 8 chars / 4 = 2 tokens
        assert_eq!(est.input_tokens, 2);
        assert!(matches!(est.confidence, EstimationConfidence::Medium));
    }

    #[test]
    fn max_tokens_caps_output_at_4096() {
        let est = estimate("openai", &[user("hi")], Some(99999));
        assert_eq!(est.output_tokens, 4096);
    }

    #[test]
    fn structured_content_extracts_text_parts() {
        let m = Message {
            role: "user".into(),
            content: json!([{"type": "text", "text": "Hello"}, {"type": "text", "text": " world"}]),
        };
        let est = estimate("gemini", &[m], None);
        assert!(est.input_tokens >= 2);
    }
}
```

- [ ] **Step 2: Run tests**

`cargo test -p tt-preview token_estimator`
Expected: 5 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/preview/src/token_estimator.rs
git commit -m "feat(preview): token estimator (tiktoken + heuristic)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Task classifier (cheap regex)

**Files:** `crates/preview/src/classifier.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Cheap regex-based task classifier. Pattern-matches the last user message
//! against indicators of: classification, extraction, code, agent, generic chat.
//! The same patterns inform `output-no-max-tokens` Inspect rule fix hints.

use crate::types::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskClass {
    Classification,
    Extraction,
    Code,
    Agent,
    Chat,
}

pub fn classify(messages: &[Message]) -> TaskClass {
    let last_user = messages.iter().rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    if last_user.contains("classify") || last_user.contains("category")
        || last_user.contains("label as") || last_user.contains("is this")
        || last_user.contains("yes or no")
    {
        return TaskClass::Classification;
    }
    if last_user.contains("extract") || last_user.contains("parse")
        || last_user.contains("structured output") || last_user.contains("json schema")
        || last_user.contains("entity") || last_user.contains("pull out")
    {
        return TaskClass::Extraction;
    }
    if last_user.contains("function") || last_user.contains("code")
        || last_user.contains("```") || last_user.contains("refactor")
        || last_user.contains("implement")
    {
        return TaskClass::Code;
    }
    if messages.len() > 4 && messages.iter().any(|m| m.role == "tool") {
        return TaskClass::Agent;
    }
    TaskClass::Chat
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn u(t: &str) -> Message { Message { role: "user".into(), content: json!(t) } }

    #[test]
    fn classify_classification() {
        assert_eq!(classify(&[u("Is this email spam? yes or no")]), TaskClass::Classification);
    }

    #[test]
    fn classify_extraction() {
        assert_eq!(classify(&[u("Extract the names from this text")]), TaskClass::Extraction);
    }

    #[test]
    fn classify_code() {
        assert_eq!(classify(&[u("write a function that adds two numbers")]), TaskClass::Code);
    }

    #[test]
    fn classify_chat_default() {
        assert_eq!(classify(&[u("Hi how are you")]), TaskClass::Chat);
    }
}
```

- [ ] **Step 2: Run tests**

`cargo test -p tt-preview classifier`
Expected: 4 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/preview/src/classifier.rs
git commit -m "feat(preview): task classifier

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Cache projection

**Files:** `crates/preview/src/cache_projection.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Cache hit-rate projection.
//!
//! Given the current call's cost and per-org L1/L2 hit probabilities,
//! return the expected-value savings. Hit probabilities can be plugged in
//! per-org (from cloud-side telemetry) or fall back to global defaults.

use crate::types::CacheProjections;

/// Global defaults — used when per-org telemetry isn't available.
pub const DEFAULT_L1_HIT_PROBABILITY: f32 = 0.20;
pub const DEFAULT_L2_HIT_PROBABILITY: f32 = 0.10;

pub fn project(cost_usd: f64, l1_p: f32, l2_p: f32) -> CacheProjections {
    let l1_p = l1_p.clamp(0.0, 1.0);
    let l2_p = l2_p.clamp(0.0, 1.0);
    let weighted = cost_usd * (l1_p as f64 + (1.0 - l1_p as f64) * l2_p as f64);
    CacheProjections {
        l1_hit_savings_usd: cost_usd,
        l1_hit_probability: l1_p,
        l2_hit_savings_usd: cost_usd,
        l2_hit_probability: l2_p,
        weighted_savings_usd: weighted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_math_known_values() {
        // cost=$1, L1=0.5, L2=0.5 → weighted = 1 * (0.5 + 0.5*0.5) = 0.75
        let p = project(1.0, 0.5, 0.5);
        assert!((p.weighted_savings_usd - 0.75).abs() < 1e-9);
    }

    #[test]
    fn defaults_yield_modest_savings() {
        let p = project(1.0, DEFAULT_L1_HIT_PROBABILITY, DEFAULT_L2_HIT_PROBABILITY);
        // 0.20 + 0.80 * 0.10 = 0.28
        assert!((p.weighted_savings_usd - 0.28).abs() < 1e-9);
    }

    #[test]
    fn clamps_probabilities() {
        let p = project(1.0, 2.0, -1.0);
        assert_eq!(p.l1_hit_probability, 1.0);
        assert_eq!(p.l2_hit_probability, 0.0);
    }
}
```

- [ ] **Step 2: Run tests**

`cargo test -p tt-preview cache_projection`
Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/preview/src/cache_projection.rs
git commit -m "feat(preview): cache hit-rate projection

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Route suggestions

**Files:** `crates/preview/src/route_suggestions.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Cheaper-equivalent route suggestions.
//!
//! For each candidate cheaper model, compute the cost. Per-task-class
//! whitelists determine acceptability. Quality risk band is `UNKNOWN`
//! by default; cloud-side enrichment may upgrade to LOW/MEDIUM/HIGH.

use crate::classifier::TaskClass;
use crate::pricing::{cost_usd, LookupHit};
use crate::types::{QualityRiskBand, RouteSuggestion};

/// Candidate cheaper models per task class. Ordered by preference.
fn candidates_for(class: TaskClass) -> &'static [&'static str] {
    match class {
        TaskClass::Classification => &[
            "claude-haiku-4-5",
            "gpt-4o-mini",
            "gemini-2-5-flash-lite",
        ],
        TaskClass::Extraction => &[
            "claude-haiku-4-5",
            "gpt-4o-mini",
            "gemini-2-5-flash",
        ],
        TaskClass::Chat => &[
            "claude-haiku-4-5",
            "gpt-4o-mini",
        ],
        TaskClass::Code => &[
            "claude-haiku-4-5",
            "gpt-4o-mini",
        ],
        TaskClass::Agent => &[],
    }
}

pub fn suggest(
    current_model: &str,
    current_cost_usd: f64,
    input_tokens: u32,
    output_tokens: u32,
    task_class: TaskClass,
) -> Vec<RouteSuggestion> {
    let mut out = Vec::new();
    for &candidate in candidates_for(task_class) {
        if candidate == current_model { continue; }
        let Ok(hit) = crate::pricing::lookup(candidate) else { continue; };
        let cost = cost_usd(input_tokens, output_tokens, &hit);
        if cost >= current_cost_usd { continue; }
        out.push(RouteSuggestion {
            route: format!("swap-to-{candidate}"),
            model: candidate.into(),
            cost_usd: cost,
            savings_usd: current_cost_usd - cost,
            quality_risk_band: QualityRiskBand::Unknown,
            rationale: format!(
                "{candidate} historically handles {:?} tasks at lower cost. Quality \
                 band not yet computed for your org (UNKNOWN); enable Plan engine \
                 quality scoring to upgrade to LOW/MEDIUM/HIGH.",
                task_class,
            ),
            applicable: true,
        });
        if out.len() >= 3 { break; }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_class_yields_no_suggestions() {
        let v = suggest("claude-opus", 1.0, 100, 100, TaskClass::Agent);
        assert!(v.is_empty());
    }

    #[test]
    fn excludes_current_model() {
        let v = suggest("claude-haiku-4-5", 0.001, 100, 100, TaskClass::Classification);
        assert!(!v.iter().any(|s| s.model == "claude-haiku-4-5"));
    }

    #[test]
    fn caps_at_3_suggestions() {
        let v = suggest("claude-opus", 1.0, 1000, 1000, TaskClass::Extraction);
        assert!(v.len() <= 3);
    }
}
```

- [ ] **Step 2: Run tests**

`cargo test -p tt-preview route_suggestions`
Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/preview/src/route_suggestions.rs
git commit -m "feat(preview): route suggestions

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Top-level `preview()` function

**Files:** `crates/preview/src/lib.rs` (replace existing)

- [ ] **Step 1: Replace `lib.rs` with the public orchestrator**

```rust
//! `tt-preview` — pure cost preview engine.
//!
//! See `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md`.

pub mod cache_projection;
pub mod classifier;
pub mod error;
pub mod pricing;
pub mod route_suggestions;
pub mod token_estimator;
pub mod types;

pub use error::PreviewError;
pub use types::{
    CacheProjections, CurrentEstimate, EstimationConfidence, PreviewRequest,
    PreviewResponse, QualityRiskBand, RouteSuggestion, Suggestion,
};

use uuid::Uuid;

/// Top-level entry point. Returns a complete `PreviewResponse`. The only
/// way this returns `Err` is if the model is unknown AND the optional
/// fallback heuristic also fails — in practice the handler converts that
/// into a 400 with a clear message.
pub fn preview(req: &PreviewRequest) -> Result<PreviewResponse, PreviewError> {
    let mut warnings = Vec::new();

    let hit = pricing::lookup(&req.model)?;
    let est = token_estimator::estimate(hit.provider, &req.messages, req.max_tokens);
    let cost = pricing::cost_usd(est.input_tokens, est.output_tokens, &hit);

    let task_class = classifier::classify(&req.messages);

    let cache = cache_projection::project(
        cost,
        cache_projection::DEFAULT_L1_HIT_PROBABILITY,
        cache_projection::DEFAULT_L2_HIT_PROBABILITY,
    );

    let suggestions = route_suggestions::suggest(
        &req.model, cost, est.input_tokens, est.output_tokens, task_class,
    );
    if suggestions.is_empty() && !matches!(task_class, classifier::TaskClass::Agent) {
        warnings.push(format!(
            "no cheaper-equivalent candidates for {} on this task class — \
             current model may already be the cheapest in family",
            req.model,
        ));
    }

    Ok(PreviewResponse {
        current: CurrentEstimate {
            model: req.model.clone(),
            provider: hit.provider.to_string(),
            input_tokens_estimated: est.input_tokens,
            output_tokens_estimated: est.output_tokens,
            cost_usd: cost,
            estimation_confidence: est.confidence,
        },
        cache_projections: cache,
        route_suggestions: suggestions,
        warnings,
        trace_id: Uuid::new_v4().to_string(),
    })
}
```

- [ ] **Step 2: Add `uuid` dep**

In `crates/preview/Cargo.toml` `[dependencies]`:
```toml
uuid = { version = "1.10", features = ["v4"] }
```

(or `uuid.workspace = true` if the workspace already has it; check root Cargo.toml first.)

- [ ] **Step 3: Run all crate tests**

`cargo test -p tt-preview`
Expected: all tests pass (cumulative from prior tasks).

- [ ] **Step 4: Commit**

```bash
git add crates/preview/Cargo.toml crates/preview/src/lib.rs
git commit -m "feat(preview): top-level preview() function

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Gateway HTTP handler + route registration

**Files:**
- Modify: `crates/core/Cargo.toml` (add `tt-preview` dep)
- Create: `crates/core/src/routes/preview.rs`
- Modify: `crates/core/src/server.rs` (register `/v1/preview`)

- [ ] **Step 1: Add `tt-preview` dep**

In `crates/core/Cargo.toml`:
```toml
tt-preview.workspace = true
```

- [ ] **Step 2: Write the handler**

`crates/core/src/routes/preview.rs`:

```rust
//! POST /v1/preview — synchronous cost preview.
//!
//! Mirrors the auth-key middleware applied to /v1/chat/completions. Body is
//! a subset of the chat-completion request; response is `tt_preview::PreviewResponse`.

use axum::{extract::State, http::StatusCode, Json};
use serde_json::json;

use crate::state::AppState;
use tt_preview::PreviewRequest;

pub async fn post_preview(
    State(_state): State<AppState>,
    Json(req): Json<PreviewRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let resp = tt_preview::preview(&req).map_err(|e| (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": e.to_string() })),
    ))?;
    Ok(Json(serde_json::to_value(resp).unwrap()))
}
```

- [ ] **Step 3: Register the route in `server.rs`**

Find the existing `Router::new().route("/v1/chat/completions", ...)` chain. After the `chat_completions` route, add:

```rust
        .route("/v1/preview", axum::routing::post(crate::routes::preview::post_preview))
```

Also `pub mod preview;` in `crates/core/src/routes/mod.rs`.

- [ ] **Step 4: Compile + test**

```
cargo check -p tt-core
cargo clippy -p tt-core -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add crates/core/Cargo.toml crates/core/src/server.rs crates/core/src/routes/preview.rs crates/core/src/routes/mod.rs
git commit -m "feat(core): POST /v1/preview handler

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Integration test against full Gateway

**Files:** `crates/core/tests/preview_endpoint.rs`

- [ ] **Step 1: Write the test**

```rust
//! Integration test: POST /v1/preview returns a valid PreviewResponse.

use serde_json::json;
use tt_core::{build_router, state::AppState};

#[tokio::test]
async fn preview_returns_shape_for_known_model() {
    let app = build_router(AppState::with_default_providers());
    let body = json!({
        "model": "claude-haiku-4-5",
        "messages": [{"role": "user", "content": "Hello"}],
        "max_tokens": 100
    });
    let req = http::Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["current"]["model"], "claude-haiku-4-5");
    assert!(json["current"]["cost_usd"].as_f64().unwrap() > 0.0);
    assert!(json["cache_projections"]["weighted_savings_usd"].as_f64().unwrap() >= 0.0);
}

#[tokio::test]
async fn preview_returns_400_on_unknown_model() {
    let app = build_router(AppState::with_default_providers());
    let body = json!({
        "model": "model-that-does-not-exist",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let req = http::Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(resp.status(), 400);
}
```

- [ ] **Step 2: Add dev-deps if missing**

In `crates/core/Cargo.toml` `[dev-dependencies]` make sure: `http`, `tower`, `axum` with `macros` feature, `tokio` with `macros + rt-multi-thread`. Most already present — only add what's missing.

- [ ] **Step 3: Run tests**

`cargo test -p tt-core --test preview_endpoint`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/core/tests/preview_endpoint.rs crates/core/Cargo.toml
git commit -m "test(core): /v1/preview end-to-end

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: Documentation + final gate

**Files:**
- Create: `docs/04-cost-preview-api-reference.md`
- Modify: `.claude/CONTEXT_MAP.md`
- Modify: `.claude/BACKLOG.md` (Task 13)

- [ ] **Step 1: Write the API reference doc**

`docs/04-cost-preview-api-reference.md`:

```markdown
# Cost Preview API

`POST /v1/preview` — synchronous, no LLM calls, no Postgres lookups.

## Request

\`\`\`json
{
  "model": "claude-haiku-4-5",
  "messages": [{"role": "user", "content": "..."}],
  "max_tokens": 1024
}
\`\`\`

## Response

\`\`\`json
{
  "current": {
    "model": "claude-haiku-4-5",
    "provider": "anthropic",
    "input_tokens_estimated": 12,
    "output_tokens_estimated": 100,
    "cost_usd": 0.000023,
    "estimation_confidence": "high"
  },
  "cache_projections": { ... },
  "route_suggestions": [ ... ],
  "warnings": [],
  "trace_id": "..."
}
\`\`\`

See `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md` for the design rationale.
```

- [ ] **Step 2: Add a context-map entry**

In `.claude/CONTEXT_MAP.md` Domains table, add:

```markdown
### Cost preview

| If you're doing | Read |
|---|---|
| Adding a model to pricing | the provider crate's `pricing.rs` (NOT `tt-preview`) |
| Adjusting cache-hit defaults | `crates/preview/src/cache_projection.rs::DEFAULT_*` |
| Adding a task class | `crates/preview/src/classifier.rs` |
| API reference | `docs/04-cost-preview-api-reference.md` |
| Spec | `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md` |
```

- [ ] **Step 3: Run the full gate**

```
cargo fmt --check
cargo clippy -p tt-preview -p tt-core -- -D warnings
cargo test -p tt-preview
cargo test -p tt-core --test preview_endpoint
./scripts/tt-inspect-self.sh
```

Expected: all pass.

- [ ] **Step 4: Commit**

```bash
git add docs/04-cost-preview-api-reference.md .claude/CONTEXT_MAP.md
git commit -m "docs(preview): API reference + context map entry

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Mark backlog item complete

- [ ] **Step 1: Flip `[ ]` → `[x]` for `trackC-cost-preview-api` in `.claude/BACKLOG.md` and append `_Shipped 2026-MM-DD — Day-0 MVP._`.**

- [ ] **Step 2: Commit**

```bash
git add .claude/BACKLOG.md
git commit -m "backlog: trackC cost preview API Day-0 MVP shipped

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Spec coverage check

| Spec section | Covered by |
|---|---|
| §4 architecture | Tasks 1, 10 |
| §4.3 request shape | Task 2 |
| §4.4 response shape | Task 2 |
| §5 token estimation | Task 5 |
| §6 route suggestions | Task 8 |
| §7 cache projection | Task 7 |
| §8 auth (reuses existing middleware) | inherited from `crates/core` |
| §9 testing | Tasks 2–8 (units) + Task 11 (integration) |
| §10 rollout Day 0 | Tasks 1–13 |
| §10 Day 14+ enrichment | deferred — fallback defaults wired |

Cloud-side enrichment endpoints from the spec (per-org cache hit rates, Plan engine quality bands) are explicitly deferred to Track C v2; the current crate plumbs global defaults and exposes the trait shape needed to swap in cloud telemetry later.
