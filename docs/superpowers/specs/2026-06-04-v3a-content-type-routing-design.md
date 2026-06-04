# Design: V3a — Content-type routing + simple-rule ergonomics

_Date: 2026-06-04 · Status: approved design, pre-implementation · Repos: `public` (gateway + `tt` CLI + routing crate) and `cloud` (routes admin API + dashboard)_

> First slice of **V3 — Routing overhaul** (see `2026-06-03-cli-platform-roadmap.md`).
> Builds the extensible condition/action framework, ships the "simple rules made
> trivial" ergonomics, and adds the first new dimension: **input-modality routing**.

## Problem

Today's routing engine matches on four predicates only — `model_in`,
`input_tokens_lt`, `input_tokens_gt`, `tag_equals` — and can only be edited as raw
JSON in the dashboard or via the cloud admin API directly. Users asked for routing
that is *simple when they want it* ("always send everything to model X", "route
A→B") and *richer when they need it* — starting with routing by **what kind of
content a request carries**: send image/vision requests to one model, audio
requests to another. There is also no first-class CLI for routes, even though the
CLI is the primary surface and now has a credential it can authenticate with (V0).

## Current state (verified 2026-06-04)

- **Routing types** — `tt_routing::{Route, RouteConditions, RouteAction, RoutingEngine}`
  (`crates/routing/src/lib.rs:32-168`). `RouteConditions` fields are all
  `#[serde(default)]`; `matches()` (`:142-168`) AND-es them; empty/`None` matches
  anything. Comment at `:48-50` requires keeping it **in lockstep with
  `tt_plan_core::types::RouteConditions`**.
- **Plan-side mirror** — `tt_plan_core::types::RouteConditions`
  (`crates/plan-core/src/types.rs:114-126`) + `matches_conditions()`
  (`crates/plan-core/src/routing.rs:18`) replay against historical `RequestLog`
  rows (`model`, `input_tokens`, tag). **`RequestLog` carries no modality flags.**
- **Request content** — `tt_shared::messages::{MessageContent, ContentPart}`
  (`crates/shared/src/messages.rs:182-208`): `MessageContent::{Text(String),
  Parts(Vec<ContentPart>)}`; `ContentPart::{Text, ImageUrl{image_url}, InputAudio{input_audio}}`.
  **No `Video` variant** — video is not representable, hence out of scope.
- **Gateway application** — `apply_routing()` (`crates/core/src/routes/chat.rs:1592-1667`)
  runs **before** cache lookup, estimates input tokens, calls `engine.evaluate`, then
  a **capability guard** (`:1629-1652`) skips a route whose target lacks a capability
  the request requires (vision/audio/etc.), and rewrites `req.model`. Savings are
  attributed to `request_logs.matched_route_id`.
- **Same-provider constraint (ADR-007)** — enforced in
  `cloud/crates/api/src/routes_admin.rs::validate_same_provider` (`:39-64`) via
  `tt_shared::providers::infer_provider`. Documented only in code + the error
  message — **not yet in `DECISIONS.md`**.
- **Cloud routes CRUD** — `routes` table (`migrations/0002_routes.up.sql`) stores
  `conditions`/`target` as **JSONB** (no migration needed for new fields). Endpoints
  (`server.rs:185-193`): `POST/GET /v1/admin/routes`, `GET|PATCH|DELETE /v1/admin/routes/{id}`.
  **These are gated by `admin::require_admin` (shared `TT_ADMIN_TOKEN`) and require a
  caller-supplied `org_id`** — they are platform-operator endpoints, **not** a
  user-key-authenticated API. There is **no `/v1/routes` user endpoint** in the
  gateway today (only `/v1/chat/completions`, `/v1/embeddings`, `/v1/models`,
  `/v1/preview`, `/health`). A user-authenticated `tt route` CLI therefore needs a
  **new** `/v1/routes` endpoint that derives `org_id` from the `tt_live_` key.
- **CLI** — no `tt route` command; routes are dashboard/admin-API only. V0 added
  `tt_cli::context::ResolvedContext` (key + base URL resolution).
- **Dashboard** — `cloud/apps/dashboard/src/pages/routes/index.astro` lists routes
  and edits `conditions`/`target` as raw JSON.

## Goals / non-goals

**Goals:**
1. **Extensible framework (Approach A):** add modality predicates as additive,
   `#[serde(default)]` optional fields on `RouteConditions` (and its plan-core
   mirror) — no DB migration, backward-compatible.
2. **Input-modality routing:** `has_images` and `has_audio` conditions; the gateway
   matcher detects `ContentPart::ImageUrl` / `InputAudio` in the request.
3. **Simple-rule ergonomics:** a `tt route` CLI (`list`/`add`/`rm`/`show`) authed via
   V0, with flags that make the common rules one-liners (`--always`, `--from/--to`,
   `--when-has-images`, `--when-has-audio`).
4. **Dashboard exposure:** surface the new conditions + a one-click simple-rule
   quick-add on the existing routes page.
5. **Honesty:** capability-guard + create-time validation so an image route never
   targets a non-vision model; ADR-007 finally written into `DECISIONS.md`.

**Non-goals (later slices):** image/video **generation** endpoints & routing;
topic/keyword & privacy→local routing; cross-provider + beyond-token preferences;
a polished dashboard visual builder; boolean (AND/OR/NOT) condition trees;
semantic classification.

## Architecture

**Approach A — extend the flat conditions struct.** Chosen over a boolean predicate
tree (B, over-built, hard to give a simple UI) and a tagged enum list (C,
unnecessary). Additive fields keep JSONB rows and existing routes valid, match the
current engine, and let "simple vs complex" fall out of how many fields are set.

### Implementation slicing (revised after discovering routes CRUD is admin-only)

- **Plan 1 — content-type routing engine (public):** components 1–3 below + ADR +
  gateway integration test. Modality routes become live immediately for any route
  that can be created today (the dashboard's raw-JSON editor accepts the new
  fields). Pure-logic + integration; high confidence.
- **Plan 2 — user-facing routes API + CLI + dashboard (next):** a **new
  `/v1/routes` endpoint that derives `org_id` from the `tt_live_` key** (the admin
  CRUD requires the shared `TT_ADMIN_TOKEN` + a caller-supplied `org_id`, so it is
  not usable by an end-user CLI), then the `tt route` CLI (component 5) on top, plus
  cloud validation (component 4) and dashboard exposure (component 6).

### 1. Data model (`tt_routing` + `tt_plan_core`, in lockstep)

Add to **both** `RouteConditions` structs:

```rust
/// Match only if the request carries at least one image input part. None = ignore.
#[serde(default)]
pub has_images: Option<bool>,
/// Match only if the request carries at least one audio input part. None = ignore.
#[serde(default)]
pub has_audio: Option<bool>,
```

`Some(true)` = require the modality present; `Some(false)` = require it absent;
`None` = don't care. All conditions remain AND-ed.

### 2. Gateway matcher (`tt_routing::matches`)

Add a small pure helper (in `tt_routing`, fed the request) that scans
`req.messages[*].content` for `MessageContent::Parts` containing
`ContentPart::ImageUrl` (→ has-images) / `ContentPart::InputAudio` (→ has-audio),
and two match arms comparing the detected booleans to the conditions. The engine
signature is unchanged (it already receives `&ChatCompletionRequest`). The existing
capability guard in `apply_routing` is unchanged and complements this: a `has_images`
route whose target lacks `Vision` is skipped at run time.

### 3. Plan-side mirror (`tt_plan_core`)

Add the same fields for type lockstep / lossless config round-trip. **Limitation:**
`RequestLog` records no modality, so `matches_conditions` treats a request whose
modality is unknown as **not matching** a `has_images=Some(true)` / `has_audio=Some(true)`
condition (conservative — Plan never over-projects savings for modality rules). Noted
follow-up: capture `had_images`/`had_audio` on `request_logs` so Plan replay can
project these routes. Documented in the spec, not built in this slice.

### 4. Cloud validation (`routes_admin.rs`)

Extend create/patch validation: when `conditions.has_images == Some(true)`, the
`target.target_model` (and any `fallbacks`) must be vision-capable per the model
registry/`pricing`; same for `has_audio` ↔ audio capability. Reuse the existing
same-provider check unchanged. JSONB storage means **no migration**.

### 5. `tt route` CLI (new module `crates/cli/src/route/`)

Subcommands, all resolving key+base via `ResolvedContext::load` (V0) and calling the
cloud admin API:
- `tt route list` → `GET /v1/admin/routes` (table: name, priority, enabled,
  conditions summary, target, 24h hits).
- `tt route show <id>` → `GET /v1/admin/routes/{id}`.
- `tt route rm <id>` → `DELETE /v1/admin/routes/{id}`.
- `tt route add` → `POST /v1/admin/routes`, with simple-rule flags:
  - `--always <model>` — match-all pin (empty conditions, target = model).
  - `--from <model> --to <model>` — `model_in:[from]`, target = to.
  - `--when-has-images` / `--when-has-audio` — set the modality condition(s).
  - `--when-tokens-lt N` / `--when-tokens-gt N`, `--priority N`, `--name <s>`,
    `--fallback <model>` (repeatable), `--disabled`.
  Pure arg→`CreateRouteRequest` mapping is unit-tested without network.

### 6. Dashboard (`routes/index.astro` + controller)

Expose `has_images`/`has_audio` as checkboxes in the route editor, render them in the
conditions summary, and add a "quick add" row for the two simplest rules (pin-all,
A→B). Full visual builder remains a later slice.

## Data flow

`tt route add --from gpt-4o --to gpt-4o-mini --when-has-images`
→ CLI builds `CreateRouteRequest` → `POST /v1/admin/routes` (bearer = V0 key)
→ cloud validates (same-provider + vision-capable target) → INSERT JSONB row
→ gateway's per-org `CachingRoutingStore` refreshes → next image chat request:
`apply_routing` detects `has_images`, matches, capability-guard OK, rewrites model
→ savings logged against `matched_route_id`.

## Error handling

- Cross-provider target → existing ADR-007 error (unchanged).
- `has_images=true` with a non-vision target → **rejected at create/patch** (clear
  message naming the capability) **and** skipped at run time (capability guard) —
  defense in depth.
- Unknown/malformed model id → validation error (reject blank; provider inference
  stays permissive for genuinely-new ids, consistent with today).
- CLI: surface cloud API non-2xx with status + body message; missing key → the V0
  "run `tt login`" guidance.

## Testing (TDD; scoped `cargo test -p <crate>` / dashboard vitest)

- `tt_routing`: matcher matrix — has-images/has-audio × present/absent/None ×
  combined-with-model_in; empty-conditions still matches; detection helper over
  `Text` vs `Parts` messages.
- `tt_plan_core`: new fields parse; modality condition does not match a
  no-modality `RequestLog`.
- `core` integration: an image-bearing chat request matches a `has_images` route and
  is rewritten + attributed; a text-only request does not; capability guard still
  skips a non-vision target.
- `cloud` `routes_admin`: validation accepts vision target, rejects non-vision target
  for `has_images`; same-provider unchanged.
- `tt route` CLI: arg→request mapping for each simple-rule flag; error rendering.

## Success criteria

- `tt route add --always <model>` and `tt route add --from A --to B [--when-has-images]`
  create working routes from the CLI with only a prior `tt login`.
- An image/vision chat request is routed per a `has_images` rule (verified by
  `matched_route_id` + savings); text-only traffic is unaffected.
- A `has_images` route cannot be created against a non-vision model.
- New fields round-trip through cloud JSONB and the plan-core mirror with no
  migration and no breakage of existing routes/tests.
- ADR-007 is recorded in `DECISIONS.md`.

## Out of scope (restated)

Image/video generation endpoints & routing; topic/keyword & privacy→local; cross-
provider & non-token preferences; dashboard visual builder; boolean condition trees;
semantic classification; capturing modality in `request_logs` (noted follow-up for
Plan projection).
