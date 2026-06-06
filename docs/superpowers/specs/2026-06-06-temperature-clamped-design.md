# `X-TokenTrimmer-Warnings: temperature_clamped` (G-B3) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** G-B3 (`gw-warnings-header`, slice 3 of 3, public repo). Clamp `temperature` to the routed provider's valid range before dispatch and emit `X-TokenTrimmer-Warnings: temperature_clamped`. Completes the B-series warnings channel.

## Goal

Providers have different valid `temperature` ranges; the gateway currently forwards the value verbatim, so an out-of-range value (e.g. `1.5` to Anthropic, whose max is `1.0`) is rejected upstream with a `400`. This slice clamps the value to the provider's range before dispatch — turning that failure into a success — and tells the caller via the warnings header.

## Background (current, verified)

- All adapters forward `temperature` unchanged: Anthropic `translate.rs:330` (`temperature: req.temperature`), Gemini `translate.rs:424`, compat `translate.rs` (non-reasoning). No clamping exists anywhere.
- `ChatCompletionRequest.temperature: Option<f32>` (`messages.rs:125`). `ModelInfo` has no temperature-range field.
- Documented ranges: OpenAI **0.0–2.0**, Gemini **0.0–2.0**, Anthropic **0.0–1.0** (values `>1.0` are rejected). The Anthropic 1.0 ceiling is the concrete, common clamp trigger.
- Reasoning models (`o3`/`o4-mini`) **drop** `temperature` (compat/openai), reported by B1 as `param_dropped:temperature`. The clamp must not touch a dropped param.
- B1/B2 (merged #59/#60/#61): `Provider::dropped_params` + `Provider::supports_response_schema`, the gateway `warnings: Vec<String>` declared after the cost-limit block (`chat.rs` ~`:688`), `maybe_downgrade_response_format` runs there, and `attach_warnings(headers, provider, req, served_model, extra: &[String])` already merges pre-dispatch tokens onto the channel at the two dispatch paths.
- Doc (`docs/04-gateway-api-reference.md:151`) marks temperature clamping `_(Planned)_`.

## Architecture

### 1. `Provider::temperature_range` (additive trait method)
`crates/shared/src/provider.rs`:
```rust
/// The provider's accepted `temperature` range `(min, max)`. The gateway clamps
/// an out-of-range request value to this and emits `temperature_clamped`.
/// Default `(0.0, 2.0)` — the widest common range (OpenAI/Gemini). Override only
/// with a narrower range you are confident is correct, so the gateway never
/// wrongly tightens a provider whose true max is uncertain.
fn temperature_range(&self) -> (f32, f32) {
    (0.0, 2.0)
}
```
Override → **Anthropic** (`anthropic/src/lib.rs`) returns `(0.0, 1.0)`. Gemini, OpenAI, compat (+ wrappers), local inherit the default.

### 2. Gateway clamp (pre-dispatch, `chat.rs`)
A helper run immediately after `maybe_downgrade_response_format` (same normalization point, `warnings` already in scope):
```rust
fn maybe_clamp_temperature(
    req: &mut ChatCompletionRequest,
    provider: &dyn tt_shared::Provider,
    warnings: &mut Vec<String>,
) {
    let Some(t) = req.temperature else {
        return;
    };
    // Reasoning models drop temperature (B1 param_dropped) — don't clamp a dropped param.
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
The mutation happens before the cache key + dispatch, so the cache and every failover candidate see the clamped value. No `attach_warnings` change — B2's `extra: &[String]` already carries pre-dispatch tokens (the call sites pass `&warnings`).

### 3. Token format
Bare `temperature_clamped` (consistent with the doc's "with a warning"; the other tokens are likewise self-describing). The clamped value is not embedded.

### 4. Docs
Flip the `_(Planned)_` line at `:151` to honored, and update the B1 `X-TokenTrimmer-Warnings` prose to list `temperature_clamped` as emitted.

## Interaction notes
- **Order:** B2 downgrade then B3 clamp, both before the cache/dispatch branch; independent (different fields). Both push onto the one `warnings` Vec.
- **Reasoning models:** `temperature` is dropped → `param_dropped:temperature`, and the clamp's `dropped_params` guard skips it (no double-report).
- **Default-wide range:** a provider with an unknown/narrower true max (e.g. some compat models) keeps `(0.0, 2.0)`, so in-range-for-OpenAI values pass through untouched and the upstream decides — no wrongful clamp.

## Testing
- **Unit:** `temperature_range` returns `(0.0, 1.0)` for Anthropic and `(0.0, 2.0)` for a default (non-overriding) adapter, e.g. Gemini.
- **Gateway integration** (`crates/core/tests/`, MockProvider with a configurable `temperature_range` + a `seen` capture of the dispatched `temperature`):
  - `temperature: 1.5` routed to a `(0.0, 1.0)` mock → response has `x-tokentrimmer-warnings: temperature_clamped` AND the dispatched request's `temperature == 1.0`.
  - `temperature: 0.5` → no clamp, no warning, dispatched value unchanged.
  - A mock that drops `temperature` (returns it from `dropped_params`) → `param_dropped:temperature`, NOT `temperature_clamped`, and no temperature mutation beyond the drop.
- Gates: `cargo test` (changed crates + workspace); `cargo clippy --all-targets -- -D warnings`; `cargo fmt --check`; `cargo doc`.

## Out of scope
- Per-*model* temperature ranges (per-provider for v1).
- Tightening compat-family / local ranges without documented evidence.
- Embedding the clamped value in the token.
- `top_p` or other parameter clamping.
