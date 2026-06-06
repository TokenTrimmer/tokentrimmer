# `temperature_clamped` warning (G-B3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clamp `temperature` to the routed provider's valid range before dispatch and emit `X-TokenTrimmer-Warnings: temperature_clamped`.

**Architecture:** Additive `Provider::temperature_range() -> (f32, f32)` default `(0.0, 2.0)`, overridden `(0.0, 1.0)` on Anthropic. A gateway pre-dispatch helper `maybe_clamp_temperature` (run right after B2's `maybe_downgrade_response_format`) mutates `req.temperature` and pushes `temperature_clamped` onto the existing `warnings` Vec; B2 already wired `attach_warnings`'s `extra` merge.

**Tech Stack:** Rust, `async_trait`, axum, the `crates/core/tests` MockProvider harness.

---

### Task 1: `Provider::temperature_range` trait method + Anthropic override

**Files:**
- Modify: `crates/shared/src/provider.rs` (after `supports_response_schema`)
- Modify: `crates/providers/anthropic/src/lib.rs` (`impl Provider for AnthropicProvider`, after `supports_response_schema` `:127`)
- Modify: `crates/providers/anthropic/tests/translate.rs` + `crates/providers/gemini/tests/translate.rs` (unit tests)

- [ ] **Step 1: Add the default method**

In `crates/shared/src/provider.rs`, after the `supports_response_schema` default method, add:
```rust
    /// The provider's accepted `temperature` range `(min, max)`. The gateway
    /// clamps an out-of-range request value to this and emits
    /// `temperature_clamped`. Default `(0.0, 2.0)` — the widest common range
    /// (OpenAI/Gemini). Override only with a narrower range you are confident is
    /// correct, so the gateway never wrongly tightens a provider whose true max
    /// is uncertain.
    fn temperature_range(&self) -> (f32, f32) {
        (0.0, 2.0)
    }
```

- [ ] **Step 2: Override on Anthropic**

In `crates/providers/anthropic/src/lib.rs`, in `impl Provider for AnthropicProvider`, after the `supports_response_schema` method (`:127`), add:
```rust
    fn temperature_range(&self) -> (f32, f32) {
        // Anthropic rejects temperature > 1.0.
        (0.0, 1.0)
    }
```

- [ ] **Step 3: Unit tests**

Append to `crates/providers/anthropic/tests/translate.rs`:
```rust
#[test]
fn anthropic_temperature_range_is_zero_to_one() {
    use tt_provider_anthropic::{AnthropicProvider, ClientConfig};
    use tt_shared::Provider;
    assert_eq!(
        AnthropicProvider::new(ClientConfig::default()).temperature_range(),
        (0.0, 1.0)
    );
}
```
Append to `crates/providers/gemini/tests/translate.rs` (default range holds for a non-overriding adapter):
```rust
#[test]
fn gemini_temperature_range_is_default_wide() {
    use tt_provider_gemini::{ClientConfig, GeminiProvider};
    use tt_shared::Provider;
    assert_eq!(
        GeminiProvider::new(ClientConfig::default()).temperature_range(),
        (0.0, 2.0)
    );
}
```

- [ ] **Step 4: Run unit tests**

Run: `cargo test -p tt-provider-anthropic anthropic_temperature_range 2>&1 | tail -6`
Run: `cargo test -p tt-provider-gemini gemini_temperature_range 2>&1 | tail -6`
Expected: both PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shared/src/provider.rs crates/providers/anthropic crates/providers/gemini
git commit -m "feat(providers): Provider::temperature_range (Anthropic 0..1, default 0..2)"
```

---

### Task 2: Gateway clamp + integration tests

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add `maybe_clamp_temperature`; call after the downgrade at `:692`)
- Create: `crates/core/tests/temperature_clamped.rs`

- [ ] **Step 1: Write the failing integration test**

Create `crates/core/tests/temperature_clamped.rs`:

```rust
//! `X-TokenTrimmer-Warnings: temperature_clamped` + temperature clamping.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};

struct TempMock {
    range: (f32, f32),
    drops_temp: bool,
    seen: Arc<Mutex<Option<f32>>>,
}

#[async_trait]
impl Provider for TempMock {
    fn id(&self) -> &'static str {
        "tempmock"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "tm-1".into(),
            provider: "tempmock".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.1,
            output_per_million: 0.1,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: Utc::now(),
        })
    }
    fn temperature_range(&self) -> (f32, f32) {
        self.range
    }
    fn dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String> {
        if self.drops_temp && req.temperature.is_some() {
            vec!["temperature".to_string()]
        } else {
            Vec::new()
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        *self.seen.lock().unwrap() = req.temperature;
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("hi".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Err(ProviderError::Unsupported("no stream".into()))
    }
}

fn app_with(mock: TempMock) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(mock));
    build_router(AppState::new(registry))
}

fn req_with_temp(temp: f64) -> String {
    json!({ "model": "tm-1", "messages": [{"role":"user","content":"hi"}], "temperature": temp })
        .to_string()
}

async fn post(app: axum::Router, body: String) -> axum::http::Response<Body> {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn clamps_out_of_range_temperature() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(TempMock {
            range: (0.0, 1.0),
            drops_temp: false,
            seen: Arc::clone(&seen),
        }),
        req_with_temp(1.5),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(warn.contains("temperature_clamped"), "got: {warn}");
    let t = seen.lock().unwrap().expect("temperature seen");
    assert!((t - 1.0).abs() < 1e-6, "expected clamped to 1.0, got {t}");
}

#[tokio::test]
async fn in_range_temperature_not_clamped() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(TempMock {
            range: (0.0, 1.0),
            drops_temp: false,
            seen: Arc::clone(&seen),
        }),
        req_with_temp(0.5),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-tokentrimmer-warnings").is_none());
    let t = seen.lock().unwrap().expect("temperature seen");
    assert!((t - 0.5).abs() < 1e-6, "expected unchanged 0.5, got {t}");
}

#[tokio::test]
async fn dropped_temperature_is_not_clamped() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(TempMock {
            range: (0.0, 1.0),
            drops_temp: true,
            seen: Arc::clone(&seen),
        }),
        req_with_temp(1.5),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(warn.contains("param_dropped:temperature"), "got: {warn}");
    assert!(!warn.contains("temperature_clamped"), "got: {warn}");
}
```

- [ ] **Step 2: Run — expect FAIL (no clamp yet)**

Run: `cargo test -p tt-core --test temperature_clamped 2>&1 | tail -25`
Expected: `clamps_out_of_range_temperature` FAILS — no `temperature_clamped` header and the mock saw `1.5` (not clamped). The other two may already pass.

- [ ] **Step 3: Add the `maybe_clamp_temperature` helper**

In `crates/core/src/routes/chat.rs`, immediately after the `maybe_downgrade_response_format` function definition (it ends near `:1530`), add:
```rust
/// Clamp `req.temperature` to the routed provider's accepted range, recording a
/// `temperature_clamped` warning when the value actually changed. Skips a
/// temperature that the provider drops outright (reasoning models — B1
/// param_dropped) so the two warnings don't both fire.
fn maybe_clamp_temperature(
    req: &mut ChatCompletionRequest,
    provider: &dyn tt_shared::Provider,
    warnings: &mut Vec<String>,
) {
    let Some(t) = req.temperature else {
        return;
    };
    if provider
        .dropped_params(req)
        .iter()
        .any(|p| p == "temperature")
    {
        return;
    }
    let (lo, hi) = provider.temperature_range();
    let clamped = t.clamp(lo, hi);
    if (clamped - t).abs() > f32::EPSILON {
        req.temperature = Some(clamped);
        warnings.push("temperature_clamped".to_string());
    }
}
```

- [ ] **Step 4: Call it after the downgrade**

In the handler, immediately after the existing downgrade call (`chat.rs:692`):
```rust
    maybe_downgrade_response_format(&mut req, provider.as_ref(), &mut warnings);
```
add:
```rust
    maybe_clamp_temperature(&mut req, provider.as_ref(), &mut warnings);
```

- [ ] **Step 5: Run — expect PASS**

Run: `cargo test -p tt-core --test temperature_clamped 2>&1 | tail -15`
Run: `cargo test -p tt-core --test warnings_header --test response_format_downgrade 2>&1 | tail -10`
Expected: all pass (3 new + B1's 3 + B2's 3).

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/tests/temperature_clamped.rs
git commit -m "feat(core): clamp temperature to provider range + temperature_clamped warning"
```

---

### Task 3: Docs + gates

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (`:151` clamping line; the `X-TokenTrimmer-Warnings` prose)

- [ ] **Step 1: Update docs**

At `docs/04-gateway-api-reference.md:151`, change the `_(Planned)_` clamping line to honored:
```markdown
- Parameters with different ranges across providers (e.g., temperature) are clamped to the provider's valid range, with a `temperature_clamped` warning (e.g. Anthropic caps `temperature` at 1.0).
```
In the `X-TokenTrimmer-Warnings` prose paragraph (near `:440`), change the trailing "The `temperature_clamped` token is a planned follow-up." to:
```markdown
A `temperature_clamped` token is emitted when the request's `temperature` is clamped to the routed provider's accepted range (e.g. a `1.5` request to Anthropic, whose max is `1.0`).
```

- [ ] **Step 2: Workspace tests**

Run: `cargo test -p tt-shared -p tt-provider-anthropic -p tt-provider-gemini -p tt-core 2>&1 | grep -E "test result:|error\[|FAILED" | tail -10`
Expected: all pass.

- [ ] **Step 3: Clippy + fmt**

Run: `cargo clippy -p tt-shared -p tt-provider-anthropic -p tt-provider-gemini -p tt-core --all-targets -- -D warnings 2>&1 | grep -E "^warning:|^error" | grep -v "Permission denied\|auto-clean" | tail -8`
Expected: clean.

Run: `cargo fmt && cargo fmt -- --check 2>&1 | tail -3`
Expected: clean.

- [ ] **Step 4: Commit docs (+ any fmt)**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs(gateway): temperature_clamped is now honored"
git diff --quiet || (git add -A && git commit -m "style: cargo fmt")
```

- [ ] **Step 5: Confirm scope**

Run: `git diff main --stat`
Expected: `crates/shared/src/provider.rs`, `crates/providers/anthropic/src/lib.rs`, the two adapter test files, `crates/core/src/routes/chat.rs`, `crates/core/tests/temperature_clamped.rs`, `docs/04-gateway-api-reference.md` (+ spec/plan docs).
