# `X-TokenTrimmer-Warnings: param_dropped` + warnings channel (G-B1) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** G-B1 (`gw-warnings-header`, slice 1 of 3, public repo). Surface params the gateway silently drops during provider translation as `X-TokenTrimmer-Warnings` response headers, and establish the reusable warnings channel that B2 (`response_format_downgrade`) and B3 (`temperature_clamped`) extend.

## Goal

The doc (`docs/04-gateway-api-reference.md`) advertises an `X-TokenTrimmer-Warnings` response header marked "Planned (not yet emitted)". Meanwhile the provider adapters genuinely drop OpenAI-only request params during translation, silently. This slice makes those drops observable via the header, and stands up the collection/emission plumbing so the two follow-up slices only add producers.

## Background (current, verified)

Real drops today (silent — only a `tracing` log at most):
- **Anthropic** (`crates/providers/anthropic/src/translate.rs:337-338`): intentionally drops `n, seed, response_format, presence_penalty, frequency_penalty` (Anthropic rejects them).
- **Gemini** (`crates/providers/gemini/src/translate.rs:468-469`): drops `n, seed, presence_penalty, frequency_penalty, user`. `response_format` is **translated** (→ `responseMimeType`/`responseSchema`), not dropped.
- **compat** (`crates/providers/compat/src/translate.rs:82-96`) + **OpenAI native** (`crates/providers/openai/src/lib.rs` reasoning fixup): drop `temperature` for reasoning models (`o3`, `o4-mini` per `compat::is_reasoning_model`, `translate.rs:24-25`) when present. The `max_tokens`→`max_completion_tokens` rename is a rename, not a drop.
- **local** and non-reasoning **openai**: drop nothing.

Seams:
- `Provider` trait (`crates/shared/src/provider.rs:15`) already uses default methods (`fee_multiplier` → 1.0, `embeddings` → Unsupported, `health_check` → Ok). The gateway dispatches through `dyn Provider`.
- `RequestContext` (`crates/shared/src/context.rs:49`) is plain data passed by `&ctx`.
- Response headers are attached in `chat.rs` via `attach_cost_headers(...)` plus manual `headers.insert("x-tokentrimmer-cache"/"x-tokentrimmer-route-matched", ...)` on each return path (`:1448/:1467/:1473/:1502/:1514/:1548/:1600`). Streaming headers are set in `sse.rs:644-647` (`x-tokentrimmer-trace-id`, `x-tokentrimmer-provider`) before the body.
- `04-gateway-api-reference.md:427`: the `X-TokenTrimmer-Warnings` response row is "Planned (not yet emitted)"; `:150` prose claims params "silently dropped, with a `X-TokenTrimmer-Warnings` response header".

## Architecture

### 1. `Provider::dropped_params` (additive trait method)
`crates/shared/src/provider.rs`:
```rust
/// Names of request params this adapter silently drops for `req` during
/// translation (because the upstream provider rejects them). Used by the
/// gateway to emit `X-TokenTrimmer-Warnings: param_dropped:<name>`. Default:
/// none.
fn dropped_params(&self, _req: &ChatCompletionRequest) -> Vec<String> {
    Vec::new()
}
```
Overrides (each returns only params actually present in `req`):
- **Anthropic** (`anthropic/src/lib.rs`): check `req.n`, `req.seed`, `req.response_format`, `req.presence_penalty`, `req.frequency_penalty`; push the names that are `Some`/non-empty. A private `fn dropped_param_names(req) -> Vec<String>` co-located with translate keeps it in sync.
- **Gemini** (`gemini/src/lib.rs`): same for `n, seed, presence_penalty, frequency_penalty, user`.
- **compat-based** (groq/mistral/together/openrouter — whichever wrap `compat`) **and OpenAI native**: `vec!["temperature".into()]` when `compat::is_reasoning_model(&req.model) && req.temperature.is_some()`, else empty. Expose a `compat::dropped_params(req) -> Vec<String>` helper reused by all compat-based adapters; OpenAI native uses the same reasoning predicate.

### 2. Warnings channel (gateway, `chat.rs`)
After the provider is finalized (post routing/pin/fallback) and **before** `req` is moved into dispatch, build the tokens once so they apply on every return path:
```rust
let mut warnings: Vec<String> = Vec::new();
warnings.extend(
    provider.dropped_params(&req).into_iter().map(|p| format!("param_dropped:{p}")),
);
```
A small helper attaches them:
```rust
fn attach_warnings(headers: &mut HeaderMap, warnings: &[String]) {
    if warnings.is_empty() { return; }
    if let Ok(v) = HeaderValue::from_str(&warnings.join(",")) {
        headers.insert("x-tokentrimmer-warnings", v);
    }
}
```
Call `attach_warnings(resp.headers_mut(), &warnings)` on each chat return path (cache-hit, cache-miss/dispatch, and the forced/route-matched paths) right where `x-tokentrimmer-cache` is already inserted. Computing from `(req, provider)` (not from dispatch) means a cache hit still reports that the caller's params were ignored. B2/B3 push additional tokens onto `warnings` before attachment.

### 3. Streaming (`sse.rs`)
Compute the same tokens from `(req, provider)` and `headers.insert("x-tokentrimmer-warnings", …)` alongside the existing `trace-id`/`provider` inserts (`:644-647`), guarded by non-empty.

### 4. Header format
Comma-separated tokens; each dropped param is its own token: `param_dropped:n,param_dropped:seed`. Self-describing and unambiguous; B2's `response_format_downgrade` and B3's `temperature_clamped` slot in as sibling tokens.

### 5. Docs (`04-gateway-api-reference.md`)
- Flip the response-header row (`:427`) to **Honored**, value example `param_dropped:frequency_penalty,param_dropped:n`.
- Note (near `:150`): the param-drop + Warnings header is now honored; `response_format_downgrade` / `temperature_clamped` tokens land in follow-ups (still "Planned").

## Testing

- **Adapter unit tests** (in each provider crate): `dropped_params` returns the expected names when those params are set, empty when absent; the temperature/reasoning case returns `["temperature"]` only for `o3`/`o4-mini` with a temperature set.
- **Gateway integration (httpmock, `chat.rs` tests):** a request carrying `n` + `seed` routed to an Anthropic mock yields a response whose `x-tokentrimmer-warnings` contains `param_dropped:n` and `param_dropped:seed`; a request with none of the dropped params yields **no** `x-tokentrimmer-warnings` header.
- **Streaming parity:** the SSE path sets the same header (one httpmock SSE test).

Gates: `cargo test` (changed crates + workspace); `cargo clippy --all-targets -- -D warnings`; `cargo fmt --check` (public CI gates fmt); `cargo doc` (intra-doc links).

## Out of scope
- B2 (`response_format_downgrade` — needs a json_object-vs-json_schema capability distinction) and B3 (`temperature_clamped` — needs per-provider ranges). Separate slices.
- Changing *what* gets dropped — this slice only *reports* existing behavior.
- Embeddings (no such params).
- Reworking `RequestContext` (the channel lives in `chat.rs`/`sse.rs`, not the context).
