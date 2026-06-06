# `X-TokenTrimmer-Warnings: response_format_downgrade` (G-B2) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** G-B2 (`gw-warnings-header`, slice 2 of 3, public repo). When a request asks for `response_format: json_schema` but the routed provider supports only `json_object`, downgrade it before dispatch and emit `X-TokenTrimmer-Warnings: response_format_downgrade` onto the B1 warnings channel.

## Goal

Make the doc's `response_format_downgrade` behavior real. Today the gateway forwards `response_format` unchanged; providers that don't support structured outputs (json_schema) either reject it or silently ignore the schema. This slice normalizes such requests to `json_object` and tells the caller via the warnings header.

## Background (current, verified)

- `ResponseFormat { r#type: String, json_schema: Option<Value> }` (`crates/shared/src/messages.rs`); `type` is `"json_object"` or `"json_schema"`.
- `Capability::JsonMode` (`crates/shared/src/pricing.rs:53`) is **binary** — no object-vs-schema distinction. Routing's `RequiredCapabilities.json_mode` is set for either type (`capability_check.rs:53`), so a json request only routes to a `JsonMode`-capable model.
- Provider schema handling: **OpenAI-native** and **compat** forward `response_format` as-is (`compat/src/translate.rs:115`); **Gemini** translates `json_schema` faithfully into `responseSchema` (`gemini/src/translate.rs:583-595`) — so OpenAI + Gemini effectively support schema. **Anthropic** drops `response_format` entirely (B1 reports `param_dropped:response_format`). The compat-family upstreams (groq/mistral/together/openrouter) and local mostly accept only `json_object`.
- B1 (merged #59): `Provider::dropped_params(&req)` + the gateway warnings channel — `attach_warnings(headers, provider, req, served_model)` emits comma-joined tokens on the two dispatch paths (non-stream miss, streaming live), computed against the served model. `warnings` is intended as a `Vec` other producers extend.
- `registry.model_info(model)` (`crates/core/src/registry.rs:44`) gives the gateway the routed model's `ModelInfo`.
- Doc (`docs/04-gateway-api-reference.md:300`) currently marks `response_format_downgrade` _(Planned)_.

## Architecture

### 1. `Provider::supports_response_schema` (additive trait method)
`crates/shared/src/provider.rs`:
```rust
/// Whether this provider faithfully honors `response_format: json_schema`
/// (structured outputs). Default `false` (conservative: the gateway downgrades
/// to `json_object` for providers that don't, and warns). Override `true` only
/// where schema mode is genuinely supported.
fn supports_response_schema(&self) -> bool {
    false
}
```
Overrides → `true` on **OpenAI-native** (`openai/src/lib.rs`) and **Gemini** (`gemini/src/lib.rs`). Anthropic, `OpenAICompatibleProvider`, the 4 compat wrappers, and local inherit `false`.

### 2. Pre-dispatch downgrade (gateway, `chat.rs`)
A helper, called once right after routing finalizes the provider and **before** the stream/non-stream branch + cache lookup (~`:700`), so the cache key and every failover candidate see the normalized request:
```rust
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
    // If the adapter drops response_format outright (Anthropic), that's a B1
    // param_dropped — not a downgrade.
    if provider.dropped_params(req).iter().any(|p| p == "response_format") {
        return;
    }
    req.response_format = Some(tt_shared::messages::ResponseFormat {
        r#type: "json_object".to_string(),
        json_schema: None,
    });
    warnings.push("response_format_downgrade".to_string());
}
```
A `let mut warnings: Vec<String> = Vec::new();` is declared at this point and threaded to both attach sites.

### 3. Channel merge (`attach_warnings`)
Add an `extra: &[String]` parameter carrying the pre-dispatch tokens (e.g. `response_format_downgrade`). The helper emits `extra` tokens **plus** the model-dependent `param_dropped:*` tokens it already computes:
```rust
fn attach_warnings(headers, provider, req, served_model, extra: &[String]) {
    // param_dropped:* computed against served_model (B1) ...
    let mut tokens: Vec<String> = /* param_dropped tokens */;
    tokens.extend(extra.iter().cloned());
    // emit comma-joined if non-empty
}
```
Both dispatch call sites pass `&warnings`. (Cache-hit / fake-stream paths still emit nothing — and a downgrade that happened pre-cache-lookup means the cached entry is already the `json_object` form.)

## Interaction notes
- **Cache key:** downgrade runs before `namespaced_l1_key`, so a `json_schema` and an equivalent `json_object` request to a non-schema provider share a cache entry — correct, since they produce the same upstream call.
- **Failover:** the decision uses the routed (pre-failover) provider; the mutated `json_object` req is what all candidates receive. Matches the doc's "routed to a provider that doesn't support schema mode." (A primary that supports schema failing over to one that doesn't is a rare edge the doc's per-provider framing doesn't promise to catch — noted, not handled.)
- **Anthropic:** unchanged — `response_format` is dropped (`param_dropped:response_format`), no downgrade (the `dropped_params` guard skips it).

## Testing
- **Unit:** `supports_response_schema` is `true` for OpenAI + Gemini, `false` for Anthropic / compat / groq / mistral / together / openrouter / local.
- **Gateway integration** (`crates/core/tests/`, MockProvider harness):
  - A `json_schema` request routed to a default-`false` mock → response has `x-tokentrimmer-warnings: response_format_downgrade`, AND the mock received `response_format.type == "json_object"` with no schema (capture the dispatched req via `Arc<Mutex<Option<ResponseFormat>>>`).
  - A `json_schema` request to a mock overriding `supports_response_schema() = true` → no downgrade token, schema preserved in the dispatched req.
  - A mock that drops `response_format` (returns it from `dropped_params`) → `param_dropped:response_format`, NOT `response_format_downgrade`.
- Gates: `cargo test` (changed crates + workspace); `cargo clippy --all-targets -- -D warnings`; `cargo fmt --check`; `cargo doc`.

## Out of scope
- B3 (`temperature_clamped`).
- Per-*model* schema granularity (per-provider for v1; e.g. OpenAI is treated as schema-capable across its models).
- Changing routing's capability gating (still keys on the binary `JsonMode`).
- Adding a `Capability::JsonSchema` variant / models.toml churn (rejected in favor of the trait method).
