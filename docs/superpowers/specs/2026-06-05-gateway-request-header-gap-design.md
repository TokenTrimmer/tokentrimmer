# Gateway Request-Header Gap (Safe Subset + Doc-Fix) Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** Post-roadmap follow-up #4 (final). Closes the documented-but-unhonored request/response header gap.

## Goal

`docs/04-gateway-api-reference.md` §6.1/§6.2 advertise request and response headers
the gateway silently ignores — a trust/correctness gap. The gateway today reads
only `X-TokenTrimmer-Tag` and `X-TokenTrimmer-Trace-Id`, and on the response side
sets the six cost/trace headers + `X-TokenTrimmer-Cache`.

Close the gap honestly:
- **Implement** the two headers that cleanly reuse existing machinery and are
  low-risk: `X-TokenTrimmer-Cost-Limit-Usd` (request → 402) and
  `X-TokenTrimmer-Latency-Ms` (response).
- **Doc-fix** every remaining unimplemented header to "Planned — not yet honored".

## Scope

**In:** the two headers above, on **both** the chat (`/v1/chat/completions`) and
embeddings (`/v1/embeddings`) handlers (the header contract is gateway-wide;
implementing on only one recreates the gap); shared `pub(crate)` helpers in
`chat.rs`; the doc edits; tests.

**Out (doc-fixed as "Planned"):** actually honoring `X-TokenTrimmer-Cache`,
`-Route`, `-Provider`, `-Fallback`, `-Timeout-Ms`, `-Trace-Parent` (request) and
`X-TokenTrimmer-Route-Matched`, `-Warnings` (response). Each is real
feature/adapter work (cache-decision wiring, route-by-name, provider override with
cross-provider credentials, fallback chains, `RequestContext.deadline` enforcement
in adapters) for a future slice.

## Part A — Shared helpers (`crates/core/src/routes/chat.rs`)

Three small `pub(crate)` helpers (so the embeddings handler reuses them):

```rust
/// Parse `X-TokenTrimmer-Cost-Limit-Usd` (a positive USD ceiling), if present.
pub(crate) fn cost_limit_from_header(headers: &HeaderMap) -> Option<f64> {
    headers
        .get("x-tokentrimmer-cost-limit-usd")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
}

/// Reject with 402 when the estimated request cost exceeds the header limit.
/// Permissive when pricing is unknown (can't prove an exceedance) — same
/// semantics as the route `max_cost_usd` ceiling.
pub(crate) fn enforce_cost_limit(
    limit: Option<f64>,
    pricing: Option<&ModelPricing>,
    input_tokens: u32,
    max_tokens: Option<u32>,
) -> ApiResult<()> {
    if let (Some(limit), Some(pr)) = (limit, pricing) {
        let est = estimate_cost_usd(pr, input_tokens, max_tokens);
        if est > limit {
            return Err(ApiError::CostLimitExceeded {
                estimated_usd: est,
                ceiling_usd: limit,
            });
        }
    }
    Ok(())
}

/// Attach `X-TokenTrimmer-Latency-Ms` from a request-start instant.
pub(crate) fn attach_latency_header(headers: &mut HeaderMap, started: std::time::Instant) {
    let ms = started.elapsed().as_millis();
    if let Ok(v) = ms.to_string().parse() {
        headers.insert("x-tokentrimmer-latency-ms", v);
    }
}
```

(`estimate_cost_usd`, `ApiError`, `ApiResult`, `ModelPricing` are already in scope
in `chat.rs`; `last_user_message_text` is already defined there.)

## Part B — Chat handler wiring

1. **Cost limit.** After routing (right after the existing route `max_cost_usd`
   block), before dispatch:
   ```rust
   let cost_limit = cost_limit_from_header(&headers);
   let cl_input_tokens = last_user_message_text(&req)
       .map(|s| tt_tokenize::estimate_tokens(provider.id(), s))
       .unwrap_or(0);
   enforce_cost_limit(cost_limit, provider.pricing(&req.model).as_ref(), cl_input_tokens, req.max_tokens)?;
   ```
   This applies to every request (routed or not), independent of any route ceiling.
   It is checked on the final (post-routing) model + provider.

2. **Latency.** Immediately after each `attach_cost_headers(...)` call in the
   handler (dispatch, cache-hit, sandbox paths — `request_started` is in scope),
   add `attach_latency_header(<resp>.headers_mut(), request_started);`.

## Part C — Embeddings handler wiring (`routes/embeddings.rs`)

1. Add `let request_started = std::time::Instant::now();` at the top.
2. **Cost limit.** After the routing block, before dispatch:
   ```rust
   let cl_input_tokens = tt_tokenize::estimate_tokens(provider.id(), &input_as_text(&req.input));
   enforce_cost_limit(
       cost_limit_from_header(&headers),
       provider.pricing(&req.model).as_ref(),
       cl_input_tokens,
       None, // embeddings have no output tokens
   )?;
   ```
   (Import the helpers from `crate::routes::chat`.)
3. **Latency.** After the dispatch `attach_cost_headers(...)` call and in
   `sandbox_embeddings_response`, add `attach_latency_header(http.headers_mut(), request_started)`.
   (Thread `request_started` into `sandbox_embeddings_response`.)

## Part D — Doc-fix (`docs/04-gateway-api-reference.md`)

Edit §6.1 (request headers) and §6.2 (response headers) so the table reflects
reality. Add a **Status** column (or an inline "(planned)" marker) per row:
- §6.1 honored: `X-TokenTrimmer-Tag`, `X-TokenTrimmer-Cost-Limit-Usd`.
- §6.1 planned (not yet honored): `X-TokenTrimmer-Cache`, `-Route`, `-Provider`,
  `-Fallback`, `-Timeout-Ms`, `-Trace-Parent`. (The gateway does read an
  `X-TokenTrimmer-Trace-Id` request header for trace continuity, but that header
  isn't in the §6.1 table, so leave the table to the eight documented rows.)
- §6.2 honored: `Trace-Id`, `Provider`, `Model-Used`, `Cache`, `Cost-Usd`,
  `Baseline-Cost-Usd`, `Saved-Usd`, `Latency-Ms`.
- §6.2 planned: `Route-Matched`, `Warnings`.
- Correct the line "Customers can rely on these headers being present in every
  response (success or error)" to accurately state that the cost/latency/trace
  headers are attached on responses that reach dispatch/cache/sandbox, and are not
  guaranteed on early validation errors (4xx before dispatch).

## Error handling

- A malformed/zero/negative `Cost-Limit-Usd` → ignored (treated as no limit), not
  an error (lenient parsing, matching how `Tag` is read).
- Over-limit → `402 cost_limit_exceeded` with `{estimated_usd, ceiling_usd}` (the
  existing `ApiError::CostLimitExceeded` body).
- `attach_latency_header` is best-effort (a parse failure simply omits the header).

## Testing

**Chat (`crates/core/src/server.rs`, `app_with_mock`)** — mock pricing is $1/M
input, $2/M output:
- `cost_limit_header_rejects_over_limit`: a chat request with `max_tokens` set and
  `X-TokenTrimmer-Cost-Limit-Usd: 0.0000001` → `402`, body code `cost_limit_exceeded`.
- `cost_limit_header_allows_under_limit`: same request, `X-TokenTrimmer-Cost-Limit-Usd: 100`
  → `200`.
- `latency_header_present`: a normal dispatch → response has `x-tokentrimmer-latency-ms`,
  parseable as a non-negative integer.

**Embeddings** — `app_with_mock`, model `mock-model-1`:
- `embeddings_cost_limit_rejects_over_limit`: `X-TokenTrimmer-Cost-Limit-Usd: 0.0000001`
  → `402`.
- `embeddings_latency_header_present`: dispatch response has `x-tokentrimmer-latency-ms`.

**Regression:** all existing chat + embeddings tests stay green (the helpers are
additive; the cost-limit check is a no-op without the header).

**Gates:** `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`;
`cargo test -p tt-core`; `cargo deny check advisories`.

## Out of scope

- Honoring the "Planned" headers (separate future slices).
- Setting cost/latency headers on early-error (pre-dispatch 4xx) responses — the
  docs are corrected to match current behavior instead.
- Any change to the SDK (`tt-client` doesn't send these request headers yet;
  adding `.cost_limit()` to the builder is a possible follow-up, not this slice).
