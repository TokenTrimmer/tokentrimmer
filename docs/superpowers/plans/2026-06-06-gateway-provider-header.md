# Honor `X-TokenTrimmer-Provider` (F6) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Honor the `X-TokenTrimmer-Provider` request header on `/v1/chat/completions` and `/v1/embeddings` — pin the dispatch provider (routing still sets the model), re-resolving credentials for cross-provider pins and failing closed.

**Architecture:** A header reader + a shared `apply_provider_override` helper in `routes/chat.rs` (reused by `routes/embeddings.rs`), applied after each handler's routing block so the pin is the final provider. Cross-provider pins reuse the existing `resolve_credentials_for(..., allow_bearer_fallback=false)` fail-closed guard. Integration-tested with two fake providers.

**Tech Stack:** Rust, axum, tokio, the in-crate test harness (`tt_core::build_router` + `ProviderRegistry`).

---

### Task 1: `provider_override_from_header` reader

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add the fn near `cost_limit_from_header` ~line 73; add a unit test in the `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing unit test**

In `crates/core/src/routes/chat.rs`, inside `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn provider_override_header_parsing() {
        use axum::http::HeaderMap;
        let mut h = HeaderMap::new();
        assert_eq!(provider_override_from_header(&h), None);
        h.insert("x-tokentrimmer-provider", "  Anthropic ".parse().unwrap());
        assert_eq!(provider_override_from_header(&h).as_deref(), Some("anthropic"));
        let mut empty = HeaderMap::new();
        empty.insert("x-tokentrimmer-provider", "   ".parse().unwrap());
        assert_eq!(provider_override_from_header(&empty), None);
    }
```

Run: `cargo test -p tt-core provider_override_header_parsing 2>&1 | tail -15`
Expected: FAIL to compile — `provider_override_from_header` does not exist.

- [ ] **Step 2: Implement the reader**

Add near `cost_limit_from_header` (after it, ~line 80):

```rust
/// `X-TokenTrimmer-Provider` — an exact provider id to pin for this request
/// (lowercased; provider ids are lowercase). `None` when absent or blank.
pub(crate) fn provider_override_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tokentrimmer-provider")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}
```

Run: `cargo test -p tt-core provider_override_header_parsing 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "feat(core): add provider_override_from_header reader"
```

---

### Task 2: Integration tests (RED)

**Files:**
- Create: `crates/core/tests/provider_override.rs`

- [ ] **Step 1: Write the test file**

Create `crates/core/tests/provider_override.rs`:

```rust
//! `X-TokenTrimmer-Provider` pins the dispatch provider for one request.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;

use tt_auth::credentials::InMemoryProviderCredentialStore;
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tower::util::ServiceExt;
use uuid::Uuid;

/// A provider that records its call count. `owns_model` controls whether it
/// claims `gpt-4o` in `models()` (and thus the `by_model` registry entry).
struct FakeProvider {
    id: &'static str,
    owns_model: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        if !self.owns_model {
            return vec![];
        }
        vec![ModelInfo {
            id: "gpt-4o".into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _c: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("ok".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _r: ChatCompletionRequest,
        _c: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _r: EmbeddingsRequest,
        _c: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .unwrap()
        .plaintext
}

struct Harness {
    app: axum::Router,
    key: String,
    alpha_calls: Arc<AtomicUsize>,
    beta_calls: Arc<AtomicUsize>,
}

/// Build the app with two providers: `alpha` owns `gpt-4o`; `beta` is id-only
/// (reachable only via the pin). `with_cred_store` adds an empty credential store
/// (forces the cross-provider fail-closed path).
async fn harness(with_cred_store: bool) -> Harness {
    let alpha_calls = Arc::new(AtomicUsize::new(0));
    let beta_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(FakeProvider {
        id: "alpha",
        owns_model: true,
        calls: Arc::clone(&alpha_calls),
    }));
    registry.register(Arc::new(FakeProvider {
        id: "beta",
        owns_model: false,
        calls: Arc::clone(&beta_calls),
    }));

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    let mut state = AppState::new(registry).with_key_store(key_store);
    if with_cred_store {
        state = state.with_credential_store(Arc::new(InMemoryProviderCredentialStore::new()));
    }
    Harness {
        app: build_router(state),
        key,
        alpha_calls,
        beta_calls,
    }
}

fn chat_request(provider_header: Option<&str>, key: &str) -> Request<Body> {
    let body =
        json!({ "model": "gpt-4o", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"));
    if let Some(p) = provider_header {
        b = b.header("x-tokentrimmer-provider", p);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn pin_overrides_serving_provider() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(Some("beta"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("beta")
    );
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 1);
    assert_eq!(h.alpha_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn no_header_uses_model_default_provider() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(None, &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.alpha_calls.load(Ordering::Relaxed), 1);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn pin_same_as_source_is_noop() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(Some("alpha"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.alpha_calls.load(Ordering::Relaxed), 1);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn pin_unknown_provider_is_400() {
    let h = harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(Some("nope"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.alpha_calls.load(Ordering::Relaxed), 0);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn cross_provider_pin_without_credential_fails_closed() {
    let h = harness(true).await; // empty credential store
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(Some("beta"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.beta_calls.load(Ordering::Relaxed), 0, "must not dispatch");
}
```

- [ ] **Step 2: Run to verify they fail (RED)**

Run: `cargo test -p tt-core --test provider_override 2>&1 | tail -30`
Expected: `pin_overrides_serving_provider`, `pin_unknown_provider_is_400`, `cross_provider_pin_without_credential_fails_closed` FAIL (the header is ignored today: beta never serves, unknown is ignored → alpha serves 200). `no_header_*` and `pin_same_as_source_*` pass.

- [ ] **Step 3: Commit the failing tests**

```bash
git add crates/core/tests/provider_override.rs
git commit -m "test(core): X-TokenTrimmer-Provider pin behavior (RED)"
```

---

### Task 3: Implement `apply_provider_override` + wire both handlers (GREEN)

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add helper; wire handler)
- Modify: `crates/core/src/routes/embeddings.rs` (import + wire handler)

- [ ] **Step 1: Add the helper to `chat.rs`**

After `provider_override_from_header` (from Task 1), add:

```rust
/// Apply an `X-TokenTrimmer-Provider` pin. Returns the provider to dispatch and,
/// when it differs from `current`, the credentials to use. The pin overrides the
/// routed/inferred provider (the routed model is kept). Cross-provider pins
/// re-resolve the target's stored credentials and fail closed (never forward the
/// source key); pinning back to the source restores source credentials.
///
/// # Errors
/// - [`ApiError::InvalidRequest`] if `pinned_id` is not a known provider id.
/// - [`ApiError::MissingProviderCredential`] if a cross-provider pin has no
///   stored credential.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_provider_override(
    state: &AppState,
    pinned_id: Option<&str>,
    org_id: Uuid,
    raw_bearer: &str,
    source_provider_id: &str,
    current: std::sync::Arc<dyn tt_shared::Provider>,
) -> ApiResult<(std::sync::Arc<dyn tt_shared::Provider>, Option<tt_shared::context::ProviderCredentials>)> {
    let Some(pinned_id) = pinned_id else {
        return Ok((current, None));
    };
    let pinned = state
        .registry
        .by_id(pinned_id)
        .ok_or_else(|| ApiError::InvalidRequest(format!("unknown provider: {pinned_id}")))?;
    if pinned.id() == current.id() {
        return Ok((current, None));
    }
    let creds = if pinned.id() == source_provider_id {
        // Pin back to the source provider — source credentials (bearer fallback OK).
        resolve_credentials(state, org_id, source_provider_id, raw_bearer).await
    } else {
        // Cross-provider pin — require the target's stored credential, fail closed.
        resolve_credentials_for(state, org_id, pinned.id(), raw_bearer, false)
            .await
            .ok_or_else(|| ApiError::MissingProviderCredential {
                provider: pinned.id().to_string(),
            })?
    };
    Ok((pinned, Some(creds)))
}
```

(Use whatever `Arc`/`ProviderCredentials`/`Provider` paths are already imported at the top of `chat.rs`; the fully-qualified forms above compile regardless. If `Arc` and `ProviderCredentials` are already in scope, prefer the short names to match the file.)

- [ ] **Step 2: Wire the chat handler**

In `chat.rs` `handler`, read the pin once alongside other header reads (e.g. just after `raw_bearer` is computed, ~line 386):

```rust
    let provider_pin = provider_override_from_header(&headers);
```

Change the fallbacks binding (line ~465) from `let` to `let mut`:

```rust
    let mut route_fallbacks: Vec<String> = route_match.map(|m| m.fallbacks).unwrap_or_default();
```

Immediately after the routing `if matched_route_id.is_some() { ... }` block closes (after line ~503), before the cost-limit-header block, add:

```rust
    // 2d. Explicit provider pin (X-TokenTrimmer-Provider) — overrides the
    //     routed/inferred provider; the routed model is kept. Fails closed on a
    //     cross-provider pin with no stored credential.
    let (pinned_provider, pin_creds) = apply_provider_override(
        &state,
        provider_pin.as_deref(),
        org_id,
        &raw_bearer,
        &source_provider_id,
        provider,
    )
    .await?;
    provider = pinned_provider;
    if let Some(c) = pin_creds {
        ctx.credentials = c;
    }
    if provider_pin.is_some() {
        // An explicit provider pin must not fail over to a different provider.
        route_fallbacks.clear();
    }
```

- [ ] **Step 3: Wire the embeddings handler**

In `crates/core/src/routes/embeddings.rs`, extend the chat import (lines 25-28) to add the two new items:

```rust
use crate::routes::chat::{
    apply_provider_override, apply_routing, attach_cost_headers, compute_cost,
    cost_limit_from_header, enforce_cost_limit, estimate_cost_usd, provider_override_from_header,
    resolve_credentials, resolve_credentials_for,
};
```

After the routing `if matched { ... }` block closes (after line ~190), before the cost-limit-header block, add:

```rust
    // Explicit provider pin (X-TokenTrimmer-Provider) — see chat.rs.
    let provider_pin = provider_override_from_header(&headers);
    let (pinned_provider, pin_creds) = apply_provider_override(
        &state,
        provider_pin.as_deref(),
        org_id,
        &raw_bearer,
        &source_provider_id,
        provider,
    )
    .await?;
    provider = pinned_provider;
    if let Some(c) = pin_creds {
        ctx.credentials = c;
    }
```

- [ ] **Step 4: Run the integration tests (GREEN)**

Run: `cargo test -p tt-core --test provider_override 2>&1 | tail -20`
Expected: all 5 pass.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/src/routes/embeddings.rs
git commit -m "feat(core): honor X-TokenTrimmer-Provider pin on chat + embeddings"
```

---

### Task 4: Docs

**Files:**
- Modify: `docs/04-gateway-api-reference.md:409` (§6.1 request-header table)

- [ ] **Step 1: Flip the row to honored**

Replace the `X-TokenTrimmer-Provider` row:

```
| `X-TokenTrimmer-Provider` | Override provider selection | Planned (not yet honored) | `anthropic` |
```
with:
```
| `X-TokenTrimmer-Provider` | Pin the upstream provider for this request (routing still sets the model). Requires that provider's stored credential for cross-provider pins (else `400`); disables route fallbacks. | Honored | `anthropic` |
```

- [ ] **Step 2: Commit**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs: mark X-TokenTrimmer-Provider as honored"
```

---

### Task 5: Gates + finish

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt`
Then: `git diff --quiet || (git add -A && git commit -m "style: cargo fmt")`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: no warnings. Fix any, re-run.

- [ ] **Step 3: Test the crate**

Run: `cargo test -p tt-core 2>&1 | grep -E "test result:" | tail`
Expected: all pass (incl. the 5 provider_override tests + the unit test).

- [ ] **Step 4: Doc gate**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-core --no-deps 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 5: Advisories**

Run: `cargo deny check advisories 2>&1 | tail -5`
Expected: ok.

- [ ] **Step 6: Commit any residual gate fixes**

```bash
git status --porcelain
# commit anything outstanding with a descriptive message
```
```
