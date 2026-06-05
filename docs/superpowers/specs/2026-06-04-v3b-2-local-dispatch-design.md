# Design: V3b-2 — Local-model dispatch (`ollama/…` routing)

_Date: 2026-06-04 · Status: approved design, pre-implementation · Repo: `public` (gateway `tt-core`, `tt-routing`, `tt-shared`, `tt-provider-local`)_

> Second half of **V3b — Privacy → local**. V3b-1 (PR #16) shipped the cache
> opt-out. V3b-2 makes a route able to target a **local** model so a
> **self-hosted** gateway can route sensitive requests to the user's own LLM
> (Ollama / vLLM / LM Studio). Hosted gateways can't reach a customer's localhost,
> so local backends register only when their base URL is configured.

## Problem

`LocalProvider` exists (`crates/providers/local`) but is **commented out of the
registry** (`registry.rs:196-197`) because it needs a per-deployment `base_url`,
and local model ids (`ollama/llama3.1:8b`) don't resolve: `resolve()` is
`by_model` → `infer_provider`+`by_id`, and local providers have no static models
while `infer_provider` returns `None` for local names. So a route can't target a
local model, and even if it could, **ADR-018 (same-provider only)** would reject
`gpt-4o → ollama/…` as a cross-provider rewrite — yet routing to local is
*inherently* cross-provider and is the whole point of the privacy story.

## Current state (verified 2026-06-04)

- `LocalProvider` (`crates/providers/local/src/lib.rs`): wraps
  `OpenAICompatibleProvider` per `LocalBackend` (`Ollama`/`Vllm`/`LmStudio`, ids
  `"ollama"`/`"vllm"`/`"lmstudio"`, default base URLs `localhost:11434|8000|1234/v1`).
  `models()` empty; `chat_completion`/`chat_completion_stream` delegate `req`
  **unchanged** to `inner` (`:143-157`). `LocalProvider::new(backend, ClientConfig)`
  sets `CompatConfig.default_base_url = backend.default_base_url()`.
- `ProviderRegistry::resolve` (`registry.rs:63-66`): `by_model` else
  `infer_provider`+`by_id`. `register_providers` (`:167-198`) registers the seven
  hosted providers from `ProvidersConfig` (a `TT_PROVIDERS` allowlist); local is
  the commented-out tail.
- `tt_shared::providers::infer_provider` (`crates/shared/src/providers.rs`) +
  `known_to_differ`: prefix table for hosted providers; local names → `None`.
- `tt_routing::validate::validate_same_provider` rejects when `known_to_differ`.
  The cloud admin has its own copy in `routes_admin.rs::validate_same_provider`.

## Goals / non-goals

**Goals:** a route can target `ollama/<model>` (or `vllm/`, `lmstudio/`); the
gateway resolves+dispatches it to the registered local provider, stripping the
backend prefix before forwarding; local backends register when their base-URL env
var is set; local targets are exempt from the same-provider rule.

**Non-goals:** generalized cross-provider routing (V3d); local **embeddings**
(unsupported today); a model catalog for local backends; dashboard UI for local
targets (the raw-JSON / `tt route --to ollama/…` paths suffice); per-org local
config (this is a gateway/deployment-level env setting).

## Design

### 1. `tt_shared::providers::local_backend` (new)

```rust
/// If `model` is a local-backend-prefixed id (`ollama/…`, `vllm/…`,
/// `lmstudio/…`), return the backend id; else None.
pub fn local_backend(model: &str) -> Option<&'static str> {
    for id in ["ollama", "vllm", "lmstudio"] {
        if let Some(rest) = model.strip_prefix(id).and_then(|r| r.strip_prefix('/')) {
            if !rest.is_empty() { return Some(id); }
        }
    }
    None
}
```
Single source of truth used by resolution, the same-provider exemption, and the
provider's prefix strip.

### 2. Registry resolution (`registry.rs`)

`resolve` recognizes a local-prefixed model and dispatches to the registered local
provider by backend id:
```rust
pub fn resolve(&self, model: &str) -> Option<Arc<dyn Provider>> {
    self.by_model(model)
        .or_else(|| tt_shared::providers::local_backend(model).and_then(|id| self.by_id(id)))
        .or_else(|| tt_shared::providers::infer_provider(model).and_then(|id| self.by_id(id)))
}
```
(Returns `None` when the backend isn't registered — i.e. the gateway wasn't
configured for it — yielding the existing `ModelNotFound` 404, which is correct.)

### 3. `LocalProvider` — strip the backend prefix

`chat_completion` + `chat_completion_stream` strip `"<backend>/"` from `req.model`
before delegating (Ollama expects `llama3.1:8b`, not `ollama/llama3.1:8b`):
```rust
async fn chat_completion(&self, mut req, ctx) -> ... {
    req.model = strip_backend_prefix(self.backend, req.model);
    self.inner.chat_completion(req, ctx).await
}
```
`strip_backend_prefix(backend, model)` removes a leading `"<backend.id()>/"` if
present, else returns the model unchanged (so a bare `llama3` still works).

### 4. Registration + config (`registry.rs`)

A `LocalProviders` env config registers a backend when its base URL is set:
- `TT_LOCAL_OLLAMA_URL`, `TT_LOCAL_VLLM_URL`, `TT_LOCAL_LMSTUDIO_URL`.

`register_providers` (or a new `register_local_providers` it calls) constructs a
`LocalProvider` pointed at that base URL via a new
`LocalProvider::with_base_url(backend, base_url, client_cfg)` (sets
`CompatConfig.default_base_url`). Uses `LocalProvider::suggested_client_config()`
(300s timeout) for cold-start latency. Unset env = not registered (hosted gateways
unchanged).

### 5. Same-provider exemption (`tt_routing::validate` + cloud)

`validate_same_provider`: before the `known_to_differ` check, if
`local_backend(target_model).is_some()`, **allow** (local routing is the V3b
privacy exception to ADR-018). Mirror the exemption in the cloud
`routes_admin.rs::validate_same_provider` (cloud follow-up — the gateway
`/v1/routes` path enforces the public copy now).

## Data flow

Self-hosted gateway with `TT_LOCAL_OLLAMA_URL=http://localhost:11434/v1`:
`tt route add --when-tag sensitive --from gpt-4o --to ollama/llama3.1:8b` (V3b-1's
`--disable-cache` composes) → validation allows the local target → a tagged request
matches → `apply_routing` rewrites `req.model = "ollama/llama3.1:8b"` →
`resolve` → the ollama `LocalProvider` → strips to `llama3.1:8b` → forwards to
localhost Ollama.

## Error handling

- Local-prefixed model but backend not registered → existing `ModelNotFound` (404)
  — the gateway isn't configured for it. Pricing falls back to zero (existing).
- Capability guard stays permissive for local (unknown `ModelInfo`), matching today.
- Same-provider validation no longer rejects local targets; non-local cross-provider
  still rejected (V3d relaxes that).

## Testing (TDD; scoped `cargo test -p <crate>`)

- `tt-shared`: `local_backend` matrix (`ollama/x`→`ollama`; bare `ollama`/empty→None;
  `gpt-4o`→None; `vllm/`, `lmstudio/`).
- `tt-provider-local`: `strip_backend_prefix` (strips `ollama/`; leaves bare model;
  leaves a *different* prefix untouched).
- `tt-core` registry: a registered local provider + `resolve("ollama/llama3")`
  returns it; unregistered → `None`.
- `tt-routing` validate: `gpt-4o → ollama/llama3` passes (exempt);
  `gpt-4o → claude-…` still rejected.
- `tt-core` integration (mirror `route_rewrite.rs`, register a mock provider whose
  `id()=="ollama"`): a route `gpt-4o → ollama/llama3` dispatches to it and the
  provider observes the **stripped** model `llama3`.

## Success criteria

- With a local base-URL env set, a route to `ollama/<model>` dispatches to the local
  backend with the prefix stripped; without it, `ModelNotFound`.
- Creating/validating a `→ ollama/…` route is not rejected as cross-provider; other
  cross-provider rewrites still are.
- Existing routing/dispatch tests unchanged.

## Out of scope (restated)

General cross-provider (V3d); local embeddings; local model catalog; dashboard
local-target UI; cloud `routes_admin` exemption (small follow-up); per-org local config.
