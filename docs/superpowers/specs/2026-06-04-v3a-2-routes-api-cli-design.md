# Design: V3a-2 — User-facing `/v1/routes` API + `tt route` CLI

_Date: 2026-06-04 · Status: approved design, pre-implementation · Repo: `public` (gateway `tt-core`, `tt-routing`, `tt` CLI)_

> Second slice of **V3 — Routing overhaul** (roadmap: `2026-06-03-cli-platform-roadmap.md`),
> building on V3a-1 (the `has_images`/`has_audio` engine). Gives users a way to
> **manage their own routes from the CLI** — the natural follow-on to V3a-1, which
> shipped the matcher but left route creation to the admin-only dashboard path.

## Problem

After V3a-1 the gateway *applies* modality routes, but the only way to *create*
a route is the cloud admin API (`/v1/admin/routes`) — gated by the shared
`TT_ADMIN_TOKEN` and requiring a caller-supplied `org_id`. That is a
platform-operator surface, not something an end user with a `tt_live_` key can
call. There is no user-authenticated routes API and no `tt route` CLI. V3a-2 adds
both, so a logged-in user (V0) can `tt route add …` and have it apply immediately.

## Current state (verified 2026-06-04)

- **Gateway router** (`crates/core/src/server.rs:37-60`): `build_router` composes
  `/health`, `/v1/models`, `/v1/chat/completions`, `/v1/embeddings`, `/v1/preview`
  into `base`, then layers `middleware::auth::middleware` over **all** of them — so
  any route added to `base` is authenticated.
- **Auth context** (`crates/auth/src/lib.rs:42-49`): `ApiKeyContext { key_id: Uuid,
  org_id: Uuid, tier: Option<CallerTier> }`, inserted as a request extension for a
  verified `tt_live_` key (`middleware/auth.rs:122`). With dogfood mode and no key,
  a context with `org_id == DOGFOOD_ORG_ID` is inserted (`auth.rs:130-135`);
  `tt_test_*` passes through to the sandbox. Route management must require a **real**
  key → reject absent/dogfood contexts.
- **RoutingStore** (`crates/routing/src/store.rs`): trait has **one** method,
  `list_for_org(org_id) -> Vec<Route>` (enabled-only; `PostgresRoutingStore` selects
  `WHERE enabled=TRUE` and hardcodes `enabled:true`). `InMemoryRoutingStore` has a
  test-only `set_routes`. **No create/get/delete.**
- **CachingRoutingStore** (`crates/routing/src/cache.rs`): per-org engine cache
  (60s TTL) wrapping `Arc<dyn RoutingStore>`; **already exposes
  `invalidate(org_id)`** (`:87`) — the comment even anticipates wiring it to a
  force-refresh endpoint. `AppState.routing_store: Option<Arc<CachingRoutingStore>>`.
- **Route type** (`tt_routing::Route`): `{ id, name, priority, enabled, when:
  RouteConditions, then: RouteAction }` — no `org_id`, no timestamps.
- **Validation** lives only in the cloud (`routes_admin.rs::validate_same_provider`,
  over untyped `serde_json::Value`). The gateway holds the full `ProviderRegistry`
  (the cloud does not), so it is the better home for the capability check.
- **CLI** has V0's `tt_cli::context::ResolvedContext` (key + base URL, default
  `api.tokentrimmer.com` = the gateway). No `tt route` command.

## Goals / non-goals

**Goals:** a user-authenticated `/v1/routes` CRUD on the **gateway** (org from key,
never caller-supplied); `RoutingStore` management methods + Postgres/InMemory impls;
shared typed validation (same-provider + modality→capability) usable by gateway and
(later) cloud; write-through cache invalidation so changes apply on the next
request; a `tt route list/show/add/rm` CLI with simple-rule flags.

**Non-goals (later):** route **editing** (PATCH) — v1 edit = `rm` + `add` (or a
later slice); dashboard exposure of `has_images`/`has_audio` (→ **V3a-3**, cloud);
refactoring the cloud admin endpoint onto the shared validator (nice-to-have,
follow-on); pagination (orgs have few routes); a `tt route` TUI (V1 styling slice).

## Architecture

`/v1/routes` lives in the **gateway** (chosen over cloud tt-api and a gateway→cloud
proxy): it already authenticates `tt_live_` keys and exposes `org_id`, the CLI
already targets it (one base URL, one key), and it holds the registry for capability
checks. Cost: add write methods to `RoutingStore`, share validation, invalidate the
cache on write.

### Component 1 — `RoutingStore` management methods (`tt-routing`)

Extend the trait; the hot-path `list_for_org` (enabled-only) is unchanged. New
methods get **default impls** returning `RoutingStoreError::Backend("management
unsupported")` so read-only stores are unaffected:

```rust
async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError>; // incl. disabled
async fn create_route(&self, org_id: Uuid, spec: NewRoute) -> Result<Route, RoutingStoreError>;
async fn get_route(&self, org_id: Uuid, id: Uuid) -> Result<Option<Route>, RoutingStoreError>;
async fn delete_route(&self, org_id: Uuid, id: Uuid) -> Result<bool, RoutingStoreError>;
```

`NewRoute { name, priority, enabled, when: RouteConditions, then: RouteAction }`
(server assigns `id`). Implement for `InMemoryRoutingStore` (HashMap) and
`PostgresRoutingStore` (INSERT/SELECT-all/DELETE, all `WHERE org_id=$1[/AND id=$2]`
for tenant isolation; conditions/target as JSONB). `CachingRoutingStore` overrides
the mutating methods to **delegate to `inner` then `self.invalidate(org_id)`**, and
`list_all_for_org`/`get_route` delegate straight through (management reads bypass the
hot-path engine cache).

### Component 2 — shared validation (`tt-routing`, new `validate` module)

```rust
pub fn validate_same_provider(when: &RouteConditions, then: &RouteAction) -> Result<(), ValidationError>;
pub fn validate_capability(when: &RouteConditions, then: &RouteAction,
    lookup: impl Fn(&str) -> Option<ModelInfo>) -> Result<(), ValidationError>;
```

- `validate_same_provider` ports the cloud logic to typed inputs using
  `tt_shared::providers::{infer_provider, known_to_differ}` — reject only when both
  sides are known-but-different (unknown/aggregator names pass, as today).
- `validate_capability`: when `when.has_images == Some(true)` **or** `has_audio ==
  Some(true)`, the `target_model` must list `Capability::Vision` — mirroring the
  runtime guard, which sets `vision=true` for image *and* audio inputs
  (`capability_check.rs:64`), so a route that validates here actually fires at
  runtime. Unknown target (lookup `None`) → permissive (allow), matching the runtime
  guard. The gateway supplies `lookup` from its `ProviderRegistry`; `tt-routing`
  gains no dependency on `tt-core`. (Finer `has_audio`→`Capability::Audio` is
  deferred alongside fixing the runtime guard's image/audio conflation.)

### Component 3 — gateway `/v1/routes` router (`tt-core`)

Added to `base` in `build_router` (so it's behind the auth middleware). Handlers
take `State<AppState>` + the `ApiKeyContext` extension:

| Method / path | Behavior |
|---|---|
| `GET /v1/routes` | list **all** the caller-org's routes (incl. disabled) |
| `POST /v1/routes` | validate + create; returns the created route (201) |
| `GET /v1/routes/{id}` | fetch one (404 if not the caller's / absent) |
| `DELETE /v1/routes/{id}` | delete one (404 if not the caller's / absent) |

Every handler: extract `ApiKeyContext`; **401** if absent or `org_id ==
DOGFOOD_ORG_ID` (real key required); **503** if `state.routing_store` is `None`
(routing not configured). `POST` runs `validate_same_provider` + `validate_capability`
(lookup = `state.registry`), writes via the `CachingRoutingStore` (which invalidates
the org cache), and the new route applies on the next chat request — no TTL wait.

### Component 4 — `tt route` CLI (`tt-cli`, new `route` module)

`tt route list | show <id> | rm <id> | add [flags]`, resolving key+base via
`ResolvedContext::load` (V0) and calling `{base}/v1/routes`:
- `--always <model>` — match-all pin (empty conditions, target = model).
- `--from <model> --to <model>` — `model_in:[from]`, target = to.
- `--when-has-images` / `--when-has-audio` — set the modality condition(s).
- `--priority N`, `--name <s>`, `--fallback <m>` (repeatable), `--disabled`.

Pure arg→`NewRoute`-JSON mapping is unit-tested without network. `list` prints a
plain table (id, name, priority, enabled, a one-line condition/target summary);
richer styling waits for V1.

## Data flow

`tt route add --from gpt-4o --to gpt-4o-mini --when-has-images`
→ `POST {base}/v1/routes` (Bearer = V0 key) → auth middleware stamps
`ApiKeyContext{org_id}` → handler validates (same-provider + vision-capable target
via registry) → `CachingRoutingStore.create_route(org_id, …)` → Postgres INSERT +
`invalidate(org_id)` → next image chat request matches and rewrites.

## Error handling

- Missing/dogfood/sandbox key → **401** ("route management requires a `tt_live_`
  key — run `tt login`"). No routing store → **503**.
- Cross-provider target → **422** (same-provider message). `has_images`/`has_audio`
  with a non-vision target → **422** naming the capability.
- `get`/`delete` of an id not owned by the caller's org → **404** (no cross-tenant
  signal). CLI surfaces non-2xx with status + body; missing key → V0's `tt login`
  hint.

## Testing (TDD; scoped `cargo test -p <crate>`)

- `tt-routing`: store management round-trips (InMemory create→list-all→get→delete;
  org isolation; delete-missing → false). `validate_*` unit matrix (same-provider
  pass/reject; modality→capability pass/reject/unknown-permissive).
- `tt-core`: gateway integration (mirror `route_content_type.rs` harness) — create
  via `POST` then a chat request honors it **without** a TTL wait (cache
  invalidation); 401 for no-key/dogfood; 422 for cross-provider + non-vision target;
  404 cross-org; list includes disabled.
- `tt-cli`: arg→`NewRoute` mapping per flag; error rendering for a non-2xx response.

## Success criteria

- After `tt login`, `tt route add --from A --to B [--when-has-images]` /
  `tt route add --always M` create working routes from the CLI; `tt route list`
  shows them; `tt route rm <id>` removes them — all org-scoped via the key, no
  `org_id` ever supplied by the caller.
- A newly-added route affects the next chat request immediately (cache invalidated).
- A `has_images` route cannot be created against a non-vision target; cross-provider
  is rejected.
- Hot-path `list_for_org` and existing routing/chat tests are unchanged/green.

## Out of scope (restated)

Route editing (PATCH); dashboard exposure (V3a-3, cloud); cloud admin endpoint
adopting the shared validator; pagination; CLI TUI/rich styling (V1); finer
audio-capability validation.

## Scope split

This spec is the **public-repo slice** (gateway API + CLI). Dashboard exposure of
the modality conditions is **V3a-3** (cloud) — the dashboard talks to the cloud
admin API, independent of this gateway endpoint.
