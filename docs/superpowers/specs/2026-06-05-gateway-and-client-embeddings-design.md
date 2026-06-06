# Gateway + Client Embeddings Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** Post-roadmap follow-up #3. Implements `/v1/embeddings` end-to-end (gateway dispatch with routing) and adds embeddings to the `tt-client` SDK.
**Depends on:** the OpenAI adapter's real `embeddings()` (exists), `tt-client` chat (#36).

## Goal

Make `POST /v1/embeddings` real (it currently returns `501`) — a routed, billing-correct, single round-trip — and add a typed `embed()` to the SDK. Ship as one PR.

Most infrastructure already exists and is reused, not duplicated:
- The OpenAI native adapter implements `embeddings()` (`crates/providers/openai/src/lib.rs`); the OpenAI-compat layer does too. Other providers keep their `Unsupported` stubs.
- `text-embedding-3-small` ($0.02/M) and `-large` ($0.13/M) are already priced in `crates/shared/data/pricing.toml` (output rate 0).
- `compute_cost`, `attach_cost_headers`, `resolve_credentials`, `resolve_credentials_for`, `estimate_cost_usd`, `apply_routing`, and `RouteMatch` exist in `crates/core/src/routes/chat.rs` (currently private to that module).

The only genuinely new code: the embeddings **handler** (replacing the 501) and the SDK **`embed()`** method.

## Scope

**In:** model→provider resolution, credential resolution (per-org store + raw-Bearer passthrough), **routing** (model rewrite + cost-condition + post-rewrite `max_cost_usd` ceiling, cross-provider credential re-resolve), provider dispatch, cost computation, `x-tokentrimmer-*` headers, spend recording, the SDK method.

**Out (deferred):** L1/L2 cache (embeddings aren't cached here yet — the L2 semantic cache is a separate internal consumer), streaming (N/A for embeddings), failover chains (route `fallbacks` — single-provider dispatch only; a route that sets fallbacks still rewrites/gates, but the embeddings path does not iterate a candidate chain), flipping the other providers' `Unsupported` stubs.

## Part A — Make chat helpers reusable (`crates/core/src/routes/chat.rs`)

Change the visibility of these items from private to `pub(crate)` (no logic change):
`apply_routing`, `estimate_cost_usd`, `compute_cost`, `attach_cost_headers`,
`resolve_credentials`, `resolve_credentials_for`, and the `RouteMatch` struct
(and its fields, if not already `pub`). All existing chat tests must remain green
— this is a pure visibility change.

Rationale: these are gateway-dispatch helpers, not chat-specific. `pub(crate)`
keeps the diff minimal and avoids moving billing-critical code; the embeddings
handler imports them via `crate::routes::chat::…`.

## Part B — `ChatCompletionRequest: Default` (`crates/shared/src/messages.rs`)

Add `#[derive(Default)]` to `ChatCompletionRequest`. Every field is `Default`-able
(`String`/`Vec`/`Option`/`bool`/`HashMap`). This lets the embeddings handler build
a synthetic request for routing as `ChatCompletionRequest { model, messages, ..Default::default() }`.
A unit test asserts the default is empty (`model == ""`, `messages` empty,
`stream == false`, `tools` empty).

## Part C — Embeddings handler (`crates/core/src/routes/embeddings.rs`)

Replace the 501 stub with a routed dispatch that mirrors chat's non-streaming
path (minus cache/streaming/failover). Pseudostructure:

```text
handler(State(state), headers, Json(mut req): EmbeddingsRequest):
  trace_id = new uuid; (org_id, api_key_id) from the auth extension (same as chat)
  provider  = state.registry.resolve(&req.model) or 404 ModelNotFound
  source_provider_id = provider.id()
  raw_bearer = bearer from Authorization header
  credentials = resolve_credentials(&state, org_id, provider.id(), &raw_bearer).await
  ctx = RequestContext { trace_id, org_id, api_key_id, credentials, tag: header tag, deadline: None }

  requested_pricing = provider.pricing(&req.model)          // BEFORE routing → baseline

  // --- routing via synthetic chat request ---
  let mut synth = ChatCompletionRequest {
      model: req.model.clone(),
      messages: vec![Message::User { content: Text(input_as_text(&req.input)), name: None }],
      ..Default::default()                                   // max_tokens None
  };
  let route_match = apply_routing(&state, &ctx, &mut synth).await;
  req.model = synth.model;                                   // adopt the routed model
  if route_match matched:
      provider = state.registry.resolve(&req.model) or 404
      if provider.id() != source_provider_id:               // cross-provider: fail closed
          ctx.credentials = resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, false).await
                            or return 4xx MissingProviderCredential
      if let Some(ceiling) = route_match.max_cost_usd:       // V3d-2b ceiling
          if let Some(pr) = provider.pricing(&req.model):
              if estimate_cost_usd(&pr, route_match.input_tokens_estimate, None) > ceiling:
                  return 402 CostLimitExceeded

  // --- dispatch ---
  let resp = provider.embeddings(req.clone(), &ctx).await?;  // ProviderError → ApiError via IntoResponse

  // --- cost + headers + spend ---
  // Price the served (post-routing) model; baseline against the original.
  let routed_pricing = provider.pricing(&req.model);
  let (cost_usd, baseline_cost_usd) = compute_cost(&resp.usage, routed_pricing.as_ref(), requested_pricing.as_ref(), <fee_multiplier — copy chat's exact compute_cost arg>);
  let saved_usd = (baseline_cost_usd - cost_usd).max(0.0);
  state.spend_sink().record(org_id, cost_usd, Utc::now());

  let mut http = (StatusCode::OK, Json(resp)).into_response();
  attach_cost_headers(http.headers_mut(), trace_id, provider.id(), &req.model, cost_usd, baseline_cost_usd, saved_usd);
  http
```

Notes:
- **`input_as_text(&EmbeddingInput)`**: `Single(s) → s.clone()`; `Batch(v) → v.join("\n")`.
  Used only to build the synthetic routing request (token estimate + prompt-contains).
- **Baseline pricing** is the originally-requested model's pricing (`requested_pricing`),
  so a downgrade route surfaces a positive `saved_usd` (same contract as chat).
- **`fee_multiplier`**: pass whatever chat passes to `compute_cost` (read the chat
  call site for the exact argument; reuse it verbatim).
- **`model-used` header**: `req.model` after routing (the actually-served model).
- The exact org/api-key extraction and tag-header read must be copied from the chat
  handler verbatim so auth/attribution behave identically.

## Part D — SDK `embed()` (`crates/client`)

Add (in `lib.rs` or a small `embeddings.rs` module):

- Re-export `EmbeddingsRequest, EmbeddingsResponse, EmbeddingData, EmbeddingInput` from `tt_shared::messages`.
- `EmbedOutcome { pub response: EmbeddingsResponse, pub cost: CostInfo }` (`Debug, Clone`), with `pub fn vectors(&self) -> impl Iterator<Item = &[f32]>` (over `response.data`, ordered by `index` as returned).
- `impl Client`:
  ```rust
  pub async fn embed(&self, model: impl Into<String>, input: EmbeddingInput) -> Result<EmbedOutcome>;
  ```
  Builds `{ "model", "input" }` (dimensions/encoding_format omitted for v1), POSTs `/v1/embeddings` with bearer; parses `CostInfo` from headers; non-2xx → `Error::Status { cost: Box::new(parse_cost(...)) }`; success → decode `EmbeddingsResponse` (`Error::Decode` on failure). `model.trim().is_empty()` → `Error::MissingModel`, matching `chat()`.

  (A fluent builder isn't warranted — embeddings have few options. A plain method
  mirrors the SDK's `parse_cost`/error conventions. `dimensions`/`encoding_format`
  can be added later without breaking the signature if needed via an `embed_with`
  variant — out of scope now.)

## Error handling

- Unknown model → `404 ModelNotFound` (registry miss).
- Non-embeddings provider (e.g. Anthropic) → `provider.embeddings()` returns
  `ProviderError::Unsupported` → existing `ApiError`→HTTP mapping (`error.rs`).
- Cross-provider route with no stored target credential → fail closed
  (`MissingProviderCredential`), never forwarding the source key.
- Route ceiling exceeded → `402 CostLimitExceeded`.
- SDK: `MissingModel` / `Request` / `Status{…, cost}` / `Decode` (the existing enum).

## Testing

**Gateway (`crates/core`)** — using the existing `app()` mock harness + a mock
provider whose `embeddings()` returns canned vectors + `Usage`:
- Replace `embeddings_returns_501_not_implemented` with
  `embeddings_dispatch_returns_200_with_headers`: POST a valid request → `200`,
  body is a valid `EmbeddingsResponse`, all six `x-tokentrimmer-*` headers present,
  and the org's spend was recorded (assert via the spend sink, mirroring any chat
  dispatch test).
- `embeddings_routes_and_reports_savings`: an org route rewrites the embedding
  model to a cheaper one → `x-tokentrimmer-model-used` is the routed model and
  `x-tokentrimmer-saved-usd > 0` (baseline priced against the original).
- `embeddings_unknown_model_404` and (if a fixture exists) a non-embeddings
  provider → mapped error.
- All pre-existing chat tests stay green (Part A/B are behavior-preserving).

**SDK (`crates/client`)** — httpmock:
- `embed_returns_vectors_and_cost`: mock returns an `EmbeddingsResponse` + cost
  headers → `out.vectors()` yields the rows, `out.cost.cost_usd` parsed, model_used
  present.
- `embed_surfaces_status_error`: `501`/`500` → `Error::Status`.
- `embed_without_model_errors`: empty model → `Error::MissingModel`, no network.

**Gates:** `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`;
`cargo test -p tt-core -p tt-client` and the workspace suite; `cargo deny check
advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-client --no-deps`.

## Out of scope

- Caching embeddings; failover chains for embeddings; `dimensions`/`encoding_format`
  passthrough in the SDK; flipping non-OpenAI providers' `Unsupported` stubs;
  CLI surface for embeddings (`tt embed`) — all later.
- Generalizing the `RoutingEngine` to a non-`ChatCompletionRequest` input (the
  synthetic-request adapter is the deliberate, lower-risk seam for this slice).
