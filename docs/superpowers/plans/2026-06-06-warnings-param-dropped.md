# `X-TokenTrimmer-Warnings: param_dropped` + channel (G-B1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Emit `X-TokenTrimmer-Warnings: param_dropped:<name>` on dispatch responses for params the provider adapters silently drop, and stand up the warnings channel B2/B3 will extend.

**Architecture:** Add an additive default trait method `Provider::dropped_params(&req) -> Vec<String>` (empty by default), overridden in the adapters that drop (Anthropic, Gemini, OpenAI-native, compat + its 4 wrappers). The gateway computes tokens from `(provider, req)` and attaches the header on the two dispatch return paths (non-stream miss, streaming live) via a small helper; cache hits emit nothing (no translation occurred). Docs flip the header row to Honored.

**Tech Stack:** Rust, `async_trait`, axum, the `crates/core/tests` httpmock/MockProvider harness.

---

### Task 1: Add `Provider::dropped_params` default trait method

**Files:**
- Modify: `crates/shared/src/provider.rs` (after `fee_multiplier`, `:31-33`)

- [ ] **Step 1: Add the default method**

In `crates/shared/src/provider.rs`, inside `trait Provider`, after the `fee_multiplier` default method (`:33`), add:

```rust
    /// Names of request params this adapter **silently drops** for `req`
    /// during translation because the upstream provider rejects them. The
    /// gateway emits each as `X-TokenTrimmer-Warnings: param_dropped:<name>`.
    /// Default: nothing dropped.
    fn dropped_params(&self, _req: &ChatCompletionRequest) -> Vec<String> {
        Vec::new()
    }
```

(`ChatCompletionRequest` is already imported at `:8`.)

- [ ] **Step 2: Build**

Run: `cargo build -p tt-shared 2>&1 | tail -5`
Expected: compiles (default method, no other change).

- [ ] **Step 3: Commit**

```bash
git add crates/shared/src/provider.rs
git commit -m "feat(shared): add Provider::dropped_params default trait method"
```

---

### Task 2: Anthropic `dropped_params` override

**Files:**
- Modify: `crates/providers/anthropic/src/lib.rs` (the `impl Provider for AnthropicProvider` block, `:93`)

- [ ] **Step 1: Write the failing unit test**

In `crates/providers/anthropic/src/lib.rs`'s test module (`#[cfg(test)] mod tests`), add (if the module imports differ, mirror the file's existing test imports):

```rust
    #[test]
    fn dropped_params_reports_present_openai_only_fields() {
        let provider = AnthropicProvider::new(Default::default());
        let mut req = tt_shared::ChatCompletionRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![],
            temperature: None,
            top_p: None,
            max_tokens: None,
            stream: false,
            tools: vec![],
            tool_choice: None,
            response_format: Some(tt_shared::messages::ResponseFormat {
                r#type: "json_object".into(),
                json_schema: None,
            }),
            stop: vec![],
            presence_penalty: Some(0.1),
            frequency_penalty: None,
            n: Some(2),
            seed: None,
            user: None,
            tt_extras: std::collections::HashMap::new(),
        };
        let mut got = provider.dropped_params(&req);
        got.sort();
        assert_eq!(got, vec!["n", "presence_penalty", "response_format"]);

        // Nothing set → nothing dropped.
        req.response_format = None;
        req.presence_penalty = None;
        req.n = None;
        assert!(provider.dropped_params(&req).is_empty());
    }
```

(If `AnthropicProvider::new` takes a specific config, match the file's existing constructor calls in its tests.)

- [ ] **Step 2: Run — expect FAIL (method returns default empty)**

Run: `cargo test -p tt-provider-anthropic dropped_params_reports 2>&1 | tail -15`
Expected: FAIL — `got` is empty (trait default), assert_eq mismatch.

- [ ] **Step 3: Implement the override**

In the `impl Provider for AnthropicProvider` block (`:93`), add:

```rust
    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        // Mirror translate.rs: Anthropic rejects these OpenAI-only fields.
        let mut out = Vec::new();
        if req.n.is_some() {
            out.push("n".to_string());
        }
        if req.seed.is_some() {
            out.push("seed".to_string());
        }
        if req.response_format.is_some() {
            out.push("response_format".to_string());
        }
        if req.presence_penalty.is_some() {
            out.push("presence_penalty".to_string());
        }
        if req.frequency_penalty.is_some() {
            out.push("frequency_penalty".to_string());
        }
        out
    }
```

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p tt-provider-anthropic dropped_params_reports 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/anthropic/src/lib.rs
git commit -m "feat(anthropic): report dropped OpenAI-only params via dropped_params"
```

---

### Task 3: Gemini `dropped_params` override

**Files:**
- Modify: `crates/providers/gemini/src/lib.rs` (the `impl Provider for GeminiProvider` block, `:100`)

- [ ] **Step 1: Write the failing unit test**

In `crates/providers/gemini/src/lib.rs` test module:

```rust
    #[test]
    fn dropped_params_reports_present_fields_but_not_response_format() {
        let provider = GeminiProvider::new(Default::default());
        let req = tt_shared::ChatCompletionRequest {
            model: "gemini-3.1-pro".into(),
            messages: vec![],
            temperature: None,
            top_p: None,
            max_tokens: None,
            stream: false,
            tools: vec![],
            tool_choice: None,
            // Gemini TRANSLATES response_format, so it must NOT be reported.
            response_format: Some(tt_shared::messages::ResponseFormat {
                r#type: "json_object".into(),
                json_schema: None,
            }),
            stop: vec![],
            presence_penalty: None,
            frequency_penalty: Some(0.2),
            n: None,
            seed: Some(7),
            user: Some("u1".into()),
            tt_extras: std::collections::HashMap::new(),
        };
        let mut got = provider.dropped_params(&req);
        got.sort();
        assert_eq!(got, vec!["frequency_penalty", "seed", "user"]);
    }
```

(Match the file's existing `GeminiProvider` constructor idiom.)

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo test -p tt-provider-gemini dropped_params_reports 2>&1 | tail -15`
Expected: FAIL — empty default.

- [ ] **Step 3: Implement the override**

In `impl Provider for GeminiProvider` (`:100`):

```rust
    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        // Mirror translate.rs: Gemini drops these; response_format is translated.
        let mut out = Vec::new();
        if req.n.is_some() {
            out.push("n".to_string());
        }
        if req.seed.is_some() {
            out.push("seed".to_string());
        }
        if req.presence_penalty.is_some() {
            out.push("presence_penalty".to_string());
        }
        if req.frequency_penalty.is_some() {
            out.push("frequency_penalty".to_string());
        }
        if req.user.is_some() {
            out.push("user".to_string());
        }
        out
    }
```

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p tt-provider-gemini dropped_params_reports 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/gemini/src/lib.rs
git commit -m "feat(gemini): report dropped params via dropped_params"
```

---

### Task 4: compat reasoning-temperature drop + OpenAI-native + wrapper delegation

**Files:**
- Modify: `crates/providers/compat/src/translate.rs` (add `dropped_params` helper near `is_reasoning_model`, `:24`)
- Modify: `crates/providers/compat/src/compat.rs` (`impl Provider for OpenAICompatibleProvider`, `:145`)
- Modify: `crates/providers/openai/src/lib.rs` (`impl Provider for OpenAiProvider`, `:92`)
- Modify: `crates/providers/{groq,mistral,together,openrouter}/src/lib.rs` (each `impl Provider`)

- [ ] **Step 1: Write the failing helper test in compat**

In `crates/providers/compat/src/translate.rs` test module:

```rust
    #[test]
    fn dropped_params_temperature_only_for_reasoning_models() {
        let mut req = base_request("o3");
        req.temperature = Some(0.7);
        assert_eq!(dropped_params(&req), vec!["temperature".to_string()]);

        // Non-reasoning model: temperature is forwarded, not dropped.
        let mut req2 = base_request("gpt-4o");
        req2.temperature = Some(0.7);
        assert!(dropped_params(&req2).is_empty());

        // Reasoning model but no temperature set → nothing dropped.
        let req3 = base_request("o4-mini");
        assert!(dropped_params(&req3).is_empty());
    }
```

(`base_request` is the existing helper in this test module, `:225`.)

- [ ] **Step 2: Run — expect FAIL (no such fn)**

Run: `cargo test -p tt-provider-compat dropped_params_temperature 2>&1 | tail -15`
Expected: FAIL — `dropped_params` not found.

- [ ] **Step 3: Add the `dropped_params` helper to compat translate**

In `crates/providers/compat/src/translate.rs`, near `is_reasoning_model` (`:24`):

```rust
/// Params the compat layer silently drops for `req`. Reasoning models
/// (`o3`/`o4-mini`) reject `temperature` (see `translate_request`).
pub fn dropped_params(req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
    if is_reasoning_model(&req.model) && req.temperature.is_some() {
        vec!["temperature".to_string()]
    } else {
        Vec::new()
    }
}
```

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p tt-provider-compat dropped_params_temperature 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Wire it into OpenACompatibleProvider + OpenAI-native + the 4 wrappers**

In `crates/providers/compat/src/compat.rs`, `impl Provider for OpenAICompatibleProvider` (`:145`):
```rust
    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        crate::translate::dropped_params(req)
    }
```

In `crates/providers/openai/src/lib.rs`, `impl Provider for OpenAiProvider` (`:92`) — OpenAI-native runs the same reasoning fixup and depends on `tt-provider-compat`:
```rust
    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        tt_provider_compat::translate::dropped_params(req)
    }
```
(If `translate` is not re-exported from `tt_provider_compat`, add `pub use crate::translate;` in `compat/src/lib.rs`, or re-export the fn as `tt_provider_compat::dropped_params`. Confirm the path compiles; adjust to whichever the compat crate exposes.)

In each of `crates/providers/{groq,mistral,together,openrouter}/src/lib.rs`, inside their `impl Provider for …` block, delegate to the wrapped compat provider (the field is `inner`, mirroring their existing method delegations):
```rust
    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        self.inner.dropped_params(req)
    }
```

- [ ] **Step 6: Build all touched provider crates**

Run: `cargo build -p tt-provider-compat -p tt-provider-openai -p tt-provider-groq -p tt-provider-mistral -p tt-provider-together -p tt-provider-openrouter 2>&1 | tail -8`
Expected: all compile. (If the `tt_provider_compat::translate::dropped_params` path doesn't resolve, add the re-export noted in Step 5 and rebuild.)

- [ ] **Step 7: Commit**

```bash
git add crates/providers/compat crates/providers/openai crates/providers/groq crates/providers/mistral crates/providers/together crates/providers/openrouter
git commit -m "feat(providers): report reasoning-model temperature drop via dropped_params"
```

---

### Task 5: Gateway — attach `X-TokenTrimmer-Warnings` on dispatch paths

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add helper; call at `:949` streaming-live + `:1447` non-stream miss)
- Create: `crates/core/tests/warnings_header.rs`

- [ ] **Step 1: Write the failing integration test**

Create `crates/core/tests/warnings_header.rs`, mirroring the app/registry setup in `crates/core/tests/fallback_header.rs` (same imports + a `MockProvider`). Give the mock a `dropped_params` override and assert the header. Concretely, the mock and assertions:

```rust
//! `X-TokenTrimmer-Warnings: param_dropped:*` is emitted for dropped params.

// (imports + AppState/build_router setup copied from tests/fallback_header.rs)

#[async_trait]
impl Provider for WarnMock {
    fn id(&self) -> &'static str { "warnmock" }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "wm-1".into(), provider: "warnmock".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 128_000, max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.1, output_per_million: 0.1,
            cached_input_per_million: None, cache_write_per_million: None,
            effective_at: Utc::now(),
        })
    }
    fn dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String> {
        let mut v = Vec::new();
        if req.n.is_some() { v.push("n".to_string()); }
        if req.seed.is_some() { v.push("seed".to_string()); }
        v
    }
    async fn chat_completion(&self, req: ChatCompletionRequest, _ctx: &RequestContext)
        -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse {
            id: "x".into(), object: "chat.completion".into(), created: 0, model: req.model,
            choices: vec![Choice { index: 0, message: Message::Assistant {
                content: Some(MessageContent::Text("hi".into())), tool_calls: vec![], name: None,
            }, finish_reason: Some("stop".into()) }],
            usage: Usage { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2,
                cached_tokens: 0, cache_creation_input_tokens: None },
        })
    }
    async fn chat_completion_stream(&self, _req: ChatCompletionRequest, _ctx: &RequestContext)
        -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Err(ProviderError::Unsupported("no stream in test".into()))
    }
}

#[tokio::test]
async fn warnings_header_lists_dropped_params() {
    let app = build_test_app(); // mirror fallback_header.rs's app builder with WarnMock registered
    let body = json!({ "model": "wm-1", "messages": [{"role":"user","content":"hi"}],
        "n": 2, "seed": 7 }).to_string();
    let resp = app.oneshot(Request::builder().method("POST").uri("/v1/chat/completions")
        .header("content-type","application/json").body(Body::from(body)).unwrap())
        .await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp.headers().get("x-tokentrimmer-warnings").unwrap().to_str().unwrap();
    assert!(warn.contains("param_dropped:n"), "got: {warn}");
    assert!(warn.contains("param_dropped:seed"), "got: {warn}");
}

#[tokio::test]
async fn no_warnings_header_when_nothing_dropped() {
    let app = build_test_app();
    let body = json!({ "model": "wm-1", "messages": [{"role":"user","content":"hi"}] }).to_string();
    let resp = app.oneshot(Request::builder().method("POST").uri("/v1/chat/completions")
        .header("content-type","application/json").body(Body::from(body)).unwrap())
        .await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-tokentrimmer-warnings").is_none());
}
```

Copy the exact `build_test_app`/AppState/registry-with-mock construction from `tests/fallback_header.rs` (its test fn near the bottom) so auth/state match this crate's harness.

- [ ] **Step 2: Run — expect FAIL (no header emitted yet)**

Run: `cargo test -p tt-core --test warnings_header 2>&1 | tail -20`
Expected: FAIL — `x-tokentrimmer-warnings` is absent (`unwrap` panics in the first test).

- [ ] **Step 3: Add the `attach_warnings` helper to chat.rs**

Near the other header helpers in `crates/core/src/routes/chat.rs` (e.g. above `build_hit_l1_response`, `:1486`), add:

```rust
/// Attach `X-TokenTrimmer-Warnings` for params the provider drops on `req`.
/// Each dropped param is a `param_dropped:<name>` token; tokens are
/// comma-joined. No-op when nothing is dropped. (B2/B3 will append more
/// tokens to this same channel.)
fn attach_warnings(
    headers: &mut axum::http::HeaderMap,
    provider: &dyn tt_shared::Provider,
    req: &ChatCompletionRequest,
) {
    let tokens: Vec<String> = provider
        .dropped_params(req)
        .into_iter()
        .map(|p| format!("param_dropped:{p}"))
        .collect();
    if tokens.is_empty() {
        return;
    }
    if let Ok(v) = tokens.join(",").parse() {
        headers.insert("x-tokentrimmer-warnings", v);
    }
}
```

- [ ] **Step 4: Call it on the non-stream miss path**

In the non-stream miss block, after the `x-tokentrimmer-cache`/`route-matched` inserts and before `Ok(http_response)` (`:1475`), add:
```rust
        attach_warnings(http_response.headers_mut(), provider.as_ref(), &req);
```
(`provider` is the `Arc<dyn Provider>` already in scope, used at `:1307`/`:1315`; `req` is in scope, used at `:1387`.)

- [ ] **Step 5: Call it on the streaming-live path**

Replace the streaming-live return (`:949-952`):
```rust
        Ok(with_route_matched(
            sse::stream_response(stream, &provider, trace_id, log_ctx),
            route_matched_name.as_deref(),
        ))
```
with:
```rust
        let mut resp = with_route_matched(
            sse::stream_response(stream, &provider, trace_id, log_ctx),
            route_matched_name.as_deref(),
        );
        attach_warnings(resp.headers_mut(), provider.as_ref(), &req);
        Ok(resp)
```
(`provider` here is the post-failover rebinding from `:837`; `req` is in scope.)

- [ ] **Step 6: Run — expect PASS**

Run: `cargo test -p tt-core --test warnings_header 2>&1 | tail -15`
Expected: both tests PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/tests/warnings_header.rs
git commit -m "feat(core): emit X-TokenTrimmer-Warnings: param_dropped on dispatch"
```

---

### Task 6: Docs + gates

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (`:150` prose, `:427` response-header row)

- [ ] **Step 1: Update the docs**

At `:427`, change the `X-TokenTrimmer-Warnings` response-header row from `Planned (not yet emitted)` to `Honored`, value example `param_dropped:frequency_penalty,param_dropped:n`. Add a sentence after the table (or near `:150`):

> Emitted as comma-separated tokens. Currently: `param_dropped:<name>` for each request parameter the routed provider rejects and the gateway drops (e.g. Anthropic drops `n`/`seed`/`response_format`/`presence_penalty`/`frequency_penalty`; reasoning models drop `temperature`). The `response_format_downgrade` and `temperature_clamped` tokens are planned follow-ups.

Keep the `:150` "silently dropped, with a `X-TokenTrimmer-Warnings` response header" line — it is now accurate.

- [ ] **Step 2: Full workspace tests**

Run: `cargo test -p tt-shared -p tt-provider-anthropic -p tt-provider-gemini -p tt-provider-compat -p tt-provider-openai -p tt-core 2>&1 | grep -E "test result:|error\[|FAILED" | tail -20`
Expected: all pass.

- [ ] **Step 3: Clippy + fmt (public CI gates both)**

Run: `cargo clippy -p tt-shared -p tt-provider-anthropic -p tt-provider-gemini -p tt-provider-compat -p tt-provider-openai -p tt-provider-groq -p tt-provider-mistral -p tt-provider-together -p tt-provider-openrouter -p tt-core --all-targets -- -D warnings 2>&1 | grep -E "warning:|error" | grep -v "auto-clean\|Permission denied" | tail -15`
Expected: no warnings/errors.

Run: `cargo fmt 2>&1 | tail -2 && cargo fmt -- --check 2>&1 | tail -5`
Expected: clean (no diff).

- [ ] **Step 4: Commit docs (+ any fmt)**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs(gateway): X-TokenTrimmer-Warnings param_dropped is now honored"
git diff --quiet || (git add -A && git commit -m "style: cargo fmt")
```

- [ ] **Step 5: Confirm scope**

Run: `git diff main --stat`
Expected: only `crates/shared/src/provider.rs`, the 6 provider lib/translate files, `crates/core/src/routes/chat.rs`, `crates/core/tests/warnings_header.rs`, `docs/04-gateway-api-reference.md` (+ the spec/plan docs). No `sse.rs` change (streaming attaches at the chat.rs call site).
