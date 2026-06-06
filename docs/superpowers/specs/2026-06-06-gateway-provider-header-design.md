# Honor `X-TokenTrimmer-Provider` request header (F6) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** F6. Makes the gateway honor the `X-TokenTrimmer-Provider` request header (documented as "Planned (not yet honored)").

## Goal

Let a caller pin the upstream provider for a single request via `X-TokenTrimmer-Provider: <id>`, on both `/v1/chat/completions` and `/v1/embeddings`. The caller's pin is the final word on **which provider**; org routing still governs **which model**. Cross-provider pins must use the pinned provider's stored credentials and **fail closed** — never forward the source provider's key.

## Background (current behavior)

- Provider is resolved from the model: `state.registry.resolve(&req.model)` (`chat.rs:365`, `embeddings.rs:92`). Routing (`apply_routing`) may rewrite `req.model`, after which the provider is re-resolved (`chat.rs:466-474`, `embeddings.rs:156-162`).
- A routing cross-provider rewrite already re-resolves credentials for the target and fails closed via `resolve_credentials_for(state, org, provider, bearer, /*allow_bearer_fallback=*/ false)` → `ApiError::MissingProviderCredential` (`chat.rs:479-488`). F6 reuses this exact guard.
- The registry exposes `by_id(id) -> Option<Arc<dyn Provider>>` (`registry.rs:48`) — an exact provider-id lookup, distinct from `resolve` (model-based).
- The header is currently documented "Planned (not yet honored)" at `docs/04-gateway-api-reference.md:409`.

## Architecture

All changes in `crates/core` (`routes/chat.rs`, `routes/embeddings.rs`) plus the docs row.

### Header reader (`chat.rs`, `pub(crate)`)
Mirror `cost_limit_from_header`:
```rust
/// `X-TokenTrimmer-Provider` — an exact provider id to pin for this request.
pub(crate) fn provider_override_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tokentrimmer-provider")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}
```

### Override helper (`chat.rs`, `pub(crate)`)
One place holds the (security-critical) credential logic; both handlers call it.
```rust
/// Apply an `X-TokenTrimmer-Provider` pin. Returns the provider to dispatch and,
/// when it differs from `current`, the credentials to use. The pin overrides the
/// routed/inferred provider (the routed model is kept). Cross-provider pins
/// re-resolve the target's stored credentials and fail closed (never forward the
/// source key); pinning back to the source restores source credentials.
///
/// # Errors
/// - `InvalidRequest` if `pinned_id` is not a known provider id.
/// - `MissingProviderCredential` if a cross-provider pin has no stored credential.
pub(crate) async fn apply_provider_override(
    state: &AppState,
    pinned_id: Option<&str>,
    org_id: Uuid,
    raw_bearer: &str,
    source_provider_id: &str,
    current: Arc<dyn Provider>,
) -> ApiResult<(Arc<dyn Provider>, Option<ProviderCredentials>)> {
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
`InvalidRequest` (existing variant) keeps the error enum and its TS bindings unchanged.

### Chat handler wiring (`chat.rs`)
- Read the pin once near the other header reads:
  ```rust
  let provider_pin = provider_override_from_header(&headers);
  ```
- Make `route_fallbacks` `mut` (it is currently `let`).
- Immediately **after** the routing block (after `chat.rs:503`, before the cost-limit-header enforcement at ~505):
  ```rust
  let (pinned_provider, pin_creds) = apply_provider_override(
      &state, provider_pin.as_deref(), org_id, &raw_bearer, &source_provider_id, provider,
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
  The existing cost-limit-header block (prices on `provider.id()` + `req.model`) then prices against the pinned provider automatically.

### Embeddings handler wiring (`embeddings.rs`)
Identical, minus fallbacks (embeddings has no failover chain). After its routing block (after `embeddings.rs:189`):
```rust
let provider_pin = provider_override_from_header(&headers);
let (pinned_provider, pin_creds) = apply_provider_override(
    &state, provider_pin.as_deref(), org_id, &raw_bearer, &source_provider_id, provider,
)
.await?;
provider = pinned_provider;
if let Some(c) = pin_creds {
    ctx.credentials = c;
}
```
Import `apply_provider_override` and `provider_override_from_header` from the chat module (the same path embeddings already uses for `resolve_credentials`/`resolve_credentials_for`).

### Sandbox / interactions
- The `tt_test_*` sandbox short-circuit (`chat.rs:390`, `embeddings.rs:118`) runs **before** routing/pin and is unchanged — sandbox requests ignore the pin (no real dispatch).
- The pin applies after routing, so a routed model is kept and dispatched to the pinned provider. If the pinned provider cannot serve that model the upstream call fails normally (the caller's explicit choice).

## Testing

**Integration** (`crates/core/tests/provider_override.rs`, mirroring `disable_cache.rs`): register two fake `Provider`s — `alpha` (owns the model via `by_model`) and `beta` (registered by id; can serve the same model id). Build the app with both.
- **`pin_overrides_serving_provider`**: POST with `x-tokentrimmer-provider: beta` → the `x-tokentrimmer-provider` response header is `beta` (and beta's call counter incremented, alpha's not).
- **`pin_unknown_provider_400`**: `x-tokentrimmer-provider: nope` → `400`.
- **`pin_same_as_source_is_noop`**: `x-tokentrimmer-provider: alpha` (the source) → served by alpha, `200`.
- **`cross_provider_pin_without_credential_fails_closed`**: with a credential store configured that has **no** credential for `beta`, `x-tokentrimmer-provider: beta` → `400` (`MissingProviderCredential`), and beta is **not** called.
- **`no_header_unchanged`**: no pin header → served by the model's default provider (regression guard).

**Unit** (`chat.rs` `#[cfg(test)]`): `provider_override_from_header` — present/trim/lowercase, empty → None, absent → None.

Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p tt-core`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-core --no-deps`.

## Docs
Flip `docs/04-gateway-api-reference.md` §6.1 row: `X-TokenTrimmer-Provider` from `Planned (not yet honored)` → honored, with a one-line note that it pins the dispatch provider, requires that provider's stored credential cross-provider (else 400), and disables route fallbacks.

## Out of scope
- The other Planned request headers: `X-TokenTrimmer-Route` (F8), `X-TokenTrimmer-Cache` (F7), `X-TokenTrimmer-Fallback` (F9), `X-TokenTrimmer-Timeout-Ms` (F10), `Trace-Parent`.
- Validating that the pinned provider actually serves the model (upstream returns its own error — the caller pinned it deliberately).
- Streaming-specific changes (the pin is resolved before dispatch, so streaming inherits it unchanged).
