# V3d-1 — Cross-Provider Routing Design

**Status:** approved (design)
**Date:** 2026-06-04
**Slice:** V3d-1 (first of two V3d sub-slices; V3d-2 = beyond-token preferences, deferred)
**Depends on:** V3b-2 (#17) merged to `main` — V3d-1 generalizes the same exemption #17 added for local targets.

## Goal

Let a route rewrite a request to a model on **any** provider (e.g. `gpt-4o` → `claude-haiku-4-5`), not just the same provider. This is the last structural constraint on routing: V3b-2 already carved out a local-target exception to the same-provider rule; V3d-1 removes the rule entirely.

## Background

`tt_routing::validate::validate_same_provider` rejects a route whose `when.model_in` source and `then.target_model` are **known** to be on different providers (via `tt_shared::providers::known_to_differ`). This mirrors the conceptual "ADR-018" (same-provider only). There is **no ADR markdown file** — the label lives in doc-comments (`validate.rs`, `lib.rs`) and the Plan design.

Two facts make the relaxation small:

1. **The gateway already dispatches and prices cross-provider.** The provider registry resolves any registered model id to its provider; after a route rewrite the gateway re-resolves the target and dispatches there. Savings are computed as `baseline(original-model pricing) − cost(routed-model pricing)`, and both providers' pricing live in the same catalog. The **only** thing blocking cross-provider today is the create-time validation gate.

2. **Plan replay is already conservative on unknown pricing.** A target model with no pricing entry is counted "unchanged" (`requests_unprice_able`) — it never fabricates savings.

## Decision

**Always allow** cross-provider rewrites — drop the same-provider rejection entirely. Rationale:

- Routes are created deliberately; the user's intent ("route to this model, period") is explicit.
- The **capability guard stays** (`validate_capability`): a cross-provider target must still support a route's image/audio modality condition.
- The **runtime** is the correctness backstop for misconfiguration: if the org has no credential for the target provider, dispatch fails with a clear provider/credential error; an unregistered target yields `ModelNotFound`. These are runtime conditions, not create-time validation concerns.

Rejected alternatives: a per-route `allow_cross_provider` opt-in flag (more ceremony for the common case, extra schema/CLI/dashboard surface) and warn-but-allow (needs a create-time warning channel we don't have).

## Changes

### 1. `tt-routing` — remove the gate

`crates/routing/src/validate.rs`:
- Delete `validate_same_provider` and `ValidationError::CrossProvider`.
- Remove the now-unused imports (`known_to_differ`, `local_backend`).
- Keep `validate_capability` and `ValidationError::MissingCapability` unchanged.
- Update the module doc-comment (drop the "same-provider mirrors ADR-018" clause).
- Delete the obsolete tests: `same_provider_ok_and_cross_provider_rejected`, `unknown_models_pass_same_provider`, `local_target_is_exempt_from_same_provider`. Keep the capability tests.

`crates/routing/src/lib.rs`:
- Drop `validate_same_provider` from the `pub use validate::{…}`.
- Rewrite the `RouteAction.target_model` doc-comment: cross-provider is allowed as of V3d-1; the target is capability-checked, and savings/dispatch use the target's own provider. Replace the "same-provider only — see ADR-018" wording with a pointer to this spec.

### 2. `tt-core` — gateway accepts cross-provider routes

`crates/core/src/routes/routes_api.rs`:
- Remove the `validate_same_provider(&spec.when, &spec.then)?` call in `create` and drop the import. Keep the `validate_capability` call.
- No dispatch-path change: the rewrite + re-resolution + savings math already handle cross-provider.

### 3. `tt-plan-core` — project cross-provider savings correctly

`crates/plan-core/src/replay.rs`:
- **Problem:** `project_requests` builds the target's pricing key as `pricing_key(&req.provider, &route.then.target_model)` — it reuses the **original request's** provider. For a cross-provider target this key misses, so the request falls to `requests_unprice_able` (counted unchanged, $0 savings). Safe but misleading.
- **Fix:** build a deterministic `model → provider` index once per replay from the pricing-table keys, then key the target by **its own** provider:
  - `PricingTable` keys are `"{provider}:{model}"` (`pricing_key`), and provider names never contain `:`, so `key.split_once(':')` recovers `(provider, model)` unambiguously even when the model contains a colon (e.g. `ollama/llama3.1:8b`).
  - Build the index from **sorted** keys, first-insert wins. Sorting guarantees determinism if the (practically nonexistent) case of one model id under two providers ever occurs — without it, `HashMap` iteration order could change the winner and break the replay determinism contract.
  - Target lookup becomes: `let target_provider = index.get(target).copied().unwrap_or(req.provider.as_str()); let target_key = pricing_key(target_provider, target);`
- **Behavior preservation:** for a same-provider rewrite the target lives under `req.provider`, so the resolved key is identical to today → the JSON replay snapshot stays byte-identical. A target absent from the table still resolves to a miss → `requests_unprice_able` (conservative invariant intact).

### 4. Docs

- Update the ADR-018 references in `validate.rs` and `lib.rs` to record that V3d-1 supersedes same-provider-only routing, pointing to this spec. No new ADR file is created (none exists; the project tracks these as doc-comment concepts).

## Testing

- **`tt-routing` (`validate.rs`):** capability tests retained; same-provider rejection tests removed (the function is gone). No new validation test — absence of rejection is proven by the gateway integration test below.
- **`tt-core` integration (`routes_api`):** `POST /v1/routes` with `when.model_in=["gpt-4o"]`, `then.target_model="claude-haiku-4-5"` now returns **201 Created** (previously 400 InvalidRequest). A cross-provider route whose target lacks a required modality capability still returns 400 (capability guard intact).
- **`tt-core` integration (dispatch):** a request matching a cross-provider route dispatches to the target provider and the response reports savings computed against the **original** model's pricing (extends the existing route-rewrite/local-dispatch harness with a second non-local mock provider).
- **`tt-plan-core` (`replay.rs`):** a window of `openai`/`gpt-4o` requests with a route to `claude-haiku-4-5` (anthropic) and **both** pricings in the table projects **correct, non-zero** savings and increments `requests_rerouted` (not `requests_unprice_able`); a companion case with the target pricing **absent** still counts `requests_unprice_able`. The existing determinism snapshot stays green.

## Out of Scope / Follow-ups

- **Cloud `routes_admin.rs`** has its own same-provider check — relax it there too (separate `cloud` repo; public-first discipline, so it follows this slice).
- **V3d-2 — beyond-token preferences** (latency / quality-band / $-ceiling *ranking* of candidates): a different mechanism than first-match-by-priority; its own later slice.
- **Optional "this route crosses providers" info badge** in the dashboard/CLI — could reuse the now-freed `known_to_differ` (kept in `tt-shared` for this purpose).
