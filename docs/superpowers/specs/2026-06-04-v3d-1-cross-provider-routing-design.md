# V3d-1 — Cross-Provider Routing Design

**Status:** approved (design, revised after verification sweep)
**Date:** 2026-06-04
**Slice:** V3d-1 (first of two V3d sub-slices; V3d-2 = beyond-token preferences, deferred)
**Depends on:** V3b-2 (#17) merged to `main` — V3d-1 generalizes the same exemption #17 added for local targets.

## Goal

Let a route rewrite a request to a model on **any** provider (e.g. `gpt-4o` → `claude-haiku-4-5`), not just the same provider — and make it work correctly **end to end**, including upstream credential selection and failover chains.

## Background

`tt_routing::validate::validate_same_provider` rejects a route whose source (`when.model_in`) and `then.target_model` are known to be on different providers. This mirrors the conceptual "ADR-018" (same-provider only); there is **no ADR markdown file** — the label lives in doc-comments and the Plan design.

A pre-implementation verification sweep (10 agents) established what is already correct and surfaced a blocker the first draft missed:

**Already cross-provider-correct (no change needed):**
- **Dispatch** re-resolves the provider from the registry after a rewrite (`chat.rs:403-412`) and dispatches to the target provider — for single + failover, stream + non-stream (`failover.rs:181,276`).
- **Savings math**: the original model's pricing is captured *before* the rewrite (`chat.rs:396`) and used as the baseline; cost uses the rebound provider's pricing. Cross-provider savings are computed correctly at runtime.
- **Telemetry** attributes to the actual serving provider. **CLI**, **plan-core type mirror**, and **TS bindings** need no change. `known_to_differ`/`local_backend` stay compile-safe (still used by the registry).

**Blocker (the sweep's headline finding) — credentials are resolved for the wrong provider:**
`resolve_credentials` runs at `chat.rs:372`, **before** the rewrite/re-resolve, keyed by the *original* provider's id, and `ctx.credentials` is never rebuilt. So a `gpt-4o → claude-haiku` route dispatches to Anthropic carrying the **OpenAI** key (silent 401), or — when the org has no stored key — falls back to forwarding the customer's raw `tt_live_*` bearer to the upstream (`chat.rs:1477-1481`). The **failover path** shares this defect: it re-resolves the provider per candidate but passes the single original-provider `ctx` (`failover.rs:189,285`) — a latent bug that already mis-keys cross-provider *fallbacks* (incl. local routing) today. Cross-provider routing does not actually work until this is fixed on both paths.

**Plan replay misprices cross-provider targets (safe but misleading):**
`replay.rs:235` keys the target's pricing by `req.provider`, so a cross-provider target misses the table and is counted `requests_unprice_able` ($0 savings) rather than projecting real savings.

## Decision

**Always allow** cross-provider rewrites — delete the same-provider gate — and add the credential + replay work needed to make it correct:

- Routes are created deliberately; the user's intent ("route to this model") is explicit. The **capability guard stays** (`validate_capability`): a cross-provider target must still support a route's image/audio condition.
- **Fail closed on credentials**: the raw-bearer fallback is the source provider's key, so it is valid **only** when the dispatched provider equals the source provider. For any cross-provider target with no stored credential, return a clear error (primary path) or skip the candidate (failover) — never forward a source/`tt_live_` key to a different upstream.

Rejected alternatives: per-route `allow_cross_provider` opt-in (ceremony for the common case); warn-but-allow (no create-time warning channel).

## Changes

### 1. `tt-routing` — remove the gate
`crates/routing/src/validate.rs`:
- Delete `validate_same_provider` (body + doc) and `ValidationError::CrossProvider`.
- Delete the now-unused `use tt_shared::providers::known_to_differ;` import (line 6). Keep `use tt_shared::pricing::{Capability, ModelInfo};` (used by `validate_capability`). The `local_backend` call is fully-qualified inside the deleted fn — it disappears with the body, no import to remove.
- Delete the three obsolete unit tests: `same_provider_ok_and_cross_provider_rejected`, `unknown_models_pass_same_provider`, `local_target_is_exempt_from_same_provider`. Keep `has_images_requires_vision_target` and `no_modality_condition_skips_capability_check`.
- Update the module doc-comment (drop "Same-provider mirrors ADR-018").

`crates/routing/src/lib.rs`:
- Remove `validate_same_provider` from the `pub use validate::{…}` (keep `validate_capability`, `ValidationError`).
- Rewrite the `RouteAction.target_model` doc-comment (lines 84-86): cross-provider is allowed as of V3d-1; the target is capability-checked; dispatch/savings use the target's own provider. Point to this spec instead of "ADR-018".

### 2. `tt-core` — gateway accepts cross-provider routes
`crates/core/src/routes/routes_api.rs`:
- Remove the `validate_same_provider` import (line 10) and its call (lines 50-51) in `create`. Keep the `validate_capability` call.

### 3. `tt-core` — credentials follow the rewritten provider (primary path)
`crates/core/src/routes/chat.rs`:
- Capture `let source_provider_id = provider.id().to_string();` before routing (the provider resolved from the original `req.model`).
- Make `ctx` mutable (`let mut ctx`).
- After the post-route provider re-resolve (after line 411), when `provider.id() != source_provider_id`, re-resolve credentials for the **target** provider and rebuild `ctx.credentials`; **fail closed** if none exist:
  ```rust
  if provider.id() != source_provider_id {
      match resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, /*allow_bearer_fallback=*/ false).await {
          Some(c) => ctx.credentials = c,
          None => return Err(ApiError::MissingProviderCredential { provider: provider.id().to_string() }),
      }
  }
  ```
  This runs before the stream/non-stream branch (line 424), so it corrects **both** primary paths.
- Add `resolve_credentials_for(state, org_id, provider_id, raw_bearer, allow_bearer_fallback) -> Option<ProviderCredentials>`: store hit → `Some(c)`; store miss → `Some(bearer)` only if `allow_bearer_fallback`, else `None`. Refactor the existing `resolve_credentials` (line 1464, source-provider call at 372) to delegate with `allow_bearer_fallback=true` so its behavior is byte-identical for same-provider.
- Add `ApiError::MissingProviderCredential { provider: String }` mapping to **HTTP 400** with a clear message ("no upstream credential configured for provider `{provider}`, required by a matched route — add it before routing to this provider"). Wire it into the `ApiError` → response match (and any exhaustiveness sites).
- Update the stale same-provider comment at `chat.rs:404-405`.

### 4. `tt-core` / `failover.rs` — per-candidate credentials (failover path)
- The handler pre-resolves upstream credentials for every distinct provider in the candidate chain (both call sites, `chat.rs:529` stream and `chat.rs:863` non-stream), allowing the bearer fallback only for `source_provider_id`:
  ```rust
  let mut creds_by_provider: HashMap<String, ProviderCredentials> = HashMap::new();
  for model in &candidates {
      if let Some(p) = state.registry.resolve(model) {
          let pid = p.id().to_string();
          if !creds_by_provider.contains_key(&pid) {
              if let Some(c) = resolve_credentials_for(&state, org_id, &pid, &raw_bearer, pid == source_provider_id).await {
                  creds_by_provider.insert(pid, c);
              }
          }
      }
  }
  ```
- `dispatch_with_failover` and `dispatch_stream_with_failover` gain a `credentials_by_provider: &HashMap<String, ProviderCredentials>` parameter. Per candidate, after `registry.resolve(model)`: if the provider has no entry in the map, **skip** the candidate (log `failover_skip: no upstream credential`); otherwise build `let mut cand_ctx = ctx.clone(); cand_ctx.credentials = creds.clone();` and dispatch with `&cand_ctx` instead of the shared `&ctx`. (`RequestContext`/`ProviderCredentials` are `Clone`.) The existing exhaustion error covers "all candidates skipped".

### 5. Plan honesty — project cross-provider savings correctly
`crates/plan-core/src/replay.rs` `project_requests`:
- Build a deterministic `model → provider` index once, from the **sorted** pricing-table keys, splitting each key on its **first** `:` (`split_once(':')`; provider ids never contain `:`, models may — e.g. `llama3.1:8b`). First-insert-wins over sorted keys → deterministic on the (currently nonexistent) one-model-under-two-providers case.
- Resolve the target key with same-provider preference for byte-identical behavior:
  ```rust
  let target_provider = if pricing.contains_key(&pricing_key(&req.provider, &route.then.target_model)) {
      req.provider.as_str()                       // same-provider rewrite: unchanged key
  } else {
      model_to_provider.get(route.then.target_model.as_str()).copied().unwrap_or(req.provider.as_str())
  };
  let target_key = pricing_key(target_provider, &route.then.target_model);
  ```
  Same-provider rewrites keep the exact old key (snapshot stays byte-identical); a cross-provider target resolves to its own provider; a target absent from the table still misses → `requests_unprice_able` (conservative invariant intact).

### 6. Docs
- Update the ADR-018 / same-provider doc-comments in `validate.rs`, `lib.rs`, `plan-core/types.rs` (`RouteAction.target_model`, lines 149-152), and `chat.rs:404-405` to record that V3d-1 supersedes same-provider-only, pointing to this spec. No new ADR file (none exists).

## Testing

- **`tt-routing` (`validate.rs`):** delete the three same-provider rejection tests; keep the two capability tests.
- **`tt-core` integration (`routes_api.rs`):** invert `cross_provider_target_rejected` (lines 210-219) → assert a `gpt-4o → claude-haiku-4-5` route returns **201 Created**. Keep `has_images_non_vision_target_rejected` as proof the capability guard still rejects.
- **`tt-core` integration (`route_rewrite.rs`, new):**
  - A cross-provider route dispatches to the **target** provider and reports savings against the **original** model (register a second non-local mock provider). Both stream + non-stream.
  - A cross-provider route uses the **target provider's** credential, not the original key (credential store has the target's key) — the test that catches the §3 bug.
  - A cross-provider route whose org has **no** stored credential for the target returns `400 MissingProviderCredential` (not a silent dispatch).
  - A **failover** chain that crosses providers dispatches each candidate with its own provider's credential, and skips a candidate whose provider has no credential (§4).
- **`tt-plan-core` (`replay.rs`, new):**
  - `req.provider=openai`, route to `claude-haiku-4-5`, with **both** `openai:gpt-4o` and `anthropic:claude-haiku-4-5` priced → `requests_rerouted == 1`, `projected_savings_usd > 0` (was `requests_unprice_able`).
  - Companion: target pricing **absent** → still `requests_unprice_able == 1`.
  - Existing `snapshot_canned_replay` + determinism tests stay **byte-identical** (treat any drift as a regression, not a snapshot update).

## Out of Scope / Follow-ups

- **Cloud `routes_admin.rs`** has its own same-provider check — relax it (separate `cloud` repo; public-first, follows this slice).
- **V3d-2 — beyond-token preferences** (latency / quality-band / $-ceiling ranking): its own later slice.
- **Create-time soft warning** when a cross-provider route targets a provider the org has no credential for (runtime fail-closed is the must-have; the warning is a nicety).
- Optional "this route crosses providers" dashboard badge (could reuse `known_to_differ`).
