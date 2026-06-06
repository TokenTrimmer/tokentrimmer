# Honor `X-TokenTrimmer-Fallback` request header (F9) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** F9. Honors the `X-TokenTrimmer-Fallback` request header (documented "Planned"): a comma-separated fallback chain that overrides the route-derived chain on `/v1/chat/completions`.

## Goal

Let a caller supply a per-request failover chain (bare model ids) via
`X-TokenTrimmer-Fallback: gpt-4o-mini,claude-3-5-sonnet`. It replaces the
route-derived `route_fallbacks`, reusing the existing failover machinery
unchanged.

## Background (current behavior)

- `route_fallbacks: Vec<String>` is bound from the matched route (`chat.rs:571`, `let mut`), cleared by an `X-TokenTrimmer-Provider` pin (`chat.rs:627-630`).
- When `route_fallbacks` is non-empty, the handler builds `failover_candidates = [primary req.model, …route_fallbacks]` and `failover_creds` — a per-provider credential map resolved with the cross-provider guard `allow_bearer_fallback = (pid == source_provider_id)` (`chat.rs:649-683`).
- The failover loop (`crates/core/src/failover.rs:169-235`) resolves each candidate's provider via `registry.resolve(model)`, **skips** candidates that don't resolve, are circuit-broken, or have no credential in the map (never forwarding the source key cross-provider), and dispatches the rest in order until one succeeds. Both streaming (`dispatch_stream_with_failover`) and non-streaming (`dispatch_with_failover`) paths consume `route_fallbacks` (gated on `route_fallbacks.is_empty()` at `chat.rs:797` / `1139`).
- Docs: `X-TokenTrimmer-Fallback` row (`docs/04-gateway-api-reference.md:410`, "Planned", example `openai/gpt-4o,anthropic/claude-3-5-sonnet`).

## Architecture

All changes in `crates/core/src/routes/chat.rs` + the docs.

### Header parser (`pub(crate)`)
```rust
/// `X-TokenTrimmer-Fallback` — comma-separated override of the route's fallback
/// chain (bare model ids). Absent/blank → None (keep the route chain).
pub(crate) fn fallback_override_from_header(headers: &HeaderMap) -> Option<Vec<String>> {
    let raw = headers
        .get("x-tokentrimmer-fallback")
        .and_then(|v| v.to_str().ok())?;
    let chain: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if chain.is_empty() {
        None
    } else {
        Some(chain)
    }
}
```

### Handler wiring
Insert immediately after the provider-pin block (after `chat.rs:630`), before the cost-limit-header block:
```rust
// `X-TokenTrimmer-Fallback` overrides the route-derived chain. Applied AFTER the
// provider pin's clear, so an explicit chain opts back into failover even when a
// provider is pinned (the pin still set the primary provider above).
if let Some(chain) = fallback_override_from_header(&headers) {
    route_fallbacks = chain;
}
```
Nothing else changes: the existing `failover_candidates`/`failover_creds`
construction and the failover loop consume `route_fallbacks` as-is.

### Behavior (all inherited from the existing machinery)
- **Cross-provider safety:** a header fallback whose provider the org has no stored credential for is skipped during dispatch (the source key is never forwarded cross-provider) — the existing `resolve_credentials_for(..., allow_bearer_fallback = pid == source)` guard. In legacy no-store mode the bearer is forwarded to every candidate (unchanged passthrough semantics).
- **Unknown / typo'd fallback models** are skipped (consistent with route fallbacks — best-effort), not a `400`.
- **Enables failover with no route:** a non-empty header chain makes `route_fallbacks` non-empty, so the failover path runs even when no route matched.
- **Pin + fallback:** the pin still sets the primary provider; the header chain (applied after the pin's clear) is used for failover.
- Applies to both streaming and non-streaming (both branch on `route_fallbacks`).

## Testing

Integration (`crates/core/tests/fallback_header.rs`, mirroring `failover.rs` — a `MockProvider` that serves given model ids and either 200s or 503s; no auth key needed):
- **`fallback_header_enables_failover_without_route`**: no routing store; `primary` provider serves `m-primary` (503), `backup` serves `m-backup` (200). Request `model: "m-primary"` with `X-TokenTrimmer-Fallback: m-backup` → `200`, `x-tokentrimmer-model-used == "m-backup"`, `x-tokentrimmer-provider == "backup"`, primary tried, backup served once.
- **`fallback_header_overrides_route_chain`**: a route (dogfood org) rewrites the request to a failing primary with `fallbacks: ["route-fb"]` (served by provider `routefb`, 200). The header supplies `hdr-fb` (served by provider `hdrfb`, 200). → `hdr-fb` is served and `route-fb`'s provider is **never called** (header replaced the route chain).
- Unit: `fallback_override_from_header` — `"a, b ,c"` → `["a","b","c"]`; absent → None; blank/`" , "` → None.

(The cross-provider credential-skip and primary-healthy-skips-fallback behaviors are already covered by `failover.rs` and unchanged by this slice.)

Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p tt-core`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-core --no-deps`.

## Docs
- Flip `docs/04-gateway-api-reference.md:410` `X-TokenTrimmer-Fallback` row → "Honored". Change the example to bare model ids `gpt-4o-mini,claude-3-5-sonnet` and note: overrides the route's fallback chain; entries are bare model ids; unresolvable or uncredentialed entries are skipped.

## Out of scope
- `provider/model` syntax (bare model ids only, matching route fallbacks — the published example is corrected).
- Embeddings (no failover path).
- A way to *disable* failover via the header (absent → keep route chain; use the provider pin to suppress failover).
- The `X-TokenTrimmer-Timeout-Ms` header (F10).
