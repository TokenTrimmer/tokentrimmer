# V3b-2 — Local-model Dispatch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a route target `ollama/<model>` (or `vllm/`, `lmstudio/`) so a self-hosted gateway dispatches sensitive requests to a local LLM, stripping the backend prefix before forwarding.

**Architecture:** One prefix helper (`tt_shared::providers::local_backend`) is the single source of truth. The registry resolves a local-prefixed id to the registered `LocalProvider`; the provider strips its `<backend>/` prefix before delegating. Local backends register only when a base-URL env var is set. `validate_same_provider` exempts local targets (the ADR-018 privacy exception).

**Tech Stack:** Rust workspace — `tt-shared`, `tt-provider-local`, `tt-routing`, `tt-core`. No new external deps (adds an existing intra-workspace dep).

**Repo / branch:** `/Users/iansimon/Developer/TokenTrimmer/public` on `feat/v3b-2-local-dispatch` (off `main`). Spec: `docs/superpowers/specs/2026-06-04-v3b-2-local-dispatch-design.md`.

**Test note:** `cargo test --workspace` hook-denied — scope with `-p`. Red = compile error referencing a not-yet-defined item.

**Verified anchors:**
- `tt_shared::providers` (`crates/shared/src/providers.rs`): `infer_provider`, `known_to_differ`; no local awareness.
- `LocalProvider` (`crates/providers/local/src/lib.rs`): `LocalBackend::{Ollama,Vllm,LmStudio}` with `.id()`/`.default_base_url()`; `new(backend, ClientConfig)` builds `CompatConfig { id, default_base_url, models: vec![], pricing_table: {}, fee_multiplier: 1.0, allow_local: true }`; `chat_completion`/`chat_completion_stream` delegate `req` unchanged (`:143-157`); `suggested_client_config()` (300s).
- `ProviderRegistry` (`crates/core/src/registry.rs`): `resolve` = `by_model` else `infer_provider`+`by_id` (`:63-66`); `by_id`/`register`; `register_providers` (`:167-198`, local commented at `:196`); imports one `tt_provider_*` per provider (`:7-13`). **`tt-core` does NOT depend on `tt-provider-local` (must add).**
- `tt_routing::validate::validate_same_provider` (`crates/routing/src/validate.rs`): loops `model_in`, rejects on `known_to_differ`.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/shared/src/providers.rs` (modify) | `local_backend(model)` prefix helper + tests. |
| `crates/providers/local/src/lib.rs` (modify) | `strip_backend_prefix` + strip in `chat_completion*`; `with_base_url` ctor; tests. |
| `crates/core/Cargo.toml` (modify) | add `tt-provider-local` dep. |
| `crates/core/src/registry.rs` (modify) | local-aware `resolve`; `LocalProviders` env config + `register_local_providers`; tests. |
| `crates/routing/src/validate.rs` (modify) | local-target exemption + test. |
| `crates/core/tests/local_dispatch.rs` (create) | e2e: route `gpt-4o → ollama/llama3` dispatches to the registered ollama provider. |

---

## Task 1: `tt_shared::providers::local_backend`

**Files:** Modify `crates/shared/src/providers.rs`

- [ ] **Step 1: Write the failing test** — append to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn local_backend_recognizes_prefixes() {
        assert_eq!(local_backend("ollama/llama3.1:8b"), Some("ollama"));
        assert_eq!(local_backend("vllm/Qwen2.5-7B"), Some("vllm"));
        assert_eq!(local_backend("lmstudio/phi-4"), Some("lmstudio"));
        // Bare backend id, empty suffix, non-local, empty → None.
        assert_eq!(local_backend("ollama"), None);
        assert_eq!(local_backend("ollama/"), None);
        assert_eq!(local_backend("gpt-4o"), None);
        assert_eq!(local_backend(""), None);
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-shared local_backend` → FAIL (`cannot find function local_backend`).

- [ ] **Step 3: Implement** — add above the `#[cfg(test)]` module:

```rust
/// If `model` is a local-backend-prefixed id (`ollama/…`, `vllm/…`,
/// `lmstudio/…`) with a non-empty model name, return the backend id; else None.
/// Single source of truth for local routing — used by the registry resolver,
/// the same-provider exemption, and `LocalProvider`'s prefix strip.
pub fn local_backend(model: &str) -> Option<&'static str> {
    for id in ["ollama", "vllm", "lmstudio"] {
        if let Some(rest) = model.strip_prefix(id).and_then(|r| r.strip_prefix('/')) {
            if !rest.is_empty() {
                return Some(id);
            }
        }
    }
    None
}
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-shared local_backend` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shared/src/providers.rs
git commit -m "feat(shared): local_backend() prefix helper for ollama/vllm/lmstudio"
```

---

## Task 2: `LocalProvider` prefix strip + `with_base_url`

**Files:** Modify `crates/providers/local/src/lib.rs`

- [ ] **Step 1: Write the failing test** — append to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn strips_backend_prefix() {
        assert_eq!(strip_backend_prefix(LocalBackend::Ollama, "ollama/llama3.1:8b"), "llama3.1:8b");
        // Bare model name (no prefix) is forwarded unchanged.
        assert_eq!(strip_backend_prefix(LocalBackend::Ollama, "llama3.1:8b"), "llama3.1:8b");
        // A different backend's prefix is NOT stripped by this backend.
        assert_eq!(strip_backend_prefix(LocalBackend::Vllm, "ollama/llama3"), "ollama/llama3");
    }

    #[test]
    fn with_base_url_overrides_default() {
        let p = LocalProvider::with_base_url(
            LocalBackend::Ollama, "http://gpu-box:11434/v1", ClientConfig::default());
        assert_eq!(p.id(), "ollama");
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-provider-local strip` → FAIL (`strip_backend_prefix` / `with_base_url` undefined).

- [ ] **Step 3: Add the helper + ctor; strip in the delegations.** In `crates/providers/local/src/lib.rs`:

Add the free function (above `impl Provider for LocalProvider`):

```rust
/// Remove a leading `"<backend.id()>/"` from `model`; otherwise return it
/// unchanged. Local backends serve bare model names — the gateway routes to
/// `ollama/llama3` but Ollama expects `llama3`.
pub(crate) fn strip_backend_prefix(backend: LocalBackend, model: &str) -> String {
    match model.strip_prefix(backend.id()).and_then(|r| r.strip_prefix('/')) {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => model.to_string(),
    }
}
```

Refactor `new` to delegate to a new `with_base_url`:

```rust
    pub fn new(backend: LocalBackend, client_cfg: ClientConfig) -> Self {
        Self::with_base_url(backend, backend.default_base_url(), client_cfg)
    }

    /// Like [`LocalProvider::new`] but with an explicit `base_url` (e.g. from
    /// `TT_LOCAL_OLLAMA_URL`). Self-hosted gateways point this at their backend.
    pub fn with_base_url(
        backend: LocalBackend,
        base_url: impl Into<String>,
        client_cfg: ClientConfig,
    ) -> Self {
        let cfg = CompatConfig {
            id: backend.id(),
            default_base_url: base_url.into(),
            models: Vec::new(),
            pricing_table: HashMap::new(),
            fee_multiplier: 1.0,
            allow_local: true,
        };
        Self {
            backend,
            inner: OpenAICompatibleProvider::new(client_cfg, cfg),
        }
    }
```

(Delete the old `CompatConfig { … }` body from `new`.)

Strip in both chat delegations:

```rust
    async fn chat_completion(
        &self,
        mut req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        req.model = strip_backend_prefix(self.backend, &req.model);
        self.inner.chat_completion(req, ctx).await
    }

    async fn chat_completion_stream(
        &self,
        mut req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        req.model = strip_backend_prefix(self.backend, &req.model);
        self.inner.chat_completion_stream(req, ctx).await
    }
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-provider-local` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/providers/local/src/lib.rs
git commit -m "feat(provider-local): strip backend prefix before forwarding; with_base_url ctor"
```

---

## Task 3: Registry resolve + local registration

**Files:** Modify `crates/core/Cargo.toml`, `crates/core/src/registry.rs`

- [ ] **Step 1: Add the dependency** — in `crates/core/Cargo.toml`, under `[dependencies]`, next to the other `tt-provider-*` lines, add:

```toml
tt-provider-local = { path = "../providers/local" }
```

- [ ] **Step 2: Write the failing tests** — in `crates/core/src/registry.rs`'s `#[cfg(test)] mod tests`, add (the module already imports `super::*`):

```rust
    #[test]
    fn resolves_local_prefixed_model_to_registered_backend() {
        let mut reg = ProviderRegistry::new();
        reg.register(std::sync::Arc::new(
            tt_provider_local::LocalProvider::new(
                tt_provider_local::LocalBackend::Ollama,
                tt_provider_openai::ClientConfig::default(),
            ),
        ));
        assert!(reg.resolve("ollama/llama3.1:8b").is_some());
        // Unregistered backend → None (gateway not configured for it).
        assert!(reg.resolve("vllm/qwen").is_none());
    }

    #[test]
    fn register_local_providers_honors_configured_urls() {
        let mut reg = ProviderRegistry::new();
        register_local_providers(&mut reg, &LocalProviders {
            ollama: Some("http://localhost:11434/v1".into()),
            vllm: None,
            lmstudio: None,
        });
        assert!(reg.by_id("ollama").is_some());
        assert!(reg.by_id("vllm").is_none());
    }
```

- [ ] **Step 3: Run to verify it fails** — `cargo test -p tt-core registry::tests::resolves_local registry::tests::register_local` → FAIL (`register_local_providers`/`LocalProviders` undefined; `resolve` doesn't handle local yet).

- [ ] **Step 4: Implement.** In `crates/core/src/registry.rs`:

Add the import (next to the other provider imports, `:7-13`):

```rust
use tt_provider_local::{LocalBackend, LocalProvider};
```

Make `resolve` local-aware (`:63-66`):

```rust
    pub fn resolve(&self, model: &str) -> Option<Arc<dyn Provider>> {
        self.by_model(model)
            .or_else(|| tt_shared::providers::local_backend(model).and_then(|id| self.by_id(id)))
            .or_else(|| tt_shared::providers::infer_provider(model).and_then(|id| self.by_id(id)))
    }
```

Replace the commented local tail of `register_providers` (`:196-197`) with a call, and add the config + registrar after `register_providers`:

```rust
    // Local backends register only when their base-URL env var is set.
    register_local_providers(registry, &LocalProviders::from_env());
}

/// Self-hosted local backends, each registered only when its base URL is set
/// (`TT_LOCAL_OLLAMA_URL`, `TT_LOCAL_VLLM_URL`, `TT_LOCAL_LMSTUDIO_URL`).
#[derive(Debug, Clone, Default)]
pub struct LocalProviders {
    pub ollama: Option<String>,
    pub vllm: Option<String>,
    pub lmstudio: Option<String>,
}

impl LocalProviders {
    /// Read the three base-URL env vars; an unset/empty var leaves that backend off.
    pub fn from_env() -> Self {
        let v = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        Self {
            ollama: v("TT_LOCAL_OLLAMA_URL"),
            vllm: v("TT_LOCAL_VLLM_URL"),
            lmstudio: v("TT_LOCAL_LMSTUDIO_URL"),
        }
    }
}

/// Register a `LocalProvider` for each configured backend (longer client
/// timeout for cold-start latency).
pub fn register_local_providers(registry: &mut ProviderRegistry, cfg: &LocalProviders) {
    let cc = LocalProvider::suggested_client_config();
    if let Some(url) = &cfg.ollama {
        registry.register(Arc::new(LocalProvider::with_base_url(LocalBackend::Ollama, url.clone(), cc.clone())));
    }
    if let Some(url) = &cfg.vllm {
        registry.register(Arc::new(LocalProvider::with_base_url(LocalBackend::Vllm, url.clone(), cc.clone())));
    }
    if let Some(url) = &cfg.lmstudio {
        registry.register(Arc::new(LocalProvider::with_base_url(LocalBackend::LmStudio, url.clone(), cc.clone())));
    }
}
```

(Confirm `tt_provider_openai::ClientConfig` derives `Clone` — `suggested_client_config()` returns it and the existing code clones provider configs; if `cc.clone()` doesn't compile, call `LocalProvider::suggested_client_config()` per-backend instead.)

- [ ] **Step 5: Run to verify it passes** — `cargo test -p tt-core registry` → PASS (incl. the 2 new tests).

- [ ] **Step 6: Commit**

```bash
git add crates/core/Cargo.toml crates/core/src/registry.rs
git commit -m "feat(core): resolve ollama/-prefixed models; register local backends from env"
```

---

## Task 4: Same-provider exemption for local targets

**Files:** Modify `crates/routing/src/validate.rs`

- [ ] **Step 1: Write the failing test** — in `crates/routing/src/validate.rs`'s `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn local_target_is_exempt_from_same_provider() {
        let when = RouteConditions { model_in: vec!["gpt-4o".into()], ..Default::default() };
        // Routing an OpenAI model to a local model is allowed (privacy exception).
        assert!(validate_same_provider(&when, &action("ollama/llama3.1:8b")).is_ok());
        // A genuine cross-provider (non-local) rewrite is still rejected.
        assert!(validate_same_provider(&when, &action("claude-haiku-4-5")).is_err());
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-routing local_target_is_exempt` → FAIL (`ollama/llama3.1:8b` is currently rejected: `infer_provider("gpt-4o")="openai"` vs local `None` would actually pass today — so this test fails only after Task… ). NOTE: with `local_backend` not yet consulted, `known_to_differ("gpt-4o","ollama/llama3.1:8b")` is `false` (local → `infer_provider` None), so the `is_ok()` assertion already passes. The meaningful guard is making the exemption **explicit and intentional** so a future `infer_provider` extension can't silently start rejecting local targets. Run: `cargo test -p tt-routing local_target_is_exempt` → PASS even before the edit; proceed to Step 3 to lock the intent in.

- [ ] **Step 3: Implement the explicit exemption** — in `validate_same_provider`, before the loop:

```rust
pub fn validate_same_provider(
    when: &RouteConditions,
    then: &RouteAction,
) -> Result<(), ValidationError> {
    // Routing to a local model is a deliberate cross-provider exception for
    // privacy (V3b) — never blocked by the same-provider rule (ADR-018).
    if tt_shared::providers::local_backend(&then.target_model).is_some() {
        return Ok(());
    }
    for src in &when.model_in {
        if known_to_differ(src, &then.target_model) {
            return Err(ValidationError::CrossProvider {
                src: src.clone(),
                target: then.target_model.clone(),
            });
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-routing validate` → PASS (the existing validation tests + the new one).

- [ ] **Step 5: Commit**

```bash
git add crates/routing/src/validate.rs
git commit -m "feat(routing): exempt local targets from the same-provider rule (privacy exception)"
```

---

## Task 5: Gateway end-to-end dispatch test

**Files:** Create `crates/core/tests/local_dispatch.rs`

- [ ] **Step 1: Write the test** — create `crates/core/tests/local_dispatch.rs`. It registers a mock provider whose `id()` is `"ollama"` and plants a route `gpt-4o → ollama/llama3`; the request must reach that provider (proving local resolution):

```rust
//! A route targeting `ollama/<model>` resolves to the registered local-backend
//! provider and dispatches there (prefix-strip is unit-tested in tt-provider-local).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{keys::{issue, Environment}, InMemoryKeyStore, KeyStore};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Mock provider that answers to the `ollama` backend id and records the model.
struct MockOllama { served: Arc<Mutex<Vec<String>>> }

#[async_trait]
impl Provider for MockOllama {
    fn id(&self) -> &'static str { "ollama" }
    fn models(&self) -> Vec<ModelInfo> { Vec::new() }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing { input_per_million: 0.0, output_per_million: 0.0,
            cached_input_per_million: None, cache_write_per_million: None, effective_at: Utc::now() })
    }
    async fn chat_completion(&self, req: ChatCompletionRequest, _c: &RequestContext)
        -> Result<ChatCompletionResponse, ProviderError> {
        self.served.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "x".into(), object: "chat.completion".into(), created: 0, model: req.model,
            choices: vec![Choice { index: 0, message: Message::Assistant {
                content: Some(MessageContent::Text("ok".into())), tool_calls: vec![], name: None },
                finish_reason: Some("stop".into()) }],
            usage: Usage { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10,
                cached_tokens: 0, cache_creation_input_tokens: None },
        })
    }
    async fn chat_completion_stream(&self, _r: ChatCompletionRequest, _c: &RequestContext)
        -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(&self, _r: EmbeddingsRequest, _c: &RequestContext)
        -> Result<EmbeddingsResponse, ProviderError> { Err(ProviderError::Unsupported("no".into())) }
}

/// A second provider so the original `gpt-4o` model resolves before the rewrite.
struct MockOpenAi;
#[async_trait]
impl Provider for MockOpenAi {
    fn id(&self) -> &'static str { "openai" }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo { id: "gpt-4o".into(), provider: "openai".into(),
            capabilities: vec![Capability::Text], max_input_tokens: 4096, max_output_tokens: 4096 }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing { input_per_million: 5.0, output_per_million: 15.0,
            cached_input_per_million: None, cache_write_per_million: None, effective_at: Utc::now() })
    }
    async fn chat_completion(&self, req: ChatCompletionRequest, _c: &RequestContext)
        -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse { id: "y".into(), object: "chat.completion".into(), created: 0,
            model: req.model, choices: vec![], usage: Usage { prompt_tokens: 0, completion_tokens: 0,
                total_tokens: 0, cached_tokens: 0, cache_creation_input_tokens: None } })
    }
    async fn chat_completion_stream(&self, _r: ChatCompletionRequest, _c: &RequestContext)
        -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(&self, _r: EmbeddingsRequest, _c: &RequestContext)
        -> Result<EmbeddingsResponse, ProviderError> { Err(ProviderError::Unsupported("no".into())) }
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System).await.unwrap().plaintext
}

#[tokio::test]
async fn route_to_local_dispatches_to_ollama_provider() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockOpenAi));
    registry.register(Arc::new(MockOllama { served: Arc::clone(&served) }));

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, vec![Route {
        id: Uuid::now_v7(), name: "to-local".into(), priority: 100, enabled: true,
        when: RouteConditions { model_in: vec!["gpt-4o".into()], ..Default::default() },
        then: RouteAction { target_model: "ollama/llama3.1:8b".into(), fallbacks: vec![], force_cache_layer: None },
    }]);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(AppState::new(registry).with_key_store(key_store).with_routing_store(routing));

    let body = json!({ "model": "gpt-4o", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let r = app.oneshot(
        Request::builder().method("POST").uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(body.to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // The ollama provider was dispatched (resolution of the local-prefixed target).
    assert_eq!(served.lock().unwrap().clone(), vec!["ollama/llama3.1:8b".to_string()]);
}
```

- [ ] **Step 2: Run the test** — `cargo test -p tt-core --test local_dispatch`
Expected: PASS. If the served vec is empty / 404, `resolve("ollama/llama3.1:8b")` isn't finding the `id()=="ollama"` provider — re-check Task 3's `resolve` change.

- [ ] **Step 3: Commit**

```bash
git add crates/core/tests/local_dispatch.rs
git commit -m "test(core): route to ollama/ dispatches to the local-backend provider"
```

---

## Task 6: Final verification

**Files:** none.

- [ ] **Step 1: Format** — `cargo fmt -p tt-shared -p tt-provider-local -p tt-routing -p tt-core`; then `git diff --quiet || git commit -am "style: cargo fmt (v3b-2)"`.
- [ ] **Step 2: Clippy** — `cargo clippy -p tt-shared -p tt-provider-local -p tt-routing --all-targets -- -D warnings` then `cargo clippy -p tt-core --tests -- -D warnings`. Expected: clean.
- [ ] **Step 3: Tests** — `cargo test -p tt-shared -p tt-provider-local -p tt-routing` then `cargo test -p tt-core --test local_dispatch --test route_rewrite`. Expected: all pass.
- [ ] **Step 4: Clean tree** — `git status` + `git log --oneline -8` (Task 1–5 commits on `feat/v3b-2-local-dispatch`).

---

## Self-Review (completed by plan author)

**1. Spec coverage:** `local_backend` helper → Task 1; prefix strip + `with_base_url` → Task 2; local-aware `resolve` + env registration → Task 3 (+ `tt-provider-local` dep); same-provider exemption → Task 4; e2e dispatch → Task 5. Out-of-scope (V3d, embeddings, dashboard, cloud admin exemption) untouched.

**2. Placeholder scan:** every step has complete code/commands + expected output. Task 4 Step 2 honestly notes the test passes pre-edit (the change makes the exemption *explicit/future-proof*) rather than faking a red — and Step 3 still locks in the intent.

**3. Type consistency:** `local_backend(&str) -> Option<&'static str>` (Task 1) used by `resolve` (Task 3) and `validate_same_provider` (Task 4). `strip_backend_prefix(LocalBackend, &str) -> String` + `with_base_url(LocalBackend, impl Into<String>, ClientConfig)` (Task 2) used by `register_local_providers` (Task 3). `LocalProviders { ollama, vllm, lmstudio: Option<String> }` + `register_local_providers` defined+used in Task 3. The e2e mock `id()=="ollama"` matches the `local_backend` backend id.
