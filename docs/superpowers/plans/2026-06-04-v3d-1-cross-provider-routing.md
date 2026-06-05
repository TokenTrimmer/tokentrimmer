# V3d-1 Cross-Provider Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow a route to rewrite a request to a model on any provider, correct end-to-end (validation removed, credentials follow the rewritten provider on primary + failover paths, Plan projects cross-provider savings).

**Architecture:** Delete the create-time same-provider gate; re-resolve upstream credentials for the rewritten provider (fail closed when absent); thread per-candidate credentials through failover; fix the Plan replay pricing key to use the target's own provider.

**Tech Stack:** Rust workspace — `tt-routing`, `tt-core` (axum 0.7 gateway), `tt-plan-core`. Spec: `docs/superpowers/specs/2026-06-04-v3d-1-cross-provider-routing-design.md`.

---

## Task 1: Remove the same-provider gate

**Files:**
- Modify: `crates/core/tests/routes_api.rs:210-219` (invert the rejection test)
- Modify: `crates/routing/src/validate.rs` (delete fn + variant + import + 3 tests + module doc)
- Modify: `crates/routing/src/lib.rs:24` (re-export) and `:84-86` (RouteAction doc)
- Modify: `crates/core/src/routes/routes_api.rs:10,50-51` (import + call)

- [ ] **Step 1: Invert the integration test to assert acceptance**

In `crates/core/tests/routes_api.rs`, replace `cross_provider_target_rejected` (lines 210-219):

```rust
#[tokio::test]
async fn cross_provider_target_accepted() {
    // V3d-1: cross-provider routes are allowed. A gpt-4o -> claude-haiku-4-5
    // route creates successfully (capability guard is permissive on the
    // unknown target; the same-provider gate is gone).
    let (app, key, _) = app_with_key().await;
    let spec = json!({ "name": "x", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"claude-haiku-4-5"} });
    let r = app
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p tt-core --test routes_api cross_provider_target_accepted`
Expected: FAIL — currently `validate_same_provider` returns 400 BAD_REQUEST, not 201.

- [ ] **Step 3: Delete the gate**

In `crates/routing/src/validate.rs`:
- Delete the `use tt_shared::providers::known_to_differ;` import (line 6).
- Delete the `CrossProvider` variant (lines 12-13) from `ValidationError`.
- Delete the entire `validate_same_provider` function (lines 21-41).
- Delete the three tests `same_provider_ok_and_cross_provider_rejected`, `unknown_models_pass_same_provider`, `local_target_is_exempt_from_same_provider`.
- Update the module doc-comment (lines 1-3) to:

```rust
//! Typed route validation shared by the gateway routes API. The capability
//! check mirrors the runtime guard (`tt_shared::capability_check`). Cross-
//! provider rewrites are allowed (V3d-1) — see
//! docs/superpowers/specs/2026-06-04-v3d-1-cross-provider-routing-design.md.
```

In `crates/routing/src/lib.rs`:
- Line 24: change to `pub use validate::{validate_capability, ValidationError};`
- Lines 84-86: replace the `target_model` doc-comment with:

```rust
    /// Rewrite to this model. May target a different provider than the request
    /// (V3d-1 cross-provider routing); the target is capability-checked and
    /// dispatch/savings use the target's own provider.
```

In `crates/core/src/routes/routes_api.rs`:
- Line 10: remove `validate_same_provider` from the import → `use tt_routing::{validate_capability, NewRoute, Route, RoutingStore};`
- Lines 50-51: delete the `validate_same_provider(&spec.when, &spec.then).map_err(...)?;` statement.

- [ ] **Step 4: Run to verify green**

Run: `cargo test -p tt-routing && cargo test -p tt-core --test routes_api`
Expected: PASS — `cross_provider_target_accepted` is 201; `has_images_non_vision_target_rejected` still 400; validate.rs compiles (capability tests remain).

- [ ] **Step 5: Commit**

```bash
git add crates/routing/src/validate.rs crates/routing/src/lib.rs crates/core/src/routes/routes_api.rs crates/core/tests/routes_api.rs
git commit -m "feat(routing): allow cross-provider route targets (remove same-provider gate)"
```

---

## Task 2: Credentials follow the rewritten provider (primary path)

**Files:**
- Create: `crates/core/tests/cross_provider.rs` (new integration test + mock providers)
- Modify: `crates/core/src/error.rs` (add `MissingProviderCredential` variant + response arm)
- Modify: `crates/core/src/routes/chat.rs` (`resolve_credentials_for`, `source_provider_id`, `mut ctx`, primary re-resolution)

- [ ] **Step 1: Write the failing integration tests + mock harness**

Create `crates/core/tests/cross_provider.rs`:

```rust
//! V3d-1: cross-provider routing dispatches to the target provider with the
//! TARGET provider's upstream credential, and fails closed when the org has no
//! credential for the target.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, InMemoryProviderCredentialStore, KeyStore, ProviderCredentialStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    context::ProviderCredentials,
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
    SecretString, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// A provider with a fixed id serving one model; records the api_key it saw on
/// each call, and can be made to fail (for failover tests in Task 3).
struct Mock {
    id: &'static str,
    model: &'static str,
    input_price: f64,
    output_price: f64,
    seen_keys: Arc<Mutex<Vec<String>>>,
    fail: bool,
}

#[async_trait]
impl Provider for Mock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: self.input_price,
                output_per_million: self.output_price,
                cached_input_per_million: None,
                cache_write_per_million: None,
                effective_at: Utc::now(),
            })
        } else {
            None
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.seen_keys
            .lock()
            .unwrap()
            .push(ctx.credentials.api_key.expose().to_string());
        if self.fail {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "mock failure".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: "chatcmpl-mock".into(),
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
        _req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.seen_keys
            .lock()
            .unwrap()
            .push(ctx.credentials.api_key.expose().to_string());
        if self.fail {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "mock failure".into(),
            });
        }
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

fn creds(key: &str) -> ProviderCredentials {
    ProviderCredentials {
        api_key: SecretString::new(key.to_string()),
        base_url: None,
        extra_headers: Vec::new(),
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "k", Environment::Live, Actor::System)
        .await
        .expect("issue tt_live_ key")
        .plaintext
}

fn route(org_target: &str, fallbacks: Vec<String>) -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "x-provider".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: org_target.into(),
            fallbacks,
            force_cache_layer: None,
            disable_cache: false,
        },
    }
}

fn chat(model: &str, bearer: &str, stream: bool) -> Request<Body> {
    let body = json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": stream });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Build an app: openai(gpt-4o) + anthropic(claude-haiku-4-5) mocks, a
/// gpt-4o -> claude-haiku-4-5 route, and a credential store the caller seeds.
fn build(
    org: Uuid,
    key_store: Arc<dyn KeyStore>,
    cred_store: Arc<dyn ProviderCredentialStore>,
    openai_keys: Arc<Mutex<Vec<String>>>,
    anthropic_keys: Arc<Mutex<Vec<String>>>,
) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "openai",
        model: "gpt-4o",
        input_price: 5.0,
        output_price: 15.0,
        seen_keys: openai_keys,
        fail: false,
    }));
    registry.register(Arc::new(Mock {
        id: "anthropic",
        model: "claude-haiku-4-5",
        input_price: 0.25,
        output_price: 1.25,
        seen_keys: anthropic_keys,
        fail: false,
    }));
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, vec![route("claude-haiku-4-5", vec![])]);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(cred_store)
            .with_routing_store(routing),
    )
}

#[tokio::test]
async fn cross_provider_uses_target_credential() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI"));
    store.insert(org, "anthropic", creds("ANT"));
    let openai_keys = Arc::new(Mutex::new(Vec::new()));
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build(
        org,
        Arc::new(raw),
        Arc::new(store),
        Arc::clone(&openai_keys),
        Arc::clone(&anthropic_keys),
    );

    let r = app.oneshot(chat("gpt-4o", &key, false)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Dispatched to anthropic (the target) with ANTHROPIC's key — not OpenAI's.
    assert_eq!(anthropic_keys.lock().unwrap().clone(), vec!["ANT".to_string()]);
    assert!(openai_keys.lock().unwrap().is_empty());
    assert_eq!(r.headers()["x-tokentrimmer-provider"].to_str().unwrap(), "anthropic");
}

#[tokio::test]
async fn cross_provider_missing_credential_fails_closed() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI")); // no anthropic credential
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build(
        org,
        Arc::new(raw),
        Arc::new(store),
        Arc::new(Mutex::new(Vec::new())),
        Arc::clone(&anthropic_keys),
    );

    let r = app.oneshot(chat("gpt-4o", &key, false)).await.unwrap();
    // Fail closed: never forward the OpenAI/raw key to Anthropic.
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(anthropic_keys.lock().unwrap().is_empty());
}
```

NOTE on imports: verify `tt_shared` re-exports `context::ProviderCredentials` and `SecretString` at the paths used above; if not, import from `tt_shared::context::{ProviderCredentials, SecretString}`. Verify `ProviderError::ProviderUpstream` field names against `crates/shared/src` (used by the existing failover tests).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-core --test cross_provider`
Expected: FAIL — `cross_provider_uses_target_credential` sees `"OAI"` (stale credentials), and `cross_provider_missing_credential_fails_closed` returns 200 (silent dispatch), not 400.

- [ ] **Step 3: Add the ApiError variant**

In `crates/core/src/error.rs`, add to the enum (after `ModelNotFound`, ~line 30):

```rust
    #[error("no upstream credential for provider {provider}")]
    MissingProviderCredential { provider: String },
```

And add the response arm in `into_response` (after the `ModelNotFound` arm, ~line 95):

```rust
            ApiError::MissingProviderCredential { provider } => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "missing_provider_credential",
                format!(
                    "No upstream credential configured for provider '{provider}', required by a matched route. Add it before routing to this provider."
                ),
            ),
```

- [ ] **Step 4: Add `resolve_credentials_for` and refactor `resolve_credentials`**

In `crates/core/src/routes/chat.rs`, replace the body of `resolve_credentials` (lines 1464-1482) with a delegation and add the new helper:

```rust
async fn resolve_credentials(
    state: &AppState,
    org_id: Uuid,
    provider_id: &str,
    raw_bearer: &str,
) -> ProviderCredentials {
    // Source-provider resolution always allows the raw-Bearer fallback (legacy
    // BYO-key passthrough). `expect` is safe: allow_bearer_fallback=true never
    // returns None.
    resolve_credentials_for(state, org_id, provider_id, raw_bearer, true)
        .await
        .expect("bearer fallback yields Some")
}

/// Resolve upstream credentials for `provider_id`. Returns `None` only when the
/// store has no entry AND `allow_bearer_fallback` is false — i.e. a cross-
/// provider target whose key we must not substitute with the source provider's
/// bearer.
async fn resolve_credentials_for(
    state: &AppState,
    org_id: Uuid,
    provider_id: &str,
    raw_bearer: &str,
    allow_bearer_fallback: bool,
) -> Option<ProviderCredentials> {
    if let Some(store) = state.credential_store.as_ref() {
        match store.get(org_id, provider_id).await {
            Ok(Some(c)) => return Some(c),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "credential store lookup failed"),
        }
    }
    if allow_bearer_fallback {
        Some(ProviderCredentials {
            api_key: SecretString::new(raw_bearer.to_string()),
            base_url: None,
            extra_headers: Vec::new(),
        })
    } else {
        None
    }
}
```

(Confirm `SecretString` and `ProviderCredentials` are already imported in chat.rs — they are used at the existing line 1477.)

- [ ] **Step 5: Capture the source provider, make `ctx` mutable, re-resolve on the primary path**

In `crates/core/src/routes/chat.rs`:
- Before routing (immediately after the credentials are resolved at line 372), capture the source provider id:
  ```rust
  let source_provider_id = provider.id().to_string();
  ```
- Change `let ctx = RequestContext {` (line 374) to `let mut ctx = RequestContext {`.
- Inside the existing `if matched_route_id.is_some() {` block, after the provider is re-resolved (after line 411), add the credential re-resolution for the single-dispatch (no-fallback) case:
  ```rust
      // Cross-provider rewrite: the credentials resolved above are for the
      // source provider. For a single-provider dispatch, re-resolve for the
      // target and fail closed if the org has no credential (never forward the
      // source key). The failover path resolves per-candidate (see below).
      if route_fallbacks.is_empty() && provider.id() != source_provider_id {
          match resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, false).await {
              Some(c) => ctx.credentials = c,
              None => {
                  return Err(ApiError::MissingProviderCredential {
                      provider: provider.id().to_string(),
                  })
              }
          }
      }
  ```
- Update the stale comment at lines 404-405 to: `// Provider may change when a route crosses providers (V3d-1); the registry is the source of truth.`

- [ ] **Step 6: Run to verify green**

Run: `cargo test -p tt-core --test cross_provider cross_provider_uses_target_credential cross_provider_missing_credential_fails_closed`
Expected: PASS — anthropic sees `"ANT"`; missing-credential returns 400. Also run `cargo test -p tt-core --test route_rewrite` to confirm same-provider routes are unaffected (their provider == source, so no re-resolution).

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/error.rs crates/core/src/routes/chat.rs crates/core/tests/cross_provider.rs
git commit -m "fix(gateway): re-resolve credentials for cross-provider target, fail closed when absent"
```

---

## Task 3: Per-candidate credentials (failover path)

**Files:**
- Modify: `crates/core/src/failover.rs` (add `credentials_by_provider` param to both dispatch fns; per-candidate ctx)
- Modify: `crates/core/src/routes/chat.rs` (build the candidate credential map; pass to both call sites; hoist `candidates`)
- Modify: `crates/core/tests/cross_provider.rs` (add failover tests + a 3rd mock)

- [ ] **Step 1: Write the failing failover tests**

Append to `crates/core/tests/cross_provider.rs` a builder + tests that exercise a cross-provider fallback. Add a gemini mock and a route `gpt-4o -> claude-haiku-4-5` with `fallbacks=["gemini-2.5-flash"]`, where the anthropic mock fails so dispatch falls over to gemini:

```rust
#[allow(clippy::too_many_arguments)]
fn build_failover(
    org: Uuid,
    key_store: Arc<dyn KeyStore>,
    cred_store: Arc<dyn ProviderCredentialStore>,
    anthropic_fail: bool,
    gemini_in_store: bool,
    anthropic_keys: Arc<Mutex<Vec<String>>>,
    gemini_keys: Arc<Mutex<Vec<String>>>,
) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock { id: "openai", model: "gpt-4o", input_price: 5.0, output_price: 15.0, seen_keys: Arc::new(Mutex::new(Vec::new())), fail: false }));
    registry.register(Arc::new(Mock { id: "anthropic", model: "claude-haiku-4-5", input_price: 0.25, output_price: 1.25, seen_keys: anthropic_keys, fail: anthropic_fail }));
    registry.register(Arc::new(Mock { id: "gemini", model: "gemini-2.5-flash", input_price: 0.1, output_price: 0.4, seen_keys: gemini_keys, fail: false }));
    let _ = gemini_in_store; // store seeding done by caller
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, vec![route("claude-haiku-4-5", vec!["gemini-2.5-flash".into()])]);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    build_router(AppState::new(registry).with_key_store(key_store).with_credential_store(cred_store).with_routing_store(routing))
}

#[tokio::test]
async fn failover_candidate_uses_its_own_provider_credential() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI"));
    store.insert(org, "anthropic", creds("ANT"));
    store.insert(org, "gemini", creds("GEM"));
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let gemini_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build_failover(org, Arc::new(raw), Arc::new(store), /*anthropic_fail=*/ true, true, Arc::clone(&anthropic_keys), Arc::clone(&gemini_keys));

    let r = app.oneshot(chat("gpt-4o", &key, false)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Primary (anthropic) was tried with ANT, failed; fell over to gemini with GEM.
    assert_eq!(anthropic_keys.lock().unwrap().clone(), vec!["ANT".to_string()]);
    assert_eq!(gemini_keys.lock().unwrap().clone(), vec!["GEM".to_string()]);
    assert_eq!(r.headers()["x-tokentrimmer-provider"].to_str().unwrap(), "gemini");
}

#[tokio::test]
async fn failover_skips_candidate_without_credential() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let store = InMemoryProviderCredentialStore::new();
    store.insert(org, "openai", creds("OAI"));
    store.insert(org, "anthropic", creds("ANT")); // no gemini credential
    let anthropic_keys = Arc::new(Mutex::new(Vec::new()));
    let gemini_keys = Arc::new(Mutex::new(Vec::new()));
    let app = build_failover(org, Arc::new(raw), Arc::new(store), /*anthropic_fail=*/ true, false, Arc::clone(&anthropic_keys), Arc::clone(&gemini_keys));

    let r = app.oneshot(chat("gpt-4o", &key, false)).await.unwrap();
    // anthropic failed; gemini has no credential so it is skipped (never called);
    // no candidate succeeds -> upstream error surfaced (5xx), gemini untouched.
    assert!(gemini_keys.lock().unwrap().is_empty());
    assert!(r.status().is_server_error());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-core --test cross_provider failover_candidate_uses_its_own_provider_credential failover_skips_candidate_without_credential`
Expected: FAIL — the failover path currently passes the shared `ctx` (anthropic's `ANT`) to the gemini candidate, so `gemini_keys` would be `["ANT"]`, not `["GEM"]`; and the skip test would call gemini with `ANT`.

- [ ] **Step 3: Add the `credentials_by_provider` parameter to failover**

In `crates/core/src/failover.rs`, add the parameter to **both** `dispatch_with_failover` (the fn whose body starts ~line 150) and `dispatch_stream_with_failover` (line 232). Add after the `ctx: &RequestContext,` parameter:

```rust
    credentials_by_provider: &std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
```

In each candidate loop, after `let Some(provider) = registry.resolve(model) else { continue; };` and the breaker check, build a per-candidate context:

```rust
        let Some(cand_creds) = credentials_by_provider.get(provider.id()) else {
            tracing::info!(model = %model, provider = %provider.id(), "failover_skip: no upstream credential for candidate provider");
            continue;
        };
        let mut cand_ctx = ctx.clone();
        cand_ctx.credentials = cand_creds.clone();
```

Then change the dispatch calls to use `&cand_ctx`:
- non-stream (line 189): `with_retry(retry, || provider.chat_completion(attempt_req.clone(), &cand_ctx))`
- stream (line 285): `with_retry(retry, || provider.chat_completion_stream(attempt_req.clone(), &cand_ctx))`

- [ ] **Step 4: Build and pass the credential map at both call sites**

In `crates/core/src/routes/chat.rs`, hoist the candidate list + credential map so both the streaming (line ~529) and non-streaming (line ~863) failover branches share them. Add this immediately after the post-route provider re-resolve block (after the Task 2 credential block, ~line 412), guarded on the presence of fallbacks:

```rust
    // For a failover chain, pre-resolve upstream credentials for every distinct
    // provider in the candidate set. The raw-Bearer fallback is allowed only for
    // the source provider (the bearer is its key); cross-provider candidates with
    // no stored credential are skipped during dispatch.
    let (failover_candidates, failover_creds): (
        Vec<String>,
        std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) = if route_fallbacks.is_empty() {
        (Vec::new(), std::collections::HashMap::new())
    } else {
        let candidates: Vec<String> = std::iter::once(req.model.clone())
            .chain(route_fallbacks.iter().cloned())
            .collect();
        let mut map = std::collections::HashMap::new();
        for m in &candidates {
            if let Some(p) = state.registry.resolve(m) {
                let pid = p.id().to_string();
                if !map.contains_key(&pid) {
                    if let Some(c) =
                        resolve_credentials_for(&state, org_id, &pid, &raw_bearer, pid == source_provider_id).await
                    {
                        map.insert(pid, c);
                    }
                }
            }
        }
        (candidates, map)
    };
```

Then in both failover branches, replace the locally-built `candidates` with `failover_candidates` and pass `&failover_creds` to the dispatch fn. For the non-streaming branch (lines 853-877), delete the local `let candidates = …` (lines 854-856) and use `&failover_candidates`:

```rust
            crate::failover::dispatch_with_failover(
                &state.registry,
                &state.breaker,
                &RetryPolicy::default(),
                &failover_candidates,
                &req,
                &ctx,
                &failover_creds,
                Utc::now(),
                Some(crate::failover::CapCheck { required: &cap_required, estimated_tokens: cap_est_tokens }),
            )
```

Apply the equivalent edit to the streaming branch at line 529 (`dispatch_stream_with_failover`), inserting `&failover_creds` in the same position (after the `&ctx` argument) and using `&failover_candidates`.

NOTE: verify the exact argument order of each failover call against the current source and insert `&failover_creds` to match the new parameter position (after `ctx`). Build errors will pinpoint any mismatch.

- [ ] **Step 5: Run to verify green**

Run: `cargo test -p tt-core --test cross_provider && cargo test -p tt-core --test route_rewrite`
Expected: PASS — gemini gets `GEM` on failover; gemini skipped (untouched) when it has no credential; existing same-provider failover/rewrite tests still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/failover.rs crates/core/src/routes/chat.rs crates/core/tests/cross_provider.rs
git commit -m "fix(failover): resolve upstream credentials per candidate provider"
```

---

## Task 4: Plan replay projects cross-provider savings

**Files:**
- Modify: `crates/plan-core/src/replay.rs` (`project_requests` target pricing key)
- Modify: `crates/plan-core/tests/replay.rs` (2 new tests)

- [ ] **Step 1: Write the failing replay tests**

Append to `crates/plan-core/tests/replay.rs`:

```rust
#[test]
fn cross_provider_route_prices_target_by_its_own_provider() {
    // openai/gpt-4o request, routed to anthropic/claude-haiku-4-5. Both priced.
    // Must reroute and project savings (not count unprice_able).
    let mut req = make_req(1, 0, "gpt-4o", 1000, 100, 0.0045, false);
    req.provider = "openai".into();
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "x-provider".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions { model_in: vec!["gpt-4o".into()], ..Default::default() },
        then: RouteAction {
            target_model: "claude-haiku-4-5".into(),
            force_cache_layer: None,
            fallbacks: Vec::new(),
            disable_cache: false,
        },
    };
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-haiku-4-5", 0.25, 1.25);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    assert_eq!(result.aggregates.requests_rerouted, 1);
    assert_eq!(result.aggregates.requests_unprice_able, 0);
    assert!(result.aggregates.projected_savings_usd > 0.0);
}

#[test]
fn cross_provider_target_absent_is_conservative() {
    // Same cross-provider route but the target pricing is missing entirely.
    let mut req = make_req(1, 0, "gpt-4o", 1000, 100, 0.0045, false);
    req.provider = "openai".into();
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "x-provider".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions { model_in: vec!["gpt-4o".into()], ..Default::default() },
        then: RouteAction {
            target_model: "claude-haiku-4-5".into(),
            force_cache_layer: None,
            fallbacks: Vec::new(),
            disable_cache: false,
        },
    };
    let result = replay(input_with_routes(vec![req], vec![route], HashMap::new(), 100)).unwrap();
    assert_eq!(result.aggregates.requests_unprice_able, 1);
    assert_eq!(result.aggregates.requests_rerouted, 0);
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-plan-core --test replay cross_provider_route_prices_target_by_its_own_provider cross_provider_target_absent_is_conservative`
Expected: `cross_provider_route_prices_target_by_its_own_provider` FAILS (today it counts `requests_unprice_able == 1`); the absent test PASSES already.

- [ ] **Step 3: Resolve the target's own provider in `project_requests`**

In `crates/plan-core/src/replay.rs`, build a deterministic `model -> provider` index once at the top of `project_requests` (before the `for req in requests` loop, ~line 224):

```rust
    // Map each priced model to its provider, derived from the pricing-table keys
    // ("{provider}:{model}", provider has no ':'). Built from SORTED keys so the
    // first-wins choice on a duplicate model id is deterministic (the replay's
    // bit-identical contract). Used to price a cross-provider route's target by
    // the target's OWN provider rather than the request's provider.
    let model_to_provider: HashMap<&str, &str> = {
        let mut keys: Vec<&str> = pricing.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut m: HashMap<&str, &str> = HashMap::new();
        for k in keys {
            if let Some((prov, model)) = k.split_once(':') {
                m.entry(model).or_insert(prov);
            }
        }
        m
    };
```

Then replace the target-key construction (line 235):

```rust
                // Prefer the same-provider key (keeps same-provider replays
                // byte-identical); else resolve the target's own provider so a
                // cross-provider route is priced correctly.
                let same_provider_key =
                    crate::types::pricing_key(&req.provider, &route.then.target_model);
                let target_key = if pricing.contains_key(&same_provider_key) {
                    same_provider_key
                } else {
                    let target_provider = model_to_provider
                        .get(route.then.target_model.as_str())
                        .copied()
                        .unwrap_or(req.provider.as_str());
                    crate::types::pricing_key(target_provider, &route.then.target_model)
                };
```

- [ ] **Step 4: Run to verify green + determinism intact**

Run: `cargo test -p tt-plan-core`
Expected: PASS for the two new tests AND all existing tests, including `snapshot_canned_replay` and the `determinism_*` tests (byte-identical — same-provider fixtures resolve to the identical key). If the snapshot drifts, the fix changed same-provider behavior — do NOT accept the snapshot; fix the logic.

- [ ] **Step 5: Commit**

```bash
git add crates/plan-core/src/replay.rs crates/plan-core/tests/replay.rs
git commit -m "fix(plan-core): price cross-provider route targets by the target's own provider"
```

---

## Task 5: Doc-comment cleanup + final verification

**Files:**
- Modify: `crates/plan-core/src/types.rs:149-152` (RouteAction.target_model doc)
- Modify: `crates/core/src/routes/chat.rs` (any remaining stale same-provider comment)

- [ ] **Step 1: Update the plan-core RouteAction doc-comment**

In `crates/plan-core/src/types.rs`, replace the `target_model` doc (lines 149-152) — drop the "same-provider only / cross-provider routing lands in a follow-up" wording:

```rust
    /// Rewrite to this model. May target a different provider than the request
    /// (V3d-1). Cost projection resolves the target's own provider from the
    /// pricing table; a target absent from the table is counted unchanged.
    pub target_model: String,
```

(Verify the exact field/doc lines; adjust to match.)

- [ ] **Step 2: Grep for any remaining stale same-provider references in the changed crates**

Run: `grep -rn "same-provider\|same_provider\|ADR-018" crates/routing/src crates/plan-core/src crates/core/src/routes/chat.rs`
Expected: no remaining assertions that routing is same-provider-only (doc-comments updated). Fix any stragglers; historical `docs/` design files are point-in-time records and stay as-is.

- [ ] **Step 3: Full verification across the touched crates**

```bash
cargo fmt -p tt-routing -p tt-core -p tt-plan-core
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p tt-routing -p tt-plan-core
cargo test -p tt-core --test routes_api --test route_rewrite --test cross_provider --test local_dispatch --test disable_cache
```
Expected: fmt clean, clippy clean (workspace — catches every literal/call site), all tests green.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "docs(routing): update stale same-provider doc-comments (V3d-1)"
```

---

## Self-review notes

- **Spec coverage:** §1 gate removal → Task 1; §3 primary credentials + fail-closed → Task 2; §4 failover credentials → Task 3; §5 replay pricing → Task 4; §6 docs → Tasks 1, 4, 5. Testing matrix (§Testing) mapped across Tasks 1-4.
- **Determinism:** Task 4 prefers the same-provider key first → existing snapshots byte-identical; sorted index for the cross-provider case.
- **Fail-closed interaction:** Task 2's 400 only fires for the single-dispatch (no-fallback) cross-provider case; the failover path (Task 3) skips credential-less candidates instead — gated on `route_fallbacks.is_empty()`.
- **Type consistency:** `resolve_credentials_for(state, org_id, provider_id, raw_bearer, allow_bearer_fallback) -> Option<ProviderCredentials>` used identically in Tasks 2 and 3; `credentials_by_provider: &HashMap<String, ProviderCredentials>` keyed by `provider.id()`.
