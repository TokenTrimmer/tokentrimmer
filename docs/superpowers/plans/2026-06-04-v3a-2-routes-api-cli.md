# V3a-2 — User-facing `/v1/routes` API + `tt route` CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a logged-in user manage their own routes from the CLI: a gateway `/v1/routes` CRUD (org-from-key) backed by new `RoutingStore` management methods + shared validation, plus a `tt route list/show/add/rm` command.

**Architecture:** `/v1/routes` lives in the gateway behind the existing auth middleware, so `org_id` comes from the verified `tt_live_` key (never caller-supplied). The `RoutingStore` trait gains management methods (default impls keep read-only stores untouched); `CachingRoutingStore` writes through and calls its existing `invalidate(org_id)` so changes apply on the next request. Validation (same-provider + modality→capability) is shared typed code in `tt-routing`, fed the gateway's `ProviderRegistry`.

**Tech Stack:** Rust workspace — `tt-routing`, `tt-core` (axum), `tt-cli` (clap + reqwest). No new dependencies (`reqwest` already used by the CLI proxy path).

**Repo / branch:** `/Users/iansimon/Developer/TokenTrimmer/public` on `feat/v3a-2-routes-api` (based on the V3a-1 engine; rebase onto `main` after PR #13 lands). Spec: `docs/superpowers/specs/2026-06-04-v3a-2-routes-api-cli-design.md`.

**Test note:** `cargo test --workspace` is hook-denied — scope with `-p`. Rust "red" = a compile error referencing a not-yet-defined item.

**Verified anchors:**
- `RoutingStore` trait + impls: `crates/routing/src/store.rs` (only `list_for_org`; `InMemoryRoutingStore.inner: RwLock<HashMap<Uuid, Vec<Route>>>`; `PostgresRoutingStore` behind `#[cfg(feature="postgres")]`, maps a `RouteRow`).
- `CachingRoutingStore`: `crates/routing/src/cache.rs` — `inner: Arc<dyn RoutingStore>`, `invalidate(&self, org_id)` (`:87`), impls `RoutingStore`.
- `Route { id, name, priority: u32, enabled, when: RouteConditions, then: RouteAction }`; `RouteConditions`/`RouteAction` are `serde` + `Default` (`crates/routing/src/lib.rs`).
- `tt_shared::providers::{infer_provider, known_to_differ}`; `tt_shared::pricing::{Capability, ModelInfo}`.
- Gateway: `build_router` (`crates/core/src/server.rs:37-60`) layers `middleware::auth::middleware` over `base`. `AppState { registry: Arc<ProviderRegistry>, routing_store: Option<Arc<CachingRoutingStore>>, … }`. `ProviderRegistry::model_info(id) -> Option<&ModelInfo>` (`registry.rs:43`). Handler pattern: `State<AppState>` + `Json<T>` (`routes/models.rs`). `ApiKeyContext` read as `Option<Extension<ApiKeyContext>>` (`routes/chat.rs:295`), `ApiKeyContext { key_id, org_id, tier }` (`tt_auth`); dogfood uses `crate::DOGFOOD_ORG_ID`.
- `ApiError` (`crates/core/src/error.rs:16-40`) variants today: `InvalidRequest`, `Unauthorized`, `PaymentRequired`, `Forbidden`, `ModelNotFound`, `RateLimited`, `Provider`, `Internal` — **no generic 404/503**.
- CLI HTTP pattern: `reqwest::Client`, `.post(url).json(&v).bearer_auth(key).send().await` (`crates/cli/src/proxy/preview.rs:57-66`); `main` is `#[tokio::main] async`.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/routing/src/store.rs` (modify) | `NewRoute`; trait management methods (default = unsupported); `InMemoryRoutingStore` impl + tests. |
| `crates/routing/src/cache.rs` (modify) | `CachingRoutingStore` management methods: delegate + `invalidate` on mutation; test. |
| `crates/routing/src/validate.rs` (create) | `ValidationError`, `validate_same_provider`, `validate_capability` + tests. |
| `crates/routing/src/lib.rs` (modify) | `pub mod validate;` + re-export `NewRoute`, validation fns. |
| `crates/routing/src/store.rs` pg (modify) | `PostgresRoutingStore` management SQL (compile-checked under `postgres`). |
| `crates/core/src/error.rs` (modify) | `ApiError::NotFound(String)` (404) + `ServiceUnavailable(String)` (503). |
| `crates/core/src/routes/routes_api.rs` (create) | 4 handlers (list/create/get/delete). |
| `crates/core/src/routes/mod.rs` (modify) | `pub mod routes_api;`. |
| `crates/core/src/server.rs` (modify) | mount the 4 routes on `base`. |
| `crates/core/tests/routes_api.rs` (create) | end-to-end CRUD + auth + validation + cache-invalidation tests. |
| `crates/cli/src/route/mod.rs` (create) | `build_new_route` (pure) + `run` (HTTP) + output. |
| `crates/cli/src/lib.rs` (modify) | `pub mod route;`. |
| `crates/cli/src/main.rs` (modify) | `Command::Route` + dispatch. |

---

## Task 1: `RoutingStore` management methods + `InMemoryRoutingStore` impl

**Files:** Modify `crates/routing/src/store.rs`

- [ ] **Step 1: Write the failing tests** — append to the `#[cfg(test)] mod tests` block in `store.rs`:

```rust
    #[tokio::test]
    async fn in_memory_create_list_get_delete() {
        let s = InMemoryRoutingStore::new();
        let org = Uuid::now_v7();
        let spec = NewRoute {
            name: "pin".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions::default(),
            then: RouteAction { target_model: "m1".into(), fallbacks: vec![], force_cache_layer: None },
        };
        let created = s.create_route(org, spec).await.unwrap();
        assert_eq!(created.name, "pin");

        let all = s.list_all_for_org(org).await.unwrap();
        assert_eq!(all.len(), 1);

        let got = s.get_route(org, created.id).await.unwrap();
        assert_eq!(got.unwrap().id, created.id);

        assert!(s.delete_route(org, created.id).await.unwrap());
        assert!(s.get_route(org, created.id).await.unwrap().is_none());
        assert!(!s.delete_route(org, created.id).await.unwrap());
    }

    #[tokio::test]
    async fn in_memory_management_is_org_scoped() {
        let s = InMemoryRoutingStore::new();
        let org_a = Uuid::now_v7();
        let org_b = Uuid::now_v7();
        let created = s
            .create_route(org_a, NewRoute {
                name: "a".into(), priority: 1, enabled: true,
                when: RouteConditions::default(),
                then: RouteAction { target_model: "m".into(), fallbacks: vec![], force_cache_layer: None },
            })
            .await
            .unwrap();
        // org_b cannot see or delete org_a's route.
        assert!(s.get_route(org_b, created.id).await.unwrap().is_none());
        assert!(!s.delete_route(org_b, created.id).await.unwrap());
        assert_eq!(s.list_all_for_org(org_b).await.unwrap().len(), 0);
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-routing store::tests` → FAIL (`NewRoute` / `create_route` undefined).

- [ ] **Step 3: Add `NewRoute` + trait methods** — in `store.rs`, after the imports add:

```rust
use crate::{Route, RouteAction, RouteConditions};

/// Fields needed to create a route; the store assigns the `id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewRoute {
    pub name: String,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub when: RouteConditions,
    pub then: RouteAction,
}

fn default_priority() -> u32 {
    100
}
fn default_enabled() -> bool {
    true
}
```

(Replace the existing `use crate::Route;` line with the combined `use crate::{Route, RouteAction, RouteConditions};`.)

Then add management methods to the `RoutingStore` trait (after `list_for_org`), each with a default that read-only stores inherit:

```rust
    /// Management: ALL of an org's routes, including disabled ones.
    async fn list_all_for_org(&self, _org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        Err(RoutingStoreError::Backend("management unsupported by this store".into()))
    }
    /// Management: create a route for `org_id`. Returns the created route.
    async fn create_route(&self, _org_id: Uuid, _spec: NewRoute) -> Result<Route, RoutingStoreError> {
        Err(RoutingStoreError::Backend("management unsupported by this store".into()))
    }
    /// Management: fetch one route owned by `org_id`.
    async fn get_route(&self, _org_id: Uuid, _id: Uuid) -> Result<Option<Route>, RoutingStoreError> {
        Err(RoutingStoreError::Backend("management unsupported by this store".into()))
    }
    /// Management: delete one route owned by `org_id`. Returns whether a row was removed.
    async fn delete_route(&self, _org_id: Uuid, _id: Uuid) -> Result<bool, RoutingStoreError> {
        Err(RoutingStoreError::Backend("management unsupported by this store".into()))
    }
```

- [ ] **Step 4: Implement for `InMemoryRoutingStore`** — add these methods to its `impl RoutingStore for InMemoryRoutingStore` block (alongside `list_for_org`):

```rust
    async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        let g = self.inner.read().expect("inmemory routing store poisoned");
        Ok(g.get(&org_id).cloned().unwrap_or_default())
    }

    async fn create_route(&self, org_id: Uuid, spec: NewRoute) -> Result<Route, RoutingStoreError> {
        let route = Route {
            id: Uuid::now_v7(),
            name: spec.name,
            priority: spec.priority,
            enabled: spec.enabled,
            when: spec.when,
            then: spec.then,
        };
        let mut g = self.inner.write().expect("inmemory routing store poisoned");
        g.entry(org_id).or_default().push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, org_id: Uuid, id: Uuid) -> Result<Option<Route>, RoutingStoreError> {
        let g = self.inner.read().expect("inmemory routing store poisoned");
        Ok(g.get(&org_id).and_then(|v| v.iter().find(|r| r.id == id).cloned()))
    }

    async fn delete_route(&self, org_id: Uuid, id: Uuid) -> Result<bool, RoutingStoreError> {
        let mut g = self.inner.write().expect("inmemory routing store poisoned");
        let Some(v) = g.get_mut(&org_id) else { return Ok(false) };
        let before = v.len();
        v.retain(|r| r.id != id);
        Ok(v.len() != before)
    }
```

- [ ] **Step 5: Run to verify it passes** — `cargo test -p tt-routing store::tests` → PASS (existing + 2 new).

- [ ] **Step 6: Commit**

```bash
git add crates/routing/src/store.rs
git commit -m "feat(routing): RoutingStore management methods (NewRoute, create/get/delete/list-all) + InMemory impl"
```

---

## Task 2: `CachingRoutingStore` write-through + invalidate

**Files:** Modify `crates/routing/src/cache.rs`

- [ ] **Step 1: Write the failing test** — append to `cache.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn create_invalidates_so_engine_sees_it() {
        use crate::{RouteAction, RouteConditions};
        let backing = Arc::new(InMemoryRoutingStore::new());
        let org = Uuid::now_v7();
        let cache = CachingRoutingStore::with_ttl(
            backing as Arc<dyn RoutingStore>,
            Duration::from_secs(3600), // long TTL: only invalidation can refresh
        );
        // Warm the (empty) cache.
        assert_eq!(cache.engine_for(org).await.unwrap().routes().len(), 0);
        // Create through the caching store.
        cache
            .create_route(org, crate::store::NewRoute {
                name: "x".into(), priority: 10, enabled: true,
                when: RouteConditions::default(),
                then: RouteAction { target_model: "m".into(), fallbacks: vec![], force_cache_layer: None },
            })
            .await
            .unwrap();
        // Without invalidation the long-TTL cache would still say 0.
        assert_eq!(cache.engine_for(org).await.unwrap().routes().len(), 1);
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-routing cache::tests::create_invalidates` → FAIL (uses the default "unsupported" trait impl → `create_route` errors, `.unwrap()` panics).

- [ ] **Step 3: Implement the management methods on `CachingRoutingStore`** — add to its `impl RoutingStore for CachingRoutingStore` block (after `list_for_org`):

```rust
    async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        self.inner.list_all_for_org(org_id).await
    }

    async fn create_route(
        &self,
        org_id: Uuid,
        spec: crate::store::NewRoute,
    ) -> Result<Route, RoutingStoreError> {
        let created = self.inner.create_route(org_id, spec).await?;
        self.invalidate(org_id).await;
        Ok(created)
    }

    async fn get_route(&self, org_id: Uuid, id: Uuid) -> Result<Option<Route>, RoutingStoreError> {
        self.inner.get_route(org_id, id).await
    }

    async fn delete_route(&self, org_id: Uuid, id: Uuid) -> Result<bool, RoutingStoreError> {
        let removed = self.inner.delete_route(org_id, id).await?;
        if removed {
            self.invalidate(org_id).await;
        }
        Ok(removed)
    }
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-routing cache::tests` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/routing/src/cache.rs
git commit -m "feat(routing): CachingRoutingStore write-through management + cache invalidation"
```

---

## Task 3: Shared validation (`tt-routing::validate`)

**Files:** Create `crates/routing/src/validate.rs`; modify `crates/routing/src/lib.rs`

- [ ] **Step 1: Declare the module + write the failing tests** — in `crates/routing/src/lib.rs`, after `pub mod store;` add `pub mod validate;`. Create `crates/routing/src/validate.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::pricing::{Capability, ModelInfo};
    use crate::{RouteAction, RouteConditions};

    fn action(target: &str) -> RouteAction {
        RouteAction { target_model: target.into(), fallbacks: vec![], force_cache_layer: None }
    }
    fn vision_model(id: &str) -> ModelInfo {
        ModelInfo { id: id.into(), provider: "p".into(),
            capabilities: vec![Capability::Text, Capability::Vision],
            max_input_tokens: 1000, max_output_tokens: 1000 }
    }
    fn text_model(id: &str) -> ModelInfo {
        ModelInfo { id: id.into(), provider: "p".into(),
            capabilities: vec![Capability::Text], max_input_tokens: 1000, max_output_tokens: 1000 }
    }

    #[test]
    fn same_provider_ok_and_cross_provider_rejected() {
        let when = RouteConditions { model_in: vec!["gpt-4o".into()], ..Default::default() };
        assert!(validate_same_provider(&when, &action("gpt-4o-mini")).is_ok());
        assert!(validate_same_provider(&when, &action("claude-haiku-4-5")).is_err());
    }

    #[test]
    fn unknown_models_pass_same_provider() {
        let when = RouteConditions { model_in: vec!["llama-3.3-70b".into()], ..Default::default() };
        assert!(validate_same_provider(&when, &action("qwen-2.5-72b")).is_ok());
    }

    #[test]
    fn has_images_requires_vision_target() {
        let when = RouteConditions { has_images: Some(true), ..Default::default() };
        let lookup = |m: &str| -> Option<ModelInfo> {
            match m { "vis" => Some(vision_model("vis")), "txt" => Some(text_model("txt")), _ => None }
        };
        assert!(validate_capability(&when, &action("vis"), lookup).is_ok());
        assert!(validate_capability(&when, &action("txt"), lookup).is_err());
        // Unknown target is permissive (mirrors runtime guard).
        assert!(validate_capability(&when, &action("unknown"), lookup).is_ok());
    }

    #[test]
    fn no_modality_condition_skips_capability_check() {
        let when = RouteConditions::default();
        let lookup = |_: &str| -> Option<ModelInfo> { None };
        assert!(validate_capability(&when, &action("anything"), lookup).is_ok());
    }
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-routing validate` → FAIL (compile: `validate_same_provider` undefined).

- [ ] **Step 3: Implement** — add above the test module in `validate.rs`:

```rust
//! Typed route validation shared by the gateway routes API (and, later, the
//! cloud admin endpoint). Same-provider mirrors ADR-018; the capability check
//! mirrors the runtime guard (`tt_shared::capability_check`).

use tt_shared::pricing::{Capability, ModelInfo};
use tt_shared::providers::{infer_provider, known_to_differ};

use crate::{RouteAction, RouteConditions};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("cross-provider rewrite rejected: `{src}` and `target_model={target}` are on different providers; v1 routing is same-provider only (ADR-018)")]
    CrossProvider { src: String, target: String },
    #[error("target_model `{target}` is missing the `{capability}` capability required by this route's content-type condition")]
    MissingCapability { target: String, capability: &'static str },
}

/// Reject only when both the source model and the target are known-but-different
/// providers. Unknown / aggregator-routed names pass (no false positive).
pub fn validate_same_provider(
    when: &RouteConditions,
    then: &RouteAction,
) -> Result<(), ValidationError> {
    for src in &when.model_in {
        if known_to_differ(src, &then.target_model) {
            return Err(ValidationError::CrossProvider {
                src: src.clone(),
                target: then.target_model.clone(),
            });
        }
    }
    let _ = infer_provider; // referenced for parity with the cloud check / future messages
    Ok(())
}

/// When the route requires image or audio input, the target must be
/// `Vision`-capable (the runtime guard sets `vision=true` for both). An unknown
/// target (`lookup` returns `None`) is permissive, matching the runtime guard.
pub fn validate_capability(
    when: &RouteConditions,
    then: &RouteAction,
    lookup: impl Fn(&str) -> Option<ModelInfo>,
) -> Result<(), ValidationError> {
    let needs_vision = when.has_images == Some(true) || when.has_audio == Some(true);
    if !needs_vision {
        return Ok(());
    }
    if let Some(info) = lookup(&then.target_model) {
        if !info.capabilities.contains(&Capability::Vision) {
            return Err(ValidationError::MissingCapability {
                target: then.target_model.clone(),
                capability: "vision",
            });
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-routing validate` → PASS (4 tests).

- [ ] **Step 5: Re-export** — in `crates/routing/src/lib.rs`, add near the other `pub use`:

```rust
pub use store::NewRoute;
pub use validate::{validate_capability, validate_same_provider, ValidationError};
```

- [ ] **Step 6: Commit**

```bash
git add crates/routing/src/lib.rs crates/routing/src/validate.rs
git commit -m "feat(routing): shared typed validation (same-provider + modality→capability)"
```

---

## Task 4: `PostgresRoutingStore` management SQL

**Files:** Modify `crates/routing/src/store.rs` (the `#[cfg(feature = "postgres")] mod pg`)

> No live-DB unit test (consistent with the existing `list_for_org`, which has none); this task is **compile-verified** under the `postgres` feature and exercised in production. Behavior is covered by the InMemory contract tests (Task 1) and the gateway integration tests (Task 7, which use InMemory).

- [ ] **Step 1: Implement the management methods** — add to `impl RoutingStore for PostgresRoutingStore` (alongside `list_for_org`). A management `RouteRow` includes `enabled`:

```rust
    async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        let rows = sqlx::query_as::<_, MgmtRouteRow>(
            "SELECT id, name, priority, enabled, conditions, target \
             FROM routes WHERE org_id = $1 ORDER BY priority DESC, created_at ASC",
        )
        .bind(org_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
        Ok(rows.into_iter().filter_map(MgmtRouteRow::into_route).collect())
    }

    async fn create_route(
        &self,
        org_id: Uuid,
        spec: crate::store::NewRoute,
    ) -> Result<Route, RoutingStoreError> {
        let conditions = serde_json::to_value(&spec.when)
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
        let target = serde_json::to_value(&spec.then)
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
        let row = sqlx::query_as::<_, MgmtRouteRow>(
            "INSERT INTO routes (org_id, name, priority, conditions, target, enabled) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             RETURNING id, name, priority, enabled, conditions, target",
        )
        .bind(org_id)
        .bind(&spec.name)
        .bind(i32::try_from(spec.priority).unwrap_or(i32::MAX))
        .bind(&conditions)
        .bind(&target)
        .bind(spec.enabled)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
        row.into_route().ok_or_else(|| RoutingStoreError::Backend("created route failed to decode".into()))
    }

    async fn get_route(&self, org_id: Uuid, id: Uuid) -> Result<Option<Route>, RoutingStoreError> {
        let row = sqlx::query_as::<_, MgmtRouteRow>(
            "SELECT id, name, priority, enabled, conditions, target \
             FROM routes WHERE org_id = $1 AND id = $2",
        )
        .bind(org_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
        Ok(row.and_then(MgmtRouteRow::into_route))
    }

    async fn delete_route(&self, org_id: Uuid, id: Uuid) -> Result<bool, RoutingStoreError> {
        let res = sqlx::query("DELETE FROM routes WHERE org_id = $1 AND id = $2")
            .bind(org_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }
```

Add the management row type inside the `pg` module (next to `RouteRow`):

```rust
    #[derive(sqlx::FromRow)]
    struct MgmtRouteRow {
        id: Uuid,
        name: String,
        priority: i32,
        enabled: bool,
        conditions: sqlx::types::Json<serde_json::Value>,
        target: sqlx::types::Json<serde_json::Value>,
    }

    impl MgmtRouteRow {
        fn into_route(self) -> Option<Route> {
            let when = serde_json::from_value::<RouteConditions>(self.conditions.0).ok()?;
            let then = serde_json::from_value::<RouteAction>(self.target.0).ok()?;
            Some(Route {
                id: self.id,
                name: self.name,
                priority: u32::try_from(self.priority).unwrap_or(0),
                enabled: self.enabled,
                when,
                then,
            })
        }
    }
```

(The `pg` module already has `use crate::{RouteAction, RouteConditions};` and `use super::*;`; `NewRoute` is reachable as `crate::store::NewRoute`.)

- [ ] **Step 2: Compile-verify under the feature** — `cargo build -p tt-routing --features postgres`
Expected: SUCCESS (no errors). (`cargo test -p tt-routing` still passes — the default build excludes pg.)

- [ ] **Step 3: Commit**

```bash
git add crates/routing/src/store.rs
git commit -m "feat(routing): PostgresRoutingStore management SQL (create/get/delete/list-all)"
```

---

## Task 5: `ApiError::NotFound` + `ServiceUnavailable`

**Files:** Modify `crates/core/src/error.rs`

- [ ] **Step 1: Add the variants** — in the `ApiError` enum (after `Internal`):

```rust
    #[error("not found: {0}")]
    NotFound(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),
```

- [ ] **Step 2: Add the response arms** — in `impl IntoResponse for ApiError`'s `match`, after the `ApiError::Internal(m)` arm:

```rust
            ApiError::NotFound(m) => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "not_found",
                m.clone(),
            ),
            ApiError::ServiceUnavailable(m) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "service_unavailable",
                m.clone(),
            ),
```

- [ ] **Step 3: Verify it compiles** — `cargo build -p tt-core`
Expected: SUCCESS (no non-exhaustive-match error).

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/error.rs
git commit -m "feat(core): ApiError::NotFound (404) + ServiceUnavailable (503)"
```

---

## Task 6: Gateway `/v1/routes` handlers + wiring

**Files:** Create `crates/core/src/routes/routes_api.rs`; modify `crates/core/src/routes/mod.rs`, `crates/core/src/server.rs`

- [ ] **Step 1: Create the handler module** — `crates/core/src/routes/routes_api.rs`:

```rust
//! User-facing `/v1/routes` CRUD. Org is derived from the authenticated
//! `tt_live_` key (never caller-supplied). Requires a real key — anonymous /
//! dogfood / sandbox callers get 401.

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use tt_auth::ApiKeyContext;
use tt_routing::{validate_capability, validate_same_provider, NewRoute, Route};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::{AppState, DOGFOOD_ORG_ID};

/// Resolve the caller's real org, or 401. Dogfood/absent contexts are rejected.
fn require_org(ctx: Option<Extension<ApiKeyContext>>) -> Result<Uuid, ApiError> {
    match ctx {
        Some(Extension(c)) if c.org_id != DOGFOOD_ORG_ID => Ok(c.org_id),
        _ => Err(ApiError::Unauthorized),
    }
}

fn store(state: &AppState) -> ApiResult<&std::sync::Arc<tt_routing::CachingRoutingStore>> {
    state.routing_store.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable("route management is not configured on this gateway".into())
    })
}

/// `GET /v1/routes` — list all of the caller-org's routes (incl. disabled).
pub async fn list(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Json<Vec<Route>>> {
    let org = require_org(ctx)?;
    let routes = store(&state)?
        .list_all_for_org(org)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(routes))
}

/// `POST /v1/routes` — validate + create.
pub async fn create(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Json(spec): Json<NewRoute>,
) -> ApiResult<(axum::http::StatusCode, Json<Route>)> {
    let org = require_org(ctx)?;
    validate_same_provider(&spec.when, &spec.then).map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    let registry = state.registry.clone();
    validate_capability(&spec.when, &spec.then, |m| registry.model_info(m).cloned())
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    let created = store(&state)?
        .create_route(org, spec)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok((axum::http::StatusCode::CREATED, Json(created)))
}

/// `GET /v1/routes/:id`.
pub async fn get(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Route>> {
    let org = require_org(ctx)?;
    let route = store(&state)?
        .get_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("no route with id {id}")))?;
    Ok(Json(route))
}

/// `DELETE /v1/routes/:id`.
pub async fn delete(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let org = require_org(ctx)?;
    let removed = store(&state)?
        .delete_route(org, id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if removed {
        Ok(Json(serde_json::json!({ "ok": true, "id": id })))
    } else {
        Err(ApiError::NotFound(format!("no route with id {id}")))
    }
}
```

- [ ] **Step 2: Declare the module** — in `crates/core/src/routes/mod.rs`, add `pub mod routes_api;` (alongside the other `pub mod` route declarations).

- [ ] **Step 3: Mount the routes** — in `crates/core/src/server.rs`, in `build_router_with_retrieval`, add to the `base` router (after the `/v1/preview` route, before the `let base = match retrieval` line):

```rust
        .route(
            "/v1/routes",
            get(routes::routes_api::list).post(routes::routes_api::create),
        )
        .route(
            "/v1/routes/{id}",
            get(routes::routes_api::get).delete(routes::routes_api::delete),
        )
```

- [ ] **Step 4: Verify it compiles** — `cargo build -p tt-core`
Expected: SUCCESS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/routes_api.rs crates/core/src/routes/mod.rs crates/core/src/server.rs
git commit -m "feat(core): /v1/routes user CRUD (org-from-key, validation, cache invalidation)"
```

---

## Task 7: Gateway `/v1/routes` integration tests

**Files:** Create `crates/core/tests/routes_api.rs`

- [ ] **Step 1: Write the test file** — mirrors `route_rewrite.rs` (text-only `RecordingProvider`: `gpt-4o`, `gpt-4o-mini`). Full contents:

```rust
//! End-to-end tests for the user-facing /v1/routes CRUD.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tower::util::ServiceExt;

use tt_auth::{keys::{issue, Environment}, InMemoryKeyStore, KeyStore};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{CachingRoutingStore, InMemoryRoutingStore, RoutingStore};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

struct RecordingProvider { served: Arc<Mutex<Vec<String>>> }

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str { "recording" }
    fn models(&self) -> Vec<ModelInfo> {
        ["gpt-4o", "gpt-4o-mini"].into_iter().map(|id| ModelInfo {
            id: id.into(), provider: "recording".into(),
            capabilities: vec![Capability::Text], max_input_tokens: 4096, max_output_tokens: 4096,
        }).collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (i, o) = if model == "gpt-4o" { (5.0, 15.0) } else { (0.15, 0.6) };
        Some(ModelPricing { input_per_million: i, output_per_million: o,
            cached_input_per_million: None, cache_write_per_million: None, effective_at: Utc::now() })
    }
    async fn chat_completion(&self, req: ChatCompletionRequest, _c: &RequestContext)
        -> Result<ChatCompletionResponse, ProviderError> {
        self.served.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "x".into(), object: "chat.completion".into(), created: 0, model: req.model,
            choices: vec![Choice { index: 0, message: Message::Assistant {
                content: Some(MessageContent::Text("ok".into())), tool_calls: vec![], name: None },
                finish_reason: Some("stop".into()) }],
            usage: Usage { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10,
                cached_tokens: 0, cache_creation_input_tokens: None },
        })
    }
    async fn chat_completion_stream(&self, _r: ChatCompletionRequest, _c: &RequestContext)
        -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(&self, _r: EmbeddingsRequest, _c: &RequestContext)
        -> Result<EmbeddingsResponse, ProviderError> { Err(ProviderError::Unsupported("no".into())) }
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System).await.unwrap().plaintext
}

async fn app_with_key() -> (axum::Router, String, Arc<Mutex<Vec<String>>>) {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider { served: Arc::clone(&served) }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let routing = Arc::new(CachingRoutingStore::new(
        Arc::new(InMemoryRoutingStore::new()) as Arc<dyn RoutingStore>));
    let app = build_router(AppState::new(registry).with_key_store(key_store).with_routing_store(routing));
    (app, key, served)
}

fn req(method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    if let Some(k) = key { b = b.header("authorization", format!("Bearer {k}")); }
    b.body(body.map(|v| Body::from(v.to_string())).unwrap_or(Body::empty())).unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_list_get_delete_round_trip() {
    let (app, key, _served) = app_with_key().await;
    let spec = json!({ "name": "downgrade", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app.clone().oneshot(req("POST", "/v1/routes", Some(&key), Some(spec))).await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let created = body_json(r).await;
    let id = created["id"].as_str().unwrap().to_string();

    let r = app.clone().oneshot(req("GET", "/v1/routes", Some(&key), None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await.as_array().unwrap().len(), 1);

    let r = app.clone().oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let r = app.clone().oneshot(req("DELETE", &format!("/v1/routes/{id}"), Some(&key), None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app.oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unauthenticated_is_rejected() {
    let (app, _key, _) = app_with_key().await;
    let r = app.oneshot(req("GET", "/v1/routes", None, None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cross_provider_target_rejected() {
    let (app, key, _) = app_with_key().await;
    let spec = json!({ "name": "x", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"claude-haiku-4-5"} });
    let r = app.oneshot(req("POST", "/v1/routes", Some(&key), Some(spec))).await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn has_images_non_vision_target_rejected() {
    let (app, key, _) = app_with_key().await;
    // gpt-4o-mini in this test registry is Text-only → must reject.
    let spec = json!({ "name": "x", "when": {"has_images": true}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app.oneshot(req("POST", "/v1/routes", Some(&key), Some(spec))).await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn created_route_applies_immediately_without_ttl_wait() {
    let (app, key, served) = app_with_key().await;
    let spec = json!({ "name": "downgrade", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app.clone().oneshot(req("POST", "/v1/routes", Some(&key), Some(spec))).await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let chat = json!({ "model": "gpt-4o", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let r = app.oneshot(req("POST", "/v1/chat/completions", Some(&key), Some(chat))).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Cache was invalidated on create → the brand-new route applied on the very next request.
    assert_eq!(served.lock().unwrap().clone(), vec!["gpt-4o-mini".to_string()]);
}
```

- [ ] **Step 2: Run the tests** — `cargo test -p tt-core --test routes_api`
Expected: PASS (5 tests). If `created_route_applies_immediately_without_ttl_wait` fails (served `gpt-4o`), the create handler isn't invalidating — STOP and check Task 2 / that the handler writes through the `CachingRoutingStore`, not a bypassing path.

- [ ] **Step 3: Commit**

```bash
git add crates/core/tests/routes_api.rs
git commit -m "test(core): /v1/routes CRUD + auth + validation + immediate-apply e2e"
```

---

## Task 8: `tt route` CLI

**Files:** Create `crates/cli/src/route/mod.rs`; modify `crates/cli/src/lib.rs`, `crates/cli/src/main.rs`

- [ ] **Step 1: Declare the module + write the failing test** — in `crates/cli/src/lib.rs` add `pub mod route;` (alphabetical, after `pub mod retrieval;` or near it). Create `crates/cli/src/route/mod.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn always_pins_all_traffic() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o-mini".into()), from: None, to: None,
            when_has_images: false, when_has_audio: false,
            priority: 100, name: None, fallback: vec![], disabled: false,
        }).unwrap();
        assert_eq!(body["then"]["target_model"], "gpt-4o-mini");
        assert_eq!(body["when"], json!({}));
        assert_eq!(body["priority"], 100);
        assert_eq!(body["enabled"], true);
    }

    #[test]
    fn from_to_with_modality() {
        let body = build_new_route(&AddArgs {
            always: None, from: Some("gpt-4o".into()), to: Some("gpt-4o-mini".into()),
            when_has_images: true, when_has_audio: false,
            priority: 50, name: Some("vis".into()), fallback: vec!["gpt-4o".into()], disabled: true,
        }).unwrap();
        assert_eq!(body["when"]["model_in"], json!(["gpt-4o"]));
        assert_eq!(body["when"]["has_images"], true);
        assert_eq!(body["then"]["target_model"], "gpt-4o-mini");
        assert_eq!(body["then"]["fallbacks"], json!(["gpt-4o"]));
        assert_eq!(body["name"], "vis");
        assert_eq!(body["enabled"], false);
    }

    #[test]
    fn requires_a_target() {
        let err = build_new_route(&AddArgs {
            always: None, from: Some("gpt-4o".into()), to: None,
            when_has_images: false, when_has_audio: false,
            priority: 100, name: None, fallback: vec![], disabled: false,
        });
        assert!(err.is_err());
    }
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-cli route` → FAIL (`build_new_route` / `AddArgs` undefined).

- [ ] **Step 3: Implement** — add above the test module in `crates/cli/src/route/mod.rs`:

```rust
//! `tt route` — manage routing rules via the gateway's user-facing /v1/routes
//! API, authenticated with the V0-resolved key.

use anyhow::Context as _;
use serde_json::{json, Value};

use crate::context::ResolvedContext;

/// Flags for `tt route add`. Mirrors the clap args in `main.rs`.
pub struct AddArgs {
    pub always: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub when_has_images: bool,
    pub when_has_audio: bool,
    pub priority: u32,
    pub name: Option<String>,
    pub fallback: Vec<String>,
    pub disabled: bool,
}

/// Pure: map `add` flags to the `NewRoute` JSON body the API expects.
pub fn build_new_route(args: &AddArgs) -> anyhow::Result<Value> {
    let target = match (&args.always, &args.to) {
        (Some(m), None) => m.clone(),
        (None, Some(m)) => m.clone(),
        (Some(_), Some(_)) => anyhow::bail!("use either --always or --to, not both"),
        (None, None) => anyhow::bail!("a target is required: pass --always <model> or --to <model>"),
    };
    let mut when = serde_json::Map::new();
    // --always means match-all: no model_in. --from sets model_in.
    if args.always.is_none() {
        if let Some(from) = &args.from {
            when.insert("model_in".into(), json!([from]));
        }
    }
    if args.when_has_images {
        when.insert("has_images".into(), json!(true));
    }
    if args.when_has_audio {
        when.insert("has_audio".into(), json!(true));
    }
    let mut then = serde_json::Map::new();
    then.insert("target_model".into(), json!(target));
    if !args.fallback.is_empty() {
        then.insert("fallbacks".into(), json!(args.fallback));
    }
    Ok(json!({
        "name": args.name.clone().unwrap_or_else(|| default_name(args, &target)),
        "priority": args.priority,
        "enabled": !args.disabled,
        "when": Value::Object(when),
        "then": Value::Object(then),
    }))
}

fn default_name(args: &AddArgs, target: &str) -> String {
    match &args.from {
        Some(f) => format!("{f}->{target}"),
        None => format!("all->{target}"),
    }
}

/// What `tt route` was asked to do.
pub enum RouteCmd {
    List,
    Show(String),
    Rm(String),
    Add(AddArgs),
}

/// Dispatch a `tt route` subcommand against the gateway.
pub async fn run(cmd: RouteCmd, flag_key: Option<String>, flag_base: Option<String>) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login --token <KEY>` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();

    match cmd {
        RouteCmd::List => {
            let routes: Value = send(http.get(format!("{base}/v1/routes")).bearer_auth(&key)).await?;
            print_routes(&routes);
        }
        RouteCmd::Show(id) => {
            let route: Value = send(http.get(format!("{base}/v1/routes/{id}")).bearer_auth(&key)).await?;
            println!("{}", serde_json::to_string_pretty(&route)?);
        }
        RouteCmd::Rm(id) => {
            let _: Value = send(http.delete(format!("{base}/v1/routes/{id}")).bearer_auth(&key)).await?;
            println!("Removed route {id}.");
        }
        RouteCmd::Add(args) => {
            let body = build_new_route(&args)?;
            let route: Value = send(http.post(format!("{base}/v1/routes")).bearer_auth(&key).json(&body)).await?;
            println!("Created route {} ({}).", route["id"].as_str().unwrap_or("?"), route["name"].as_str().unwrap_or("?"));
        }
    }
    Ok(())
}

/// Send a request; map non-2xx to an error carrying the response body.
async fn send(req: reqwest::RequestBuilder) -> anyhow::Result<Value> {
    let resp = req.send().await.context("request to gateway failed")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("gateway returned {status}: {text}");
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("decode gateway response")
}

fn print_routes(routes: &Value) {
    let Some(arr) = routes.as_array() else {
        println!("(no routes)");
        return;
    };
    if arr.is_empty() {
        println!("No routes. Create one with `tt route add --from <model> --to <model>`.");
        return;
    }
    println!("{:<38}  {:<22}  {:>4}  {:<8}  {}", "ID", "NAME", "PRIO", "ENABLED", "TARGET");
    for r in arr {
        println!(
            "{:<38}  {:<22}  {:>4}  {:<8}  {}",
            r["id"].as_str().unwrap_or("?"),
            r["name"].as_str().unwrap_or("?"),
            r["priority"].as_u64().unwrap_or(0),
            r["enabled"].as_bool().unwrap_or(false),
            r["then"]["target_model"].as_str().unwrap_or("?"),
        );
    }
}
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-cli route` → PASS (3 tests).

- [ ] **Step 5: Wire the command into `main.rs`** — add a `Route` variant to `enum Command` (after `Proxy`, before the closing `}`):

```rust
    /// Manage routing rules via the hosted gateway (requires `tt login`).
    Route {
        #[command(subcommand)]
        action: RouteAction,
        /// Override the API key (else V0 resolution: env / ~/.tokentrimmer).
        #[arg(long, global = true)]
        tt_api_key: Option<String>,
        /// Override the gateway base URL.
        #[arg(long, global = true)]
        tt_api_base: Option<String>,
    },
```

Add a `RouteAction` subcommand enum (next to `AuditAction`):

```rust
#[derive(Subcommand)]
enum RouteAction {
    /// List your routes.
    List,
    /// Show one route by id.
    Show { id: String },
    /// Delete one route by id.
    Rm { id: String },
    /// Add a route. Use --always <model>, or --from <m> --to <m>.
    Add {
        #[arg(long)]
        always: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        when_has_images: bool,
        #[arg(long)]
        when_has_audio: bool,
        #[arg(long, default_value_t = 100)]
        priority: u32,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        fallback: Vec<String>,
        #[arg(long)]
        disabled: bool,
    },
}
```

Add the dispatch arm in the `match cli.command` (before `Command::Proxy`):

```rust
        Command::Route { action, tt_api_key, tt_api_base } => {
            use tt_cli::route::{AddArgs, RouteCmd};
            let cmd = match action {
                RouteAction::List => RouteCmd::List,
                RouteAction::Show { id } => RouteCmd::Show(id),
                RouteAction::Rm { id } => RouteCmd::Rm(id),
                RouteAction::Add {
                    always, from, to, when_has_images, when_has_audio,
                    priority, name, fallback, disabled,
                } => RouteCmd::Add(AddArgs {
                    always, from, to, when_has_images, when_has_audio,
                    priority, name, fallback, disabled,
                }),
            };
            tt_cli::route::run(cmd, tt_api_key, tt_api_base).await?;
        }
```

- [ ] **Step 6: Verify build + smoke the arg parsing** — `cargo build -p tt-cli`; then:

```bash
./target/debug/tt route --help
./target/debug/tt route add --help
```
Expected: both print usage with the documented subcommands/flags (no network).

- [ ] **Step 7: Commit**

```bash
git add crates/cli/src/route/mod.rs crates/cli/src/lib.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt route list/show/add/rm against the user-facing /v1/routes API"
```

---

## Task 9: Final verification

**Files:** none.

- [ ] **Step 1: Format** — `cargo fmt -p tt-routing -p tt-core -p tt-cli`; then `git diff --quiet || git commit -am "style: cargo fmt (v3a-2)"`.
- [ ] **Step 2: Clippy** — `cargo clippy -p tt-routing -p tt-core -p tt-cli --all-targets -- -D warnings`; then `cargo clippy -p tt-routing --features postgres -- -D warnings`. Expected: clean.
- [ ] **Step 3: Tests** — `cargo test -p tt-routing -p tt-cli` then `cargo test -p tt-core --test routes_api --test route_rewrite --test route_content_type`. Expected: all pass (no regression in the V3a-1 routing tests).
- [ ] **Step 4: Clean tree** — `git status` (clean) + `git log --oneline -10` (Task 1–8 commits present on `feat/v3a-2-routes-api`).

---

## Self-Review (completed by plan author)

**1. Spec coverage:** gateway `/v1/routes` (org-from-key, 401 dogfood/anon, 503 unconfigured) → Tasks 5–7; `RoutingStore` management + InMemory/Postgres → Tasks 1, 4; cache invalidation on write → Task 2 (+ proven in Task 7's immediate-apply test); shared validation (same-provider + modality→Vision) → Task 3; `tt route list/show/add/rm` + simple-rule flags → Task 8. No PATCH (spec non-goal). Dashboard = V3a-3 (out of scope).

**2. Placeholder scan:** every code/test step is complete; commands have expected output; the Postgres no-live-DB-test decision is stated with its rationale (matches the existing `list_for_org`).

**3. Type consistency:** `NewRoute { name, priority: u32, enabled, when, then }` defined once (Task 1), used by the trait (1), Caching (2), Postgres (4), the handler `Json<NewRoute>` (6), and the CLI JSON shape (8). `validate_same_provider`/`validate_capability`/`ValidationError` defined Task 3, consumed Task 6. `ApiError::{NotFound, ServiceUnavailable}` defined Task 5, used Task 6. Handlers read `Option<Extension<ApiKeyContext>>` and `crate::DOGFOOD_ORG_ID` (verified anchors). `ProviderRegistry::model_info(id).cloned()` feeds `validate_capability`'s `lookup`. CLI uses `ResolvedContext` (V0) + `reqwest …bearer_auth` (verified pattern).

**Known follow-on (V3a-3, cloud):** dashboard exposure of `has_images`/`has_audio`; optional cloud-admin adoption of the shared `tt_routing::validate` functions.
