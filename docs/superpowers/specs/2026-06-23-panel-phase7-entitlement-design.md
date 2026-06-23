# Deep Research Panel — Phase 7 (entitlement + rollout + docs) Design Spec

> Status: IMPLEMENTED (Phase 7 — branch feat/panel-phase7-entitlement). Date: 2026-06-23. Repo: public. Branch: `feat/panel-phase7-entitlement`.
> The FINAL phase of the deep-research-panel campaign. Master spec: `2026-06-21-deep-research-panel-design.md` (roadmap row 7: "CallerTier entitlement gate; RouteAction.panel org config; 04-gateway-api-reference docs; kill-switch ops runbook").
> **Scope decisions (user-approved 2026-06-23):** (1) entitlement = a **configurable `TT_PANEL_MIN_TIER`, default allow-all** (ships the gate mechanism, no-op until set or cloud tiers land); (2) **public-only** — `RouteAction.panel` org config (the cloud piece) is **deferred** to a tracked follow-up.

## 1. Goal

Finalize the three reachable, public Phase-7 deliverables and close the campaign's public scope:
1. **CallerTier entitlement gate** — replace Phase 1's default-allow with a real, configurable tier gate: a panel request from a caller below `TT_PANEL_MIN_TIER` gets `403 Forbidden`. Default `Free` (allow-all), so the panel keeps working today behind the existing kill-switch + per-request budget; an operator (or cloud, once tier-injection lands) can tighten it.
2. **Agent-loop `record_request_served` unify** — close the long-standing gap where each dispatched agent-loop turn writes a `request_logs` row but never bumps `tt_requests_served_total` (`agent_run.rs:853`).
3. **Docs + kill-switch runbook** — document the panel in `docs/04-gateway-api-reference.md` (header, `tt_extras.panel`, the `tokentrimmer.panel` response shape across all three ingresses, billing, entitlement, kill-switch) and add a `.claude/ops` runbook for the `TT_PANEL_ENABLED` / `TT_PANEL_MIN_TIER` rollout.

**Deferred (tracked follow-up):** `RouteAction.panel` org config (`crates/routing/src/lib.rs:135` + cloud org-config storage). The panel is fully usable via the `X-TokenTrimmer-Panel` header without it; the org-config piece pulls in the cloud repo + the cloud CI Actions-minutes blocker.

## 2. Key facts (verified in code)

- **`CallerTier`** (`crates/shared/src/context.rs:19`) = `Free, Pro, Team, Scale`; derives `Debug, Clone, Copy, PartialEq, Eq` — **NOT `Ord`**. (Free/Pro/Team/Scale is the natural rank, but Pro and Team share a TTL band, so a *global* `Ord` would assert a fuzzy Pro<Team ordering other code might misread — prefer a local panel-rank helper, §5.1.)
- **The caller tier is already resolved and available in `prepare`.** `tt_auth::ApiKeyContext.tier: Option<CallerTier>` (`crates/auth/src/lib.rs:50`) is populated by the gateway `TierResolver` (`crates/core/src/tier_resolver.rs`, Postgres-backed, **fail-open to `Free`** on error / no subscription row). `prepare` captures it as `caller_tier: Option<CallerTier>` (`chat.rs:915`, set at `:1082`). **Today it resolves to `Free` for ~all traffic** (cloud tier-injection `rv-tier-limits-enforcement` not fully wired) — which is exactly why a non-`Free` default would block everyone, hence the `Free` default.
- **Errors exist:** `ApiError::Forbidden(String)` → `403` (`error.rs:27/118`); `ApiError::PanelDisabled` → `403` (the existing kill-switch rejection, `chat.rs:329`).
- **Kill-switch pattern to mirror:** `panel_enabled_from_env()` (`server.rs:49`, reads `TT_PANEL_ENABLED`) → `AppState.panel_enabled: bool` (`state.rs:328`, default `false`) + `AppState::with_panel_enabled(bool)` builder (`state.rs:615`). Parsing the env **once at AppState build** (not per-request) is the established pattern and avoids env-var races in tests.
- **Agent-loop served gap:** the chat handler bumps `record_request_served("chat","dispatch")` (`chat.rs:2251`) / `("chat","cache_hit")` (`:2240`); SSE bumps `("sse","dispatch")` (`sse.rs:1308`). The agent-loop `GatewayCompleter::complete` `Dispatched` arm (`agent_run.rs:853`) bumps nothing, though cache is disabled per turn so it **always** dispatches — a per-turn under-count. `record_request_served(path, result)` (`metrics.rs:65`) has bounded labels `path ∈ {chat, sse, embeddings}` today. (Note: a panel header on `POST /v1/agent/runs` is **not** stripped, so it *can* reach per-turn `prepare`→`complete_panel` — but `complete_panel` returns `Dispatched` and the new per-turn `agent_run` bump fires exactly once per turn, matching its one `request_logs` row; the panel served-once invariant therefore still holds, and this fix is orthogonal to it.)
- **Docs target:** `docs/04-gateway-api-reference.md` exists (alongside `01-inspect-rule-catalog.md`, `routing-rules-guide.md`, etc.).

## 3. Decisions

- **D1 — `TT_PANEL_MIN_TIER`, parsed once at AppState build, default `Free` (allow-all).** Add `panel_min_tier_from_env() -> CallerTier` (`server.rs`, mirrors `panel_enabled_from_env`; parses `free|pro|team|scale` case-insensitively; unknown/unset ⇒ `Free`) → `AppState.panel_min_tier: CallerTier` (default `Free`) + `AppState::with_panel_min_tier(CallerTier)` builder. The gate reads `state.panel_min_tier` — no per-request env read.
- **D2 — Local panel-tier rank, not a global `Ord`.** A `fn panel_tier_rank(t: CallerTier) -> u8` (`Free=0, Pro=1, Team=2, Scale=3`) in the panel module; the gate is `panel_tier_rank(caller) >= panel_tier_rank(state.panel_min_tier)`. Keeps the entitlement ordering panel-local (no shared-crate derive change asserting a fuzzy global tier order).
- **D3 — Gate placement + semantics.** The entitlement check runs in the panel-resolution block of `prepare` (where the `X-TokenTrimmer-Panel` trigger is detected and `panel_budget_gate` runs), **after** the kill-switch check (`panel_enabled`) and **before** dispatch. Order with the existing checks: kill-switch (`PanelDisabled`) → **entitlement (`Forbidden`)** → budget gate (`402`). A caller with `caller_tier` (None ⇒ treated as `Free`) below `state.panel_min_tier` ⇒ `Err(ApiError::Forbidden("panel: requires <tier> tier or higher"))`, returned before any dispatch/billing. With the default `Free` min, the check is a no-op (everyone passes). This replaces the Phase-1 default-allow note.
- **D4 — Agent-loop served fix.** Add `crate::metrics::record_request_served("agent_run", "dispatch")` in the `agent_run.rs:853` `Dispatched` arm (once per dispatched turn), and extend the `metrics.rs` bounded-label doc to include `agent_run`. New label value, additive (cardinality +1).
- **D5 — Off-by-default.** A non-panel request never reaches the entitlement check (it's inside the panel-resolution block, gated on the header trigger). The default `Free` min makes the check a no-op even for panel requests until configured. The served fix only adds a counter bump on the agent-loop dispatch path (no behavior change). Docs/runbook are additive files.

## 4. Architecture

```
panel request (X-TokenTrimmer-Panel header) ─ prepare panel-resolution block:
   1. kill-switch:  if !state.panel_enabled            → Err(PanelDisabled)        [403, existing]
   2. ENTITLEMENT:  if rank(caller_tier ?? Free) < rank(state.panel_min_tier)
                                                        → Err(Forbidden)            [403, NEW — D1/D2/D3]
   3. budget gate:  panel_budget_gate(...)              → Err(402 over-budget)      [existing]
   4. resolve PanelConfig + creds → run the panel (Phases 1-6)

agent loop (orthogonal): GatewayCompleter::complete Dispatched arm (agent_run.rs:853)
   + record_request_served("agent_run","dispatch")  per dispatched turn            [NEW — D4]

AppState build:  panel_enabled = TT_PANEL_ENABLED (existing)
                 panel_min_tier = TT_PANEL_MIN_TIER (default Free)                  [NEW — D1]
```

## 5. Components & seams

### 5.1 `panel_tier_rank` + `TT_PANEL_MIN_TIER` (`panel.rs` + `server.rs` + `state.rs`)
- `panel.rs`: `pub(crate) fn panel_tier_rank(t: CallerTier) -> u8 { match t { Free=>0, Pro=>1, Team=>2, Scale=>3 } }`.
- `server.rs`: `pub fn panel_min_tier_from_env() -> CallerTier` — `std::env::var("TT_PANEL_MIN_TIER")`, lowercased, match `"pro"=>Pro, "team"=>Team, "scale"=>Scale, _=>Free` (unset/unknown ⇒ `Free`, with a `warn` on an unrecognized non-empty value). Re-export via `crate::lib` like `panel_enabled_from_env`.
- `state.rs`: add `pub panel_min_tier: CallerTier` to `AppState` (default `CallerTier::Free` in the constructor) + `pub fn with_panel_min_tier(mut self, t: CallerTier) -> Self`. Production wires it from `panel_min_tier_from_env()` where `with_panel_enabled` is wired.

### 5.2 Entitlement gate (the panel-resolution block in `prepare`, `chat.rs`)
At the panel-resolution site (where `panel_from_header` triggered and the kill-switch `panel_enabled` is checked), after the kill-switch and before `panel_budget_gate`, add:
```rust
let caller = caller_tier.unwrap_or(tt_shared::CallerTier::Free);
if panel::panel_tier_rank(caller) < panel::panel_tier_rank(state.panel_min_tier) {
    return Err(ApiError::Forbidden(format!(
        "panel: requires {:?} tier or higher", state.panel_min_tier
    )));
}
```
Remove/replace the Phase-1 default-allow TODO note at that site. `caller_tier` is the value `prepare` already captures (`chat.rs:1082`).

### 5.3 Agent-loop served fix (`agent_run.rs:853`, `metrics.rs`)
In the `CompletionOutcome::Dispatched { .. }` arm at `agent_run.rs:853` (after extracting usage, before returning `Ok`): `crate::metrics::record_request_served("agent_run", "dispatch");`. Update the `record_request_served` doc comment (`metrics.rs:61-62`) bounded-label list to `chat|sse|embeddings|agent_run`.

### 5.4 Docs (`docs/04-gateway-api-reference.md`) + runbook (`.claude/ops`)
- Add a **Deep Research Panel** section to `04-gateway-api-reference.md` mirroring the structure of the other feature sections: the `X-TokenTrimmer-Panel: synthesize|best-of-n|majority` header; the `tt_extras.panel` config object (members / arbiter / quorum / max_cost_usd) and where it's accepted (`/v1/chat/completions`, `/v1/responses`; header-only on `/v1/messages`); the `tokentrimmer.panel` response object (body on non-streaming, trailing SSE event on streaming) + the per-leg shape; aggregate billing (one `request_logs` row, `cost = Σ legs + arbiter`, `cached=false`); entitlement (`TT_PANEL_MIN_TIER`) + kill-switch (`TT_PANEL_ENABLED`); the per-request budget requirement (`X-TokenTrimmer-Cost-Limit-Usd`, fail-closed).
- Add `.claude/ops/panel-rollout.md`: a runbook for enabling the panel — set `TT_PANEL_ENABLED=1`, choose `TT_PANEL_MIN_TIER`, the `TT_PANEL_MAX_MEMBERS` cap, the kill-switch-off rollback, and what to watch (the `tokentrimmer.panel.*` metrics + the aggregate cost). **Doc only** — the runbook describes the infra steps; actually flipping the production env is the user-gated infra action (per the infra-writes-user-gated convention), not performed here.

## 6. Invariants (targeted by tests)
1. **Default allow-all.** With `panel_min_tier` unset (= `Free`), a panel request from any caller (incl. `None`/`Free`) is NOT rejected on entitlement — the panel runs (regression: existing panel suites stay green).
2. **Gate bites when configured.** With `with_panel_min_tier(Pro)`, a `Free` (or `None`) caller's panel request ⇒ `403 Forbidden`, returned before any dispatch/billing (zero `request_logs` rows); a `Pro`/`Team`/`Scale` caller passes.
3. **Order: kill-switch → entitlement → budget.** A kill-switched-off request still returns `PanelDisabled` (403) regardless of tier; entitlement is checked only when enabled.
4. **Off-by-default for non-panel.** A request with no panel header never hits the entitlement check (no behavior change).
5. **Served parity.** An N-turn agent run bumps `tt_requests_served_total{path="agent_run",result="dispatch"}` exactly N times (one per dispatched turn), matching its N `request_logs` rows.
6. **No money-path change.** No change to billing, the panel engine, or cost accounting.

## 7. Testing (TDD)
- **Entitlement default allow-all** (router/panel harness, `with_panel_enabled(true)`, no `with_panel_min_tier`): a panel request from a `Free`/`None`-tier caller returns `200` and renders `tokentrimmer.panel` (mirror the Phase-6 panel render test). Pins invariant 1.
- **Entitlement gate bites** (`with_panel_min_tier(CallerTier::Pro)`): a `Free`/`None`-tier caller's panel request ⇒ `403`, body is the `Forbidden` envelope, and a call-counter mock asserts **zero** upstream dispatches + zero `request_logs` rows; a `Pro` caller ⇒ `200`. (Use the tier-injection pattern from `crates/core/tests/tier_enforcement.rs`.) Pins invariant 2.
- **Order** test: `with_panel_enabled(false)` + `with_panel_min_tier(Pro)` + `Free` caller ⇒ `PanelDisabled` (403), not the entitlement `Forbidden` (kill-switch checked first). Pins invariant 3.
- **`panel_tier_rank` / `panel_min_tier_from_env`** unit tests: rank ordering Free<Pro<Team<Scale; env parse `"pro"/"PRO"/"team"/"scale"/unknown/unset` → expected `CallerTier` (unknown/unset ⇒ Free).
- **Agent-loop served** test: drive a multi-turn agent run (mirror the agent_run loop tests) and assert `tt_requests_served_total{path="agent_run"}` incremented once per dispatched turn (use the metrics-recorder assertion pattern from existing metrics tests; if direct metric assertion isn't wired in the harness, assert via a recorder snapshot or note the gate is the unit-level call-site test).
- **Off-by-default regression**: the full existing panel + agent_run + chat suites stay green.

## 8. Out of scope
- `RouteAction.panel` org config + any cloud-repo change (deferred follow-up; tracked in memory).
- Cloud tier-injection (`rv-tier-limits-enforcement`) — orthogonal; the gate consumes whatever tier the resolver provides.
- Overage cost-multiplier pricing for panels (Phase-3 business decision, already deferred).
- Flipping production env (`TT_PANEL_ENABLED`) — user-gated infra; the runbook documents it.
- Any change to the panel engine, transcoders, billing, or the three arbiter strategies (Phases 1–6 reused unchanged).

## 9. Self-review
- **Placeholders:** none — env parsing, the rank helper, the gate snippet, the served one-liner, and the doc/runbook contents are concrete; the one read-at-impl detail (the exact line of the panel-resolution block, which shifted across P5/P6 merges) is a cited seam the implementer locates via `panel_from_header` + `panel_budget_gate`.
- **Consistency:** `panel_min_tier` mirrors the proven `panel_enabled` env/state/builder pattern exactly; the gate ordering (kill-switch → entitlement → budget) is explicit; the served fix mirrors the chat/sse call sites.
- **Scope:** public-only, one plan, no cloud change; `RouteAction.panel` explicitly deferred.
- **Ambiguity:** default-allow semantics (D1), local rank vs global Ord (D2), gate placement/order (D3), and the served label (D4) are each pinned to one behavior.
- **Risk:** the entitlement gate is inert by default (min=Free) and only meaningful once an operator sets `TT_PANEL_MIN_TIER` or cloud injects non-Free tiers — this is intended (ships the mechanism without breaking today's Free-resolved traffic); documented in the runbook.
