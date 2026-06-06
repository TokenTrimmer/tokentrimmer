# `response_format_downgrade` warning (G-B2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Downgrade `response_format: json_schema` → `json_object` before dispatch when the routed provider lacks schema support, and emit `X-TokenTrimmer-Warnings: response_format_downgrade`.

**Architecture:** Additive `Provider::supports_response_schema() -> bool` default `false`, overridden `true` on OpenAI + Gemini. A gateway pre-dispatch helper mutates `req.response_format` and pushes a token onto a `warnings: Vec<String>` (declared before the stream/non-stream branch); `attach_warnings` gains an `extra: &[String]` param to merge those pre-dispatch tokens with B1's model-dependent `param_dropped:*`.

**Tech Stack:** Rust, `async_trait`, axum, the `crates/core/tests` MockProvider harness.

---

### Task 1: `Provider::supports_response_schema` trait method + OpenAI/Gemini overrides

**Files:**
- Modify: `crates/shared/src/provider.rs` (after `dropped_params`)
- Modify: `crates/providers/openai/src/lib.rs` (`impl Provider for OpenAiProvider`, after its `dropped_params`)
- Modify: `crates/providers/gemini/src/lib.rs` (`impl Provider for GeminiProvider`, after its `dropped_params`)
- Modify: `crates/providers/openai/tests/` or `gemini/tests/` — covered by integration in Task 2; a focused unit test here too

- [ ] **Step 1: Add the default method**

In `crates/shared/src/provider.rs`, after the `dropped_params` default method, add:
```rust
    /// Whether this provider faithfully honors `response_format: json_schema`
    /// (structured outputs). Default `false`: the gateway downgrades to
    /// `json_object` (with a `response_format_downgrade` warning) for providers
    /// that don't. Override `true` only where schema mode genuinely works.
    fn supports_response_schema(&self) -> bool {
        false
    }
```

- [ ] **Step 2: Override `true` on OpenAI-native**

In `crates/providers/openai/src/lib.rs`, in `impl Provider for OpenAiProvider`, immediately after the `dropped_params` method added in B1, add:
```rust
    fn supports_response_schema(&self) -> bool {
        true
    }
```

- [ ] **Step 3: Override `true` on Gemini**

In `crates/providers/gemini/src/lib.rs`, in `impl Provider for GeminiProvider`, after its `dropped_params` method, add:
```rust
    fn supports_response_schema(&self) -> bool {
        true
    }
```

- [ ] **Step 4: Unit test (Gemini supports, default does not)**

Append to `crates/providers/gemini/tests/translate.rs`:
```rust
#[test]
fn gemini_supports_response_schema() {
    use tt_provider_gemini::{ClientConfig, GeminiProvider};
    use tt_shared::Provider;
    assert!(GeminiProvider::new(ClientConfig::default()).supports_response_schema());
}
```
Append to `crates/providers/anthropic/tests/translate.rs` (default `false` holds for a non-overriding adapter):
```rust
#[test]
fn anthropic_does_not_support_response_schema() {
    use tt_provider_anthropic::{AnthropicProvider, ClientConfig};
    use tt_shared::Provider;
    assert!(!AnthropicProvider::new(ClientConfig::default()).supports_response_schema());
}
```

- [ ] **Step 5: Run unit tests**

Run: `cargo test -p tt-provider-gemini gemini_supports_response_schema 2>&1 | tail -8`
Run: `cargo test -p tt-provider-anthropic anthropic_does_not_support_response_schema 2>&1 | tail -8`
Expected: both PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/shared/src/provider.rs crates/providers/openai crates/providers/gemini crates/providers/anthropic
git commit -m "feat(providers): Provider::supports_response_schema (OpenAI+Gemini=true)"
```

---

### Task 2: Gateway downgrade + warnings-channel merge

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add `maybe_downgrade_response_format` helper; declare `warnings` after the cost-limit block `:687`; extend `attach_warnings` with `extra`; pass `&warnings` at both attach sites `:953`/`:1478`)
- Create: `crates/core/tests/response_format_downgrade.rs`

- [ ] **Step 1: Write the failing integration test**

Create `crates/core/tests/response_format_downgrade.rs`. It uses a `MockProvider` that records the `response_format` it receives and exposes `supports_response_schema`/`dropped_params` knobs:

```rust
//! `X-TokenTrimmer-Warnings: response_format_downgrade` + json_schema→json_object rewrite.

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
    messages::{Choice, Message, MessageContent, ResponseFormat},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};

struct RfMock {
    schema: bool,
    drops_rf: bool,
    seen: Arc<Mutex<Option<ResponseFormat>>>,
}

#[async_trait]
impl Provider for RfMock {
    fn id(&self) -> &'static str {
        "rfmock"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "rf-1".into(),
            provider: "rfmock".into(),
            capabilities: vec![Capability::Text, Capability::JsonMode],
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
    fn supports_response_schema(&self) -> bool {
        self.schema
    }
    fn dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String> {
        if self.drops_rf && req.response_format.is_some() {
            vec!["response_format".to_string()]
        } else {
            Vec::new()
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        *self.seen.lock().unwrap() = req.response_format.clone();
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

fn app_with(mock: RfMock) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(mock));
    build_router(AppState::new(registry))
}

fn schema_req() -> String {
    json!({
        "model": "rf-1",
        "messages": [{"role":"user","content":"hi"}],
        "response_format": {"type":"json_schema","json_schema":{"name":"x","schema":{"type":"object"}}}
    })
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
async fn downgrades_schema_for_non_schema_provider() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(RfMock {
            schema: false,
            drops_rf: false,
            seen: Arc::clone(&seen),
        }),
        schema_req(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(warn.contains("response_format_downgrade"), "got: {warn}");
    // The dispatched request was rewritten to json_object with no schema.
    let rf = seen.lock().unwrap().clone().expect("response_format seen");
    assert_eq!(rf.r#type, "json_object");
    assert!(rf.json_schema.is_none());
}

#[tokio::test]
async fn no_downgrade_for_schema_capable_provider() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(RfMock {
            schema: true,
            drops_rf: false,
            seen: Arc::clone(&seen),
        }),
        schema_req(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-tokentrimmer-warnings").is_none());
    let rf = seen.lock().unwrap().clone().expect("response_format seen");
    assert_eq!(rf.r#type, "json_schema");
}

#[tokio::test]
async fn dropped_response_format_is_not_downgraded() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(RfMock {
            schema: false,
            drops_rf: true,
            seen: Arc::clone(&seen),
        }),
        schema_req(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(warn.contains("param_dropped:response_format"), "got: {warn}");
    assert!(!warn.contains("response_format_downgrade"), "got: {warn}");
}
```

- [ ] **Step 2: Run — expect FAIL (no downgrade yet)**

Run: `cargo test -p tt-core --test response_format_downgrade 2>&1 | tail -25`
Expected: `downgrades_schema_for_non_schema_provider` FAILS — no `response_format_downgrade` header, and the mock saw `json_schema` (not rewritten). The other two may already pass.

- [ ] **Step 3: Add the `maybe_downgrade_response_format` helper**

In `crates/core/src/routes/chat.rs`, near `attach_warnings` (above `build_hit_l1_response`), add:
```rust
/// If `req` asks for `response_format: json_schema` but the routed provider
/// supports only `json_object`, rewrite it to `json_object` (dropping the
/// schema) and record a `response_format_downgrade` warning. Providers that
/// drop `response_format` outright (Anthropic) are left to B1's param_dropped.
fn maybe_downgrade_response_format(
    req: &mut ChatCompletionRequest,
    provider: &dyn tt_shared::Provider,
    warnings: &mut Vec<String>,
) {
    let is_schema = req
        .response_format
        .as_ref()
        .is_some_and(|rf| rf.r#type == "json_schema");
    if !is_schema || provider.supports_response_schema() {
        return;
    }
    if provider
        .dropped_params(req)
        .iter()
        .any(|p| p == "response_format")
    {
        return;
    }
    req.response_format = Some(tt_shared::messages::ResponseFormat {
        r#type: "json_object".to_string(),
        json_schema: None,
    });
    warnings.push("response_format_downgrade".to_string());
}
```

- [ ] **Step 4: Extend `attach_warnings` with an `extra` parameter**

Change the `attach_warnings` signature + body to merge pre-dispatch tokens:
```rust
fn attach_warnings(
    headers: &mut axum::http::HeaderMap,
    provider: &dyn tt_shared::Provider,
    req: &ChatCompletionRequest,
    served_model: &str,
    extra: &[String],
) {
    let dropped = if req.model == served_model {
        provider.dropped_params(req)
    } else {
        let mut served = req.clone();
        served.model = served_model.to_string();
        provider.dropped_params(&served)
    };
    let mut tokens: Vec<String> = dropped
        .into_iter()
        .map(|p| format!("param_dropped:{p}"))
        .collect();
    tokens.extend(extra.iter().cloned());
    if tokens.is_empty() {
        return;
    }
    if let Ok(v) = tokens.join(",").parse() {
        headers.insert("x-tokentrimmer-warnings", v);
    }
}
```

- [ ] **Step 5: Declare `warnings` + run the downgrade after the cost-limit block**

In the handler, immediately after the `X-TokenTrimmer-Cost-Limit-Usd` enforcement block (the block ending with `?;\n    }` at `:687`, right before the `// For a failover chain` comment at `:688`), insert:
```rust
    // Normalize the request for the routed provider and collect any pre-dispatch
    // warnings (B2: response_format_downgrade; B3 will add temperature_clamped).
    let mut warnings: Vec<String> = Vec::new();
    maybe_downgrade_response_format(&mut req, provider.as_ref(), &mut warnings);
```

- [ ] **Step 6: Pass `&warnings` at both attach sites**

Streaming-live (`:953`):
```rust
        attach_warnings(resp.headers_mut(), provider.as_ref(), &req, &served_model, &warnings);
```
Non-stream miss (`:1478`):
```rust
        attach_warnings(http_response.headers_mut(), provider.as_ref(), &req, &model_used, &warnings);
```

- [ ] **Step 7: Run — expect PASS**

Run: `cargo test -p tt-core --test response_format_downgrade 2>&1 | tail -15`
Run: `cargo test -p tt-core --test warnings_header 2>&1 | tail -10`
Expected: all pass (the new file's 3 tests + B1's 3 tests still green — `attach_warnings` callers in `warnings_header.rs` are integration-level and unaffected by the signature change; only the in-crate call sites needed updating).

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/tests/response_format_downgrade.rs
git commit -m "feat(core): downgrade json_schema->json_object + response_format_downgrade warning"
```

---

### Task 3: Docs + gates

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (`:300` downgrade note; the `X-TokenTrimmer-Warnings` prose near `:430`)

- [ ] **Step 1: Update docs**

At `docs/04-gateway-api-reference.md:300`, change the `_(Planned)_` downgrade line to honored:
```markdown
If routed to a provider that doesn't support schema mode, Gateway rewrites `response_format` to `json_object` (dropping the schema) and emits `X-TokenTrimmer-Warnings: response_format_downgrade`.
```
In the `X-TokenTrimmer-Warnings` prose paragraph (added in B1, near `:430`), change "The `response_format_downgrade` and `temperature_clamped` tokens are planned follow-ups." to:
```markdown
A `response_format_downgrade` token is emitted when a `json_schema` request is routed to a provider that supports only `json_object`. The `temperature_clamped` token is a planned follow-up.
```

- [ ] **Step 2: Workspace tests**

Run: `cargo test -p tt-shared -p tt-provider-openai -p tt-provider-gemini -p tt-provider-anthropic -p tt-core 2>&1 | grep -E "test result:|error\[|FAILED" | tail -10`
Expected: all pass.

- [ ] **Step 3: Clippy + fmt**

Run: `cargo clippy -p tt-shared -p tt-provider-openai -p tt-provider-gemini -p tt-core --all-targets -- -D warnings 2>&1 | grep -E "warning:|error" | grep -v "Permission denied\|auto-clean" | tail -10`
Expected: clean.

Run: `cargo fmt && cargo fmt -- --check 2>&1 | tail -3`
Expected: clean.

- [ ] **Step 4: Commit docs (+ any fmt)**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs(gateway): response_format_downgrade is now honored"
git diff --quiet || (git add -A && git commit -m "style: cargo fmt")
```

- [ ] **Step 5: Confirm scope**

Run: `git diff main --stat`
Expected: `crates/shared/src/provider.rs`, `crates/providers/{openai,gemini}/src/lib.rs`, the two adapter test files, `crates/core/src/routes/chat.rs`, `crates/core/tests/response_format_downgrade.rs`, `docs/04-gateway-api-reference.md` (+ spec/plan docs).
