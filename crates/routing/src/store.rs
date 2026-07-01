//! Where routes come from at runtime.
//!
//! The gateway only knows about a [`RoutingStore`] trait. Production wires
//! [`PostgresRoutingStore`] (behind the `postgres` feature flag) reading the
//! `routes` table that the cloud dashboard writes; tests use
//! [`InMemoryRoutingStore`]. A separate [`crate::cache::CachingRoutingStore`]
//! wraps either with a per-org TTL cache so the hot path isn't a DB round-trip.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use uuid::Uuid;

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

/// Who initiated a route pause. `Auto` = the gateway's quality-regression
/// evaluator ([`tt-core`]'s `AutoPauseJudgeSink`); `Manual` = an operator via
/// `POST /v1/routes/:id/pause`. Lowercase on the wire, matching the
/// `route_pauses.paused_by` CHECK constraint (migration 0019).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PausedBy {
    Auto,
    Manual,
}

impl PausedBy {
    /// SQL TEXT form, matching the `route_pauses.paused_by` CHECK constraint.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

/// Fields recorded when sticky-pausing a route ([`RoutingStore::pause_route`]).
/// `pass_rate` / `verdicts_in_window` carry the windowed evidence behind an
/// auto pause (`None` for a manual pause).
#[derive(Debug, Clone)]
pub struct NewRoutePause {
    pub paused_by: PausedBy,
    pub reason: String,
    pub pass_rate: Option<f64>,
    pub verdicts_in_window: Option<i32>,
}

/// Source of truth for an org's enabled routes.
///
/// Implementations return ALL enabled routes for `org_id`; ordering is the
/// caller's problem ([`crate::RoutingEngine::with_routes`] sorts internally).
#[async_trait]
pub trait RoutingStore: Send + Sync + std::fmt::Debug {
    /// Fetch the enabled-and-current route list for `org_id`. Returns
    /// `Ok(vec![])` when the org has no routes — not an error.
    async fn list_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError>;

    /// Management: ALL of an org's routes, including disabled ones.
    async fn list_all_for_org(&self, _org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "management unsupported by this store".into(),
        ))
    }
    /// Management: create a route for `org_id`. Returns the created route.
    async fn create_route(
        &self,
        _org_id: Uuid,
        _spec: NewRoute,
    ) -> Result<Route, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "management unsupported by this store".into(),
        ))
    }
    /// Management: fetch one route owned by `org_id`.
    async fn get_route(
        &self,
        _org_id: Uuid,
        _id: Uuid,
    ) -> Result<Option<Route>, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "management unsupported by this store".into(),
        ))
    }
    /// Management: delete one route owned by `org_id`. Returns whether a row was removed.
    async fn delete_route(&self, _org_id: Uuid, _id: Uuid) -> Result<bool, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "management unsupported by this store".into(),
        ))
    }
    /// Sticky-pause a route's rewrite. `Ok(true)` = newly paused; `Ok(false)` =
    /// already actively paused OR the route is not owned by `org_id` (the
    /// Postgres impl's `INSERT .. SELECT` guards ownership). An active pause's
    /// evidence is never overwritten (sticky); a route paused again AFTER a
    /// resume records the NEW pause's evidence. A paused route still matches
    /// but every cost lever is suppressed — requests flow to the originally-
    /// requested model until [`RoutingStore::resume_route`].
    async fn pause_route(
        &self,
        _org_id: Uuid,
        _route_id: Uuid,
        _pause: NewRoutePause,
    ) -> Result<bool, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "pause unsupported by this store".into(),
        ))
    }
    /// Clear a route's sticky pause. `Ok(true)` = an active pause was cleared;
    /// `Ok(false)` = the route was not paused or not owned by `org_id`. This is
    /// the ONLY thing that clears a pause. The pause record is RETAINED with
    /// `resumed_at` stamped — that timestamp is the watermark the auto-pause
    /// verdict window is bounded by, so a resumed route is re-evaluated only
    /// on verdicts recorded AFTER the resume (the frozen pre-pause window can
    /// never instantly re-pause it).
    async fn resume_route(
        &self,
        _org_id: Uuid,
        _route_id: Uuid,
    ) -> Result<bool, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "pause unsupported by this store".into(),
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RoutingStoreError {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Test / dev backend. Holds a HashMap<org_id, Vec<Route>>; the gateway treats
/// it like any other store. Pauses live in a separate map (mirroring the
/// production `route_pauses` table) and are OVERLAID onto every returned
/// `Route.paused` — a route planted directly with `paused: true` via
/// [`InMemoryRoutingStore::set_routes`] also reads as paused (handy for
/// plant-and-assert tests), but only `pause_route`-created pauses are
/// clearable via `resume_route`.
#[derive(Debug, Default)]
pub struct InMemoryRoutingStore {
    inner: RwLock<HashMap<Uuid, Vec<Route>>>,
    /// `route_id → pause record` (mirror of the `route_pauses` table).
    /// `resumed == true` mirrors a row with `resumed_at` stamped: no longer an
    /// active pause, retained as the resume watermark / historical record.
    pauses: RwLock<HashMap<Uuid, PauseEntry>>,
}

/// In-memory mirror of one `route_pauses` row.
#[derive(Debug)]
struct PauseEntry {
    pause: NewRoutePause,
    /// Mirror of `resumed_at IS NOT NULL` — the pause was cleared by an
    /// explicit resume (record retained).
    resumed: bool,
}

impl InMemoryRoutingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the routes for an org. Useful for plant-and-assert tests.
    pub fn set_routes(&self, org_id: Uuid, routes: Vec<Route>) {
        let mut g = self.inner.write().expect("inmemory routing store poisoned");
        g.insert(org_id, routes);
    }

    /// Overlay the pause map onto a route list (keeps a directly-planted
    /// `paused: true` as well). Only ACTIVE pauses count — a resumed entry is
    /// a retained watermark, not a pause.
    fn overlay_paused(&self, mut routes: Vec<Route>) -> Vec<Route> {
        let p = self.pauses.read().expect("inmemory pause map poisoned");
        for r in &mut routes {
            r.paused = r.paused || p.get(&r.id).is_some_and(|e| !e.resumed);
        }
        routes
    }

    fn route_exists(&self, org_id: Uuid, id: Uuid) -> bool {
        let g = self.inner.read().expect("inmemory routing store poisoned");
        g.get(&org_id).is_some_and(|v| v.iter().any(|r| r.id == id))
    }

    /// The stored pause record for `route_id`, with whether it has been
    /// resumed (`true` mirrors `resumed_at IS NOT NULL` — a retained
    /// watermark, not an active pause). Test/diagnostic accessor; `None` when
    /// the route was never paused (or its record was GC'd by delete).
    #[must_use]
    pub fn pause_record(&self, route_id: Uuid) -> Option<(NewRoutePause, bool)> {
        let p = self.pauses.read().expect("inmemory pause map poisoned");
        p.get(&route_id).map(|e| (e.pause.clone(), e.resumed))
    }
}

#[async_trait]
impl RoutingStore for InMemoryRoutingStore {
    async fn list_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        let routes = {
            let g = self.inner.read().expect("inmemory routing store poisoned");
            g.get(&org_id).cloned().unwrap_or_default()
        };
        Ok(self.overlay_paused(routes))
    }

    async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        self.list_for_org(org_id).await
    }

    async fn create_route(&self, org_id: Uuid, spec: NewRoute) -> Result<Route, RoutingStoreError> {
        let route = Route {
            id: Uuid::now_v7(),
            name: spec.name,
            priority: spec.priority,
            enabled: spec.enabled,
            when: spec.when,
            then: spec.then,
            paused: false,
        };
        let mut g = self.inner.write().expect("inmemory routing store poisoned");
        g.entry(org_id).or_default().push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, org_id: Uuid, id: Uuid) -> Result<Option<Route>, RoutingStoreError> {
        let route = {
            let g = self.inner.read().expect("inmemory routing store poisoned");
            g.get(&org_id)
                .and_then(|v| v.iter().find(|r| r.id == id).cloned())
        };
        Ok(route
            .map(|r| self.overlay_paused(vec![r]))
            .and_then(|mut v| v.pop()))
    }

    async fn delete_route(&self, org_id: Uuid, id: Uuid) -> Result<bool, RoutingStoreError> {
        let removed = {
            let mut g = self.inner.write().expect("inmemory routing store poisoned");
            let Some(v) = g.get_mut(&org_id) else {
                return Ok(false);
            };
            let before = v.len();
            v.retain(|r| r.id != id);
            v.len() != before
        };
        if removed {
            // GC the pause record with its route (mirrors the Postgres impl);
            // a recreated route gets a fresh id, so a sticky pause does NOT
            // survive the documented delete-and-recreate edit flow.
            let mut p = self.pauses.write().expect("inmemory pause map poisoned");
            p.remove(&id);
        }
        Ok(removed)
    }

    async fn pause_route(
        &self,
        org_id: Uuid,
        route_id: Uuid,
        pause: NewRoutePause,
    ) -> Result<bool, RoutingStoreError> {
        // Ownership guard: a foreign org's pause is a no-op (Ok(false)),
        // mirroring the Postgres INSERT..SELECT.
        if !self.route_exists(org_id, route_id) {
            return Ok(false);
        }
        let mut p = self.pauses.write().expect("inmemory pause map poisoned");
        if p.get(&route_id).is_some_and(|e| !e.resumed) {
            return Ok(false); // sticky: an ACTIVE pause keeps its evidence
        }
        // New pause, or re-pause after a resume (the resumed watermark entry
        // is replaced by the new pause's evidence — mirrors the Postgres
        // ON CONFLICT DO UPDATE .. WHERE resumed_at IS NOT NULL).
        p.insert(
            route_id,
            PauseEntry {
                pause,
                resumed: false,
            },
        );
        Ok(true)
    }

    async fn resume_route(&self, org_id: Uuid, route_id: Uuid) -> Result<bool, RoutingStoreError> {
        if !self.route_exists(org_id, route_id) {
            return Ok(false);
        }
        let mut p = self.pauses.write().expect("inmemory pause map poisoned");
        match p.get_mut(&route_id) {
            // Active pause → mark resumed (record retained as the watermark).
            Some(e) if !e.resumed => {
                e.resumed = true;
                Ok(true)
            }
            // Not paused (or already resumed) → Ok(false).
            _ => Ok(false),
        }
    }
}

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use crate::{RouteAction, RouteConditions};
    use sqlx::PgPool;

    /// Reads the `routes` table written by the cloud dashboard / tt-api admin
    /// endpoints. Schema lives in tokentrimmer-cloud
    /// (`crates/api/migrations/0002_routes.up.sql`):
    ///
    /// ```sql
    /// CREATE TABLE routes (
    ///   id          UUID PRIMARY KEY,
    ///   org_id      UUID NOT NULL,
    ///   name        TEXT NOT NULL,
    ///   priority    INT  NOT NULL,
    ///   conditions  JSONB NOT NULL,
    ///   target      JSONB NOT NULL,
    ///   enabled     BOOLEAN NOT NULL,
    ///   ...
    /// );
    /// ```
    ///
    /// Rows whose `conditions` or `target` JSON fails to decode are skipped
    /// with a warning — a single malformed row must not knock out routing for
    /// the org. Wrap in [`crate::cache::CachingRoutingStore`] to amortize the
    /// SELECT across hot-path requests.
    ///
    /// Pause state lives in the PUBLIC-owned `route_pauses` table (migration
    /// 0017 — the cloud-owned `routes` table cannot be ALTERed from public
    /// migrations) and is surfaced on `Route.paused` via a LEFT JOIN (active
    /// pauses only: `resumed_at IS NULL`) in every SELECT, so a sticky pause
    /// survives dashboard edits to the route row itself. A resume RETAINS the
    /// row with `resumed_at` stamped — the watermark the auto-pause verdict
    /// window is bounded by; deleting the route deletes the record.
    #[derive(Clone, Debug)]
    pub struct PostgresRoutingStore {
        pool: PgPool,
    }

    impl PostgresRoutingStore {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl RoutingStore for PostgresRoutingStore {
        async fn list_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
            let rows = sqlx::query_as::<_, RouteRow>(
                "SELECT r.id, r.name, r.priority, r.conditions, r.target, \
                        (p.route_id IS NOT NULL) AS paused \
                 FROM routes r LEFT JOIN route_pauses p ON p.route_id = r.id AND p.resumed_at IS NULL \
                 WHERE r.org_id = $1 AND r.enabled = TRUE \
                 ORDER BY r.priority DESC, r.created_at ASC",
            )
            .bind(org_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;

            Ok(rows.into_iter().filter_map(RouteRow::into_route).collect())
        }

        async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
            let rows = sqlx::query_as::<_, MgmtRouteRow>(
                "SELECT r.id, r.name, r.priority, r.enabled, r.conditions, r.target, \
                        (p.route_id IS NOT NULL) AS paused \
                 FROM routes r LEFT JOIN route_pauses p ON p.route_id = r.id AND p.resumed_at IS NULL \
                 WHERE r.org_id = $1 ORDER BY r.priority DESC, r.created_at ASC",
            )
            .bind(org_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            Ok(rows
                .into_iter()
                .filter_map(MgmtRouteRow::into_route)
                .collect())
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
                // A freshly-created route can have no pause row (new id) —
                // FALSE AS paused keeps the RETURNING shape join-free.
                "INSERT INTO routes (org_id, name, priority, conditions, target, enabled) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 RETURNING id, name, priority, enabled, conditions, target, FALSE AS paused",
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
            row.into_route()
                .ok_or_else(|| RoutingStoreError::Backend("created route failed to decode".into()))
        }

        async fn get_route(
            &self,
            org_id: Uuid,
            id: Uuid,
        ) -> Result<Option<Route>, RoutingStoreError> {
            let row = sqlx::query_as::<_, MgmtRouteRow>(
                "SELECT r.id, r.name, r.priority, r.enabled, r.conditions, r.target, \
                        (p.route_id IS NOT NULL) AS paused \
                 FROM routes r LEFT JOIN route_pauses p ON p.route_id = r.id AND p.resumed_at IS NULL \
                 WHERE r.org_id = $1 AND r.id = $2",
            )
            .bind(org_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            Ok(row.and_then(MgmtRouteRow::into_route))
        }

        async fn delete_route(&self, org_id: Uuid, id: Uuid) -> Result<bool, RoutingStoreError> {
            // The pause record (active OR resumed watermark) is GC'd with its
            // route — `route_pauses` deliberately has no FK to the cloud-owned
            // `routes` table, so the cleanup is explicit here. A recreated
            // route gets a fresh id, so a sticky pause does NOT survive the
            // documented delete-and-recreate edit flow. The data-modifying CTE
            // runs atomically with the route delete; `rows_affected` reports
            // the ROUTE delete only (the statement's top-level DELETE).
            let res = sqlx::query(
                "WITH gc AS (DELETE FROM route_pauses WHERE org_id = $1 AND route_id = $2) \
                 DELETE FROM routes WHERE org_id = $1 AND id = $2",
            )
            .bind(org_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            Ok(res.rows_affected() > 0)
        }

        async fn pause_route(
            &self,
            org_id: Uuid,
            route_id: Uuid,
            pause: crate::store::NewRoutePause,
        ) -> Result<bool, RoutingStoreError> {
            // INSERT..SELECT guards ownership (a foreign org's pause inserts
            // nothing). An ACTIVE pause (resumed_at IS NULL) is sticky — the
            // conditional ON CONFLICT update touches nothing and reports
            // Ok(false), keeping the first pause's evidence. A RESUMED
            // watermark row is overwritten by the new pause's evidence
            // (re-pause after resume), resetting resumed_at to NULL.
            let res = sqlx::query(
                "INSERT INTO route_pauses \
                   (route_id, org_id, paused_by, reason, pass_rate, verdicts_in_window) \
                 SELECT r.id, r.org_id, $3, $4, $5, $6 \
                 FROM routes r WHERE r.org_id = $1 AND r.id = $2 \
                 ON CONFLICT (route_id) DO UPDATE SET \
                   paused_at = now(), paused_by = EXCLUDED.paused_by, \
                   reason = EXCLUDED.reason, pass_rate = EXCLUDED.pass_rate, \
                   verdicts_in_window = EXCLUDED.verdicts_in_window, resumed_at = NULL \
                 WHERE route_pauses.resumed_at IS NOT NULL",
            )
            .bind(org_id)
            .bind(route_id)
            .bind(pause.paused_by.as_str())
            .bind(&pause.reason)
            .bind(pause.pass_rate)
            .bind(pause.verdicts_in_window)
            .execute(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            Ok(res.rows_affected() > 0)
        }

        async fn resume_route(
            &self,
            org_id: Uuid,
            route_id: Uuid,
        ) -> Result<bool, RoutingStoreError> {
            // The record is RETAINED with resumed_at stamped: that timestamp
            // is the watermark `RECENT_CLASSIFIED_SQL` (tt-core) bounds the
            // auto-pause verdict window by, so a resumed route is re-evaluated
            // only on post-resume verdicts. Resuming an already-resumed (or
            // never-paused) route touches nothing → Ok(false).
            let res = sqlx::query(
                "UPDATE route_pauses SET resumed_at = now() \
                 WHERE org_id = $1 AND route_id = $2 AND resumed_at IS NULL",
            )
            .bind(org_id)
            .bind(route_id)
            .execute(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            Ok(res.rows_affected() > 0)
        }
    }

    #[derive(sqlx::FromRow)]
    struct RouteRow {
        id: Uuid,
        name: String,
        priority: i32,
        conditions: sqlx::types::Json<serde_json::Value>,
        target: sqlx::types::Json<serde_json::Value>,
        /// `route_pauses` LEFT JOIN: `(p.route_id IS NOT NULL)`.
        paused: bool,
    }

    impl RouteRow {
        fn into_route(self) -> Option<Route> {
            let when = match serde_json::from_value::<RouteConditions>(self.conditions.0) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(route_id = %self.id, error = %e, "skipping route — conditions JSON failed to decode");
                    return None;
                }
            };
            let then = match serde_json::from_value::<RouteAction>(self.target.0) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(route_id = %self.id, error = %e, "skipping route — target JSON failed to decode");
                    return None;
                }
            };
            Some(Route {
                id: self.id,
                name: self.name,
                priority: u32::try_from(self.priority).unwrap_or(0),
                enabled: true,
                when,
                then,
                paused: self.paused,
            })
        }
    }

    /// Like [`RouteRow`] but carries `enabled` (management lists disabled routes too).
    #[derive(sqlx::FromRow)]
    struct MgmtRouteRow {
        id: Uuid,
        name: String,
        priority: i32,
        enabled: bool,
        conditions: sqlx::types::Json<serde_json::Value>,
        target: sqlx::types::Json<serde_json::Value>,
        /// `route_pauses` LEFT JOIN: `(p.route_id IS NOT NULL)`.
        paused: bool,
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
                paused: self.paused,
            })
        }
    }
}

#[cfg(feature = "postgres")]
pub use pg::PostgresRoutingStore;

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::Route;
    use crate::{RouteAction, RouteConditions};

    fn route(name: &str, priority: u32, target: &str) -> Route {
        Route {
            id: Uuid::now_v7(),
            name: name.into(),
            priority,
            enabled: true,
            when: RouteConditions::default(),
            then: RouteAction {
                format_switch: None,
                diff: false,
                target_model: Some(target.into()),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                doc_compaction: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                agentic_budget: None,
                panel: None,
            },
            paused: false,
        }
    }

    #[tokio::test]
    async fn in_memory_returns_empty_for_unknown_org() {
        let s = InMemoryRoutingStore::new();
        let rs = s.list_for_org(Uuid::now_v7()).await.unwrap();
        assert!(rs.is_empty());
    }

    #[tokio::test]
    async fn in_memory_set_and_fetch_round_trips() {
        let s = InMemoryRoutingStore::new();
        let org = Uuid::now_v7();
        s.set_routes(org, vec![route("a", 10, "m1"), route("b", 5, "m2")]);
        let rs = s.list_for_org(org).await.unwrap();
        assert_eq!(rs.len(), 2);
    }

    #[tokio::test]
    async fn in_memory_create_list_get_delete() {
        let s = InMemoryRoutingStore::new();
        let org = Uuid::now_v7();
        let spec = NewRoute {
            name: "pin".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions::default(),
            then: RouteAction {
                format_switch: None,
                diff: false,
                target_model: Some("m1".into()),
                fallbacks: vec![],
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                doc_compaction: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                agentic_budget: None,
                panel: None,
            },
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

    fn pause(by: PausedBy) -> NewRoutePause {
        NewRoutePause {
            paused_by: by,
            reason: "test".into(),
            pass_rate: Some(0.5),
            verdicts_in_window: Some(20),
        }
    }

    /// Sticky pause/resume round trip: `pause_route` marks `Route.paused` in
    /// every accessor, a second pause is `Ok(false)` (sticky/idempotent),
    /// `resume_route` clears, and resuming an unpaused route is `Ok(false)`.
    #[tokio::test]
    async fn in_memory_pause_resume_round_trip() {
        let s = InMemoryRoutingStore::new();
        let org = Uuid::now_v7();
        let created = s
            .create_route(
                org,
                NewRoute {
                    name: "down".into(),
                    priority: 10,
                    enabled: true,
                    when: RouteConditions::default(),
                    then: RouteAction {
                        target_model: Some("m1".into()),
                        ..Default::default()
                    },
                },
            )
            .await
            .unwrap();
        assert!(!created.paused, "a fresh route is never paused");
        assert!(
            !s.get_route(org, created.id).await.unwrap().unwrap().paused,
            "unpaused before pause_route"
        );

        assert!(
            s.pause_route(org, created.id, pause(PausedBy::Auto))
                .await
                .unwrap(),
            "first pause must report newly-paused"
        );
        assert!(s.get_route(org, created.id).await.unwrap().unwrap().paused);
        assert!(s.list_for_org(org).await.unwrap()[0].paused);
        assert!(s.list_all_for_org(org).await.unwrap()[0].paused);

        // Sticky/idempotent: a second pause changes nothing.
        assert!(
            !s.pause_route(org, created.id, pause(PausedBy::Manual))
                .await
                .unwrap(),
            "already-paused must be Ok(false)"
        );

        assert!(
            s.resume_route(org, created.id).await.unwrap(),
            "resume clears the pause"
        );
        assert!(!s.get_route(org, created.id).await.unwrap().unwrap().paused);
        assert!(!s.list_for_org(org).await.unwrap()[0].paused);
        assert!(
            !s.resume_route(org, created.id).await.unwrap(),
            "resume of an unpaused route is Ok(false)"
        );
        // The record is RETAINED as a resumed watermark (mirror of
        // `resumed_at IS NOT NULL`), not deleted.
        let (kept, resumed) = s.pause_record(created.id).expect("record retained");
        assert!(resumed, "record must be marked resumed, not removed");
        assert_eq!(kept.paused_by, PausedBy::Auto, "first pause's evidence");

        // Re-pause AFTER a resume: allowed, and the NEW pause's evidence
        // replaces the resumed watermark (resumed flag cleared).
        assert!(
            s.pause_route(org, created.id, pause(PausedBy::Manual))
                .await
                .unwrap(),
            "re-pause after resume must succeed"
        );
        assert!(s.get_route(org, created.id).await.unwrap().unwrap().paused);
        let (second, resumed) = s.pause_record(created.id).expect("record");
        assert!(!resumed);
        assert_eq!(
            second.paused_by,
            PausedBy::Manual,
            "re-pause records the NEW evidence"
        );

        // Deleting the route GCs its pause record — a recreated route (fresh
        // id) starts clean; no orphaned entries accumulate.
        assert!(s.delete_route(org, created.id).await.unwrap());
        assert!(
            s.pause_record(created.id).is_none(),
            "delete_route must GC the pause record"
        );
    }

    /// Org B cannot pause, resume, or observe org A's route pause.
    #[tokio::test]
    async fn pause_resume_is_org_scoped() {
        let s = InMemoryRoutingStore::new();
        let org_a = Uuid::now_v7();
        let org_b = Uuid::now_v7();
        let created = s
            .create_route(
                org_a,
                NewRoute {
                    name: "a".into(),
                    priority: 1,
                    enabled: true,
                    when: RouteConditions::default(),
                    then: RouteAction {
                        target_model: Some("m".into()),
                        ..Default::default()
                    },
                },
            )
            .await
            .unwrap();
        // Foreign-org pause is a no-op.
        assert!(!s
            .pause_route(org_b, created.id, pause(PausedBy::Manual))
            .await
            .unwrap());
        assert!(
            !s.get_route(org_a, created.id)
                .await
                .unwrap()
                .unwrap()
                .paused
        );
        // Pause as the owner; the foreign org cannot resume it.
        assert!(s
            .pause_route(org_a, created.id, pause(PausedBy::Auto))
            .await
            .unwrap());
        assert!(!s.resume_route(org_b, created.id).await.unwrap());
        assert!(
            s.get_route(org_a, created.id)
                .await
                .unwrap()
                .unwrap()
                .paused
        );
    }

    #[tokio::test]
    async fn in_memory_management_is_org_scoped() {
        let s = InMemoryRoutingStore::new();
        let org_a = Uuid::now_v7();
        let org_b = Uuid::now_v7();
        let created = s
            .create_route(
                org_a,
                NewRoute {
                    name: "a".into(),
                    priority: 1,
                    enabled: true,
                    when: RouteConditions::default(),
                    then: RouteAction {
                        format_switch: None,
                        diff: false,
                        target_model: Some("m".into()),
                        fallbacks: vec![],
                        disable_cache: false,
                        max_cost_usd: None,
                        flex: false,
                        batch: false,
                        compress: false,
                        doc_compaction: false,
                        redact: false,
                        traffic_pct: None,
                        shadow_model: None,
                        auto_pause: false,
                        pause_floor_pass_rate: None,
                        pause_min_verdicts: None,
                        minify_json: false,
                        reasoning_max_effort: None,
                        reasoning_budget_tokens: None,
                        agentic_budget: None,
                        panel: None,
                    },
                },
            )
            .await
            .unwrap();
        assert!(s.get_route(org_b, created.id).await.unwrap().is_none());
        assert!(!s.delete_route(org_b, created.id).await.unwrap());
        assert_eq!(s.list_all_for_org(org_b).await.unwrap().len(), 0);
    }
}
