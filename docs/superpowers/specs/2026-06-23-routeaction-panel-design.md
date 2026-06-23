# `RouteAction.panel` — org-configurable panel routes Design Spec

> Status: DRAFT (awaiting user review). Date: 2026-06-23. Repo: public (feature) + cloud (pin bump). Branch: `feat/routeaction-panel`.
> The deferred deep-research-panel follow-up (un-deferred 2026-06-23). Lets an org trigger + configure the deep-research panel via a routing rule, not just the `X-TokenTrimmer-Panel` header. Master spec: `2026-06-21-deep-research-panel-design.md` (opt-in surface row: "`RouteAction.panel` org config").
> **Decisions (user-approved 2026-06-23):** (1) **full panel config in the route** — `RouteAction.panel = { strategy, members?, arbiter?, quorum?, max_cost_usd? }`; (2) **header wins** — an explicit `X-TokenTrimmer-Panel` header overrides a matched route's panel.

## 1. Goal

Add a `panel` effect to `RouteAction` so a matched route triggers + configures the deep-research panel (the same fan-out + arbiter engine shipped in Phases 1–7). A route like *"`model_in: [gpt-4o]` → panel: 3-model synthesize with these members"* makes the panel an org policy, not a per-request header. Route-triggered panels flow through the **same** kill-switch, entitlement, budget, and billing path as header-triggered ones — only the *trigger + config source* differs.

## 2. Key facts (verified in code)

- **`RouteAction`** (`crates/routing/src/lib.rs:135–331`) is a struct of ~19 optional effects (`target_model`, `flex`, `compress`, `agentic_budget: Option<AgenticBudget>`, …). `AgenticBudget` (`:340`) is the precedent for a **self-contained nested struct mirrored across crates**.
- **`RouteAction` has a field-aligned mirror** `tt_plan_core::RouteAction` (`crates/plan-core/src/types.rs:258+`), kept in lockstep by per-field cross-type wire-compat tests (`route_action_cross_type_wire_compat` `:815`, `…_redact_…`, `…_batch_…`, `…_flex_…`, `…_canary_…`). **Any new field goes in BOTH structs + gets a wire-compat test.**
- **`validate_route_has_effect`** (`crates/routing/src/validate.rs:201–219`) — `has_effect = then.agentic_budget.is_some() || …`. A panel-only route (no `target_model`, only `panel`) must count as a real effect or it's rejected as a no-op.
- **Route → gateway flow:** `apply_routing` (`chat.rs:6276`) matches a route and projects `RouteAction` into the in-process `RouteMatch` struct (`chat.rs:6167`); effects are applied across `prepare` (`chat.rs:2410–2900`). `apply_routing` runs at `chat.rs:2400` — **before** the panel-resolution block (`chat.rs:2690`), so the matched `RouteMatch` is available there.
- **Panel trigger + config today** (`chat.rs:2690–2759`): `panel_from_header(headers)` → strategy; `PanelConfig::resolve(strategy, Option<&PanelExtras>, &PanelDefaults)` (`panel.rs:142`) merges strategy + `tt_extras.panel` + env defaults into a `PanelConfig {strategy, members, arbiter_model, quorum, max_cost_usd}`; then kill-switch (`PanelDisabled`) → entitlement (`Forbidden`) → budget gate (`402`) → per-member cred resolution → `prep.panel = Some(cfg)`. `complete_once` branches to `complete_panel` when `prep.panel` is `Some` (`chat.rs:1053`).
- **Cloud storage is JSONB.** `routes.target JSONB` (`cloud …/migrations/0002_routes.up.sql`); the cloud routes API stores `target` as an opaque `serde_json::Value` (`routes_admin.rs`) with **no field validation** — a new `panel` key needs **no migration and no cloud handler change**. The gateway's `PostgresRoutingStore` deserializes `target` JSONB → `RouteAction` via `serde_json::from_value` (serde defaults make old rows forward-compatible).
- **Cross-repo model:** cloud pins all public crates to one public-main SHA via git `rev` (`cloud/Cargo.toml`); `tt-routing` is among them. Public must merge first, then cloud bumps the pin. Cloud CI hits the Actions-minutes blocker → local pgvector DB test + admin-merge (per the ops runbook).

## 3. Decisions

- **D1 — `RoutePanel` is a self-contained nested struct (AgenticBudget pattern), mirrored in `tt_routing` + `tt_plan_core`:**
  ```rust
  pub struct RoutePanel {
      pub strategy: String,                  // "synthesize" | "best-of-n" | "majority"
      #[serde(default, skip_serializing_if = "Vec::is_empty")]
      pub members: Vec<String>,              // model ids; empty ⇒ env TT_PANEL_DEFAULT_MEMBERS
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub arbiter: Option<String>,           // arbiter model; None ⇒ env TT_PANEL_DEFAULT_ARBITER
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub quorum: Option<usize>,
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub max_cost_usd: Option<f64>,
  }
  ```
  `RouteAction.panel: Option<RoutePanel>` (after `agentic_budget`, both crates; `#[serde(default, skip_serializing_if = "Option::is_none")]`).
- **D2 — Header wins.** In the panel-resolution block, `panel_from_header(headers)` is checked **first**; the route's `panel` is consulted **only when the header is absent**. The header path is byte-for-byte unchanged. (Rationale: explicit per-request intent beats org default; keeps the shipped header behavior intact.)
- **D3 — Route-triggered panels use the same gates + engine.** A route-sourced panel runs through the identical kill-switch (`panel_enabled`), entitlement (`panel_min_tier`), budget gate, per-member cred resolution, and `complete_panel` path. The route only supplies the *strategy + config* (mapped into a `PanelExtras`); `PanelConfig::resolve` + all gates are unchanged.
- **D4 — `target_model` is inert when `panel` is set.** If a route sets both `panel` and `target_model`, the panel governs dispatch (`complete_panel` branches before single-model dispatch), so the rewrite never applies. Allowed (not an error) but documented; `validate_route_has_effect` already passes (panel is an effect).
- **D5 — Attribution reuses `matched_route_id`.** A route-triggered panel writes the same one `request_logs` row (`provider='panel'`, aggregate cost) with `matched_route_id` set — no new column. (No double routing: the panel members are dispatched by `complete_panel`, which is reached via the panel branch, not the single-model rewrite path.)
- **D6 — Validation lives in public.** `validate_route_has_effect` gains `|| then.panel.is_some()`; a new `validate_panel(then)` rejects an unparseable `strategy` and a `members` count over `TT_PANEL_MAX_MEMBERS` at route-creation time (mirrors `validate_shadow_model`/`validate_agentic_budget`). Cloud stays JSONB-opaque (no cloud validation).

## 4. Architecture

```
route stored in cloud routes.target JSONB:  { "panel": { "strategy":"synthesize","members":[...],"arbiter":"..." } }
   │  (gateway PostgresRoutingStore: target JSONB → RouteAction via serde; panel field needs the public bump)
   ▼
apply_routing (chat.rs:2400/6276) matches the route → RouteMatch { ..., panel: Option<RoutePanel> }   ← NEW field
   ▼
prepare panel-resolution block (chat.rs:2690), HEADER-WINS:
   let trigger = panel_from_header(headers).map(Header)               // existing path, unchanged
                 .or_else(|| route_match.panel.clone().map(Route));   // NEW: route fallback
   match trigger {
     Header(strategy)        → resolve with strategy + request tt_extras.panel  (existing)
     Route(rp: RoutePanel)   → resolve with rp.strategy + PanelExtras{rp.members,arbiter,quorum,max_cost}  (NEW)
     None                    → no panel
   }
   → SAME kill-switch → entitlement → budget gate → cred resolution → prep.panel = Some(cfg)
   ▼
complete_once (chat.rs:1053): prep.panel.is_some() → complete_panel (Phases 1-7 engine, unchanged)
```

## 5. Components & seams

### 5.1 `tt_routing` (`crates/routing/src/lib.rs`)
- Add `RoutePanel` struct (D1) near `AgenticBudget` (`:340`).
- Add `pub panel: Option<RoutePanel>` to `RouteAction` (after `agentic_budget`, `:330`), same serde attrs.

### 5.2 `tt_plan_core` mirror (`crates/plan-core/src/types.rs`)
- Mirror `RoutePanel` + `RouteAction.panel` at the matching position (`:403+`).
- Add `route_action_panel_cross_type_wire_compat` test (mirror `:815`/`:864`): a `RoutePanel`-bearing `RouteAction` serializes byte-identically across both crates; round-trips; omitted when `None`.

### 5.3 Validation (`crates/routing/src/validate.rs`)
- `validate_route_has_effect` (`:205`): add `|| then.panel.is_some()` to the `has_effect` chain.
- New `validate_panel(then: &RouteAction) -> Result<(), ValidationError>`: if `then.panel` is `Some`, require `strategy` parses to an `ArbiterStrategyKind` (reuse the parser) and `members.len() <= TT_PANEL_MAX_MEMBERS`; call it in the route-creation validation chain alongside the others.

### 5.4 Gateway `RouteMatch` + `apply_routing` (`crates/core/src/routes/chat.rs`)
- Add `pub(crate) panel: Option<tt_routing::RoutePanel>` to `RouteMatch` (`:6167`).
- Populate it from `m.then.panel.clone()` in `apply_routing`'s active-route arm (`:6479`). **Paused-route arm:** treat panel as a cost lever — a paused route does NOT trigger the panel (set `panel: None` on the paused projection, mirroring how cost levers are disabled when paused; SAFETY levers like redact stay on, panel is not a safety lever).

### 5.5 Panel-resolution merge (`crates/core/src/routes/chat.rs:2690`)
- Header-wins (D2): keep `panel_from_header(headers)` as the primary trigger; when it returns `None`, fall back to `route_match.panel`. Introduce a small `enum PanelTrigger { Header(ArbiterStrategyKind), Route(RoutePanel) }` (or inline) so the resolve step knows the extras source.
- For `Route(rp)`: `strategy = ArbiterStrategyKind::parse(&rp.strategy)` (skip the panel entirely if unparseable — defensive, shouldn't happen post-validation); build a `PanelExtras { members: rp.members, arbiter: rp.arbiter, quorum: rp.quorum, max_cost_usd: rp.max_cost_usd }`; call `PanelConfig::resolve(strategy, Some(&extras), &PanelDefaults::from_env())`.
- The kill-switch / entitlement / budget gate / cred-resolution that follow are **unchanged** and run identically for both trigger sources.

### 5.6 Cloud (separate PR, after public merges)
- **No code/schema change.** Bump every public-crate git `rev` in `cloud/Cargo.toml` to the public-main SHA that includes this feature; `cargo update` + `cargo build` + local pgvector `cargo test -p tt-api -- --include-ignored` + admin-merge (Actions-minutes blocker). The `target` JSONB already accepts the `panel` key; existing rows deserialize with `panel: None`.

## 6. Invariants (targeted by tests)
1. **Header path unchanged.** A request with `X-TokenTrimmer-Panel` behaves exactly as today regardless of any matched route (header wins; existing panel + header tests stay green).
2. **Route triggers the panel.** A request with no panel header that matches a route whose `RouteAction.panel = synthesize{members:[A,B]}` runs the panel (members A,B) and returns `tokentrimmer.panel`, identical in shape to a header-triggered panel.
3. **Same gates.** A route-triggered panel is subject to kill-switch (off ⇒ `403 PanelDisabled`), entitlement (below min ⇒ `403`), and budget (over/unpriceable ⇒ `402`) — a route cannot bypass them.
4. **Off-by-default.** No header + no matching panel route ⇒ byte-identical single-model path; a route without a `panel` field deserializes to `panel: None` (serde default) and changes nothing.
5. **Paused route ⇒ no panel.** A paused panel route does not trigger the panel (panel is a cost lever, off when paused).
6. **Lockstep.** `tt_routing::RouteAction` and `tt_plan_core::RouteAction` serialize identically for the `panel` field (wire-compat test).
7. **Validation.** A panel-only route (no target_model) is accepted (`has_effect`); a route with an unparseable strategy or too many members is rejected at creation.
8. **No billing change.** Route-triggered panels bill exactly like header-triggered ones (one `provider='panel'` row, aggregate cost, served once) with `matched_route_id` set.

## 7. Testing (TDD)
- **`RoutePanel` wire-compat unit** (plan-core): `route_action_panel_cross_type_wire_compat` — byte-identical serialize across crates; round-trip; omitted when `None`.
- **`validate_panel` unit** (routing): panel-only route passes `validate_route_has_effect`; unparseable strategy / `members > cap` rejected.
- **Route-triggered panel integration** (mirror `panel_engine.rs` + the routing test harness, e.g. `route_header.rs`): a route with `panel: synthesize{members:[mockA,mockB]}` + a matching request (no header) ⇒ 200, `tokentrimmer.panel` with those legs, one `provider='panel'` row, `matched_route_id` set.
- **Header-wins** integration: a request with `X-TokenTrimmer-Panel: best-of-n` that ALSO matches a `panel: synthesize` route ⇒ the panel runs `best-of-n` (header), not `synthesize` (route); the route's panel is ignored.
- **Gates on route panels**: kill-switch off ⇒ route panel ⇒ `403`; below-min tier ⇒ `403`; over-budget ⇒ `402` (zero dispatch).
- **Paused route** ⇒ panel does not trigger (single-model path).
- **Off-by-default regression**: existing routing + panel suites green; a no-panel route + no header ⇒ unchanged.

## 8. Out of scope
- Cloud schema/handler changes (JSONB passthrough; pin bump only).
- A dashboard UI for authoring panel routes (the cloud routes API already accepts the JSONB; dashboard is separate).
- Streaming-specific route-panel behavior — a route-triggered panel uses the same streaming path as a header-triggered one (Phase 5), no extra work.
- Any change to the panel engine, transcoders, billing, entitlement, or the arbiter strategies (Phases 1–7 reused unchanged).

## 9. Self-review
- **Placeholders:** none — `RoutePanel` shape, the seams (RouteAction/RouteMatch/apply_routing/prepare/validate), the header-wins merge, and the cloud pin-bump sequence are concrete + cited. The exact line of the `prepare` merge shifts with edits; it's anchored by `panel_from_header` + `apply_routing`.
- **Consistency:** `RoutePanel` follows the `AgenticBudget` self-contained-mirror precedent + the cross-type wire-compat lockstep discipline; route-triggered panels reuse `PanelConfig::resolve` + every gate unchanged; billing/engine untouched.
- **Scope:** public feature (routing + plan-core + core + validate + tests) is one plan; cloud is a mechanical pin bump (its own small PR after public merges). No migration.
- **Ambiguity:** richness (D1 full config), precedence (D2 header-wins), gate reuse (D3), target_model interaction (D4), attribution (D5), and validation location (D6) are each pinned to one behavior.
- **Cross-repo:** public-first (merge → SHA), then cloud bumps all git-dep `rev`s + local DB test + admin-merge; the JSONB column needs no migration and old rows stay valid via serde defaults.
