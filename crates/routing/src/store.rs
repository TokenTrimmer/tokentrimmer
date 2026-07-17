//! Where routes come from at runtime.
//!
//! The gateway only knows about a [`RoutingStore`] trait. Production wires
//! [`PostgresRoutingStore`] (behind the `postgres` feature flag) reading the
//! `routes` table that the cloud dashboard writes; tests use
//! [`InMemoryRoutingStore`]. A separate [`crate::cache::CachingRoutingStore`]
//! wraps either with a per-org TTL cache so the hot path isn't a DB round-trip.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, RwLock};

use async_trait::async_trait;
use uuid::Uuid;

use crate::{
    canonicalize_route_value, Route, RouteAction, RouteConditions, RouteValidationIssue,
    ROUTE_SCHEMA_VERSION,
};

/// Fields needed to create a route; the store assigns the `id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
/// `POST /v1/routes/:id/pause?expected_revision=N`. Lowercase on the wire, matching the
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

    /// Legacy typed management accessor: all canonical routes, including
    /// disabled ones. Because its `Route` return type cannot represent a
    /// malformed persisted row, HTTP/admin callers must use
    /// [`Self::list_management_for_org`] instead.
    async fn list_all_for_org(&self, _org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "management unsupported by this store".into(),
        ))
    }
    /// Management: all stored route rows with their canonical activation
    /// assessment. Unlike [`Self::list_all_for_org`], this must not silently
    /// discard an invalid legacy/manual row merely because it cannot become a
    /// typed runtime [`Route`]. Implementations backed by raw persistence
    /// should override this method; the fallback gives typed-only stores the
    /// same response shape.
    async fn list_management_for_org(
        &self,
        org_id: Uuid,
    ) -> Result<Vec<RouteManagementView>, RoutingStoreError> {
        self.list_all_for_org(org_id)
            .await?
            .into_iter()
            .map(management_view_from_route)
            .collect()
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
    /// Management: fetch one stored route row with its activation assessment.
    /// See [`Self::list_management_for_org`] for why this is distinct from the
    /// typed runtime getter.
    async fn get_management_route(
        &self,
        org_id: Uuid,
        id: Uuid,
    ) -> Result<Option<RouteManagementView>, RoutingStoreError> {
        self.get_route(org_id, id)
            .await?
            .map(management_view_from_route)
            .transpose()
    }
    /// Management: delete one route owned by `org_id`, but only if its current
    /// generation matches the revision observed by the caller. `Ok(false)`
    /// means the row is absent or its definition has changed; callers must
    /// re-read before deciding which case applies.
    async fn delete_route(
        &self,
        _org_id: Uuid,
        _id: Uuid,
        _expected_revision: i64,
    ) -> Result<bool, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "management unsupported by this store".into(),
        ))
    }
    /// Sticky-pause a route's rewrite only when its current definition
    /// generation matches `expected_revision`. `Ok(true)` = newly paused;
    /// `Ok(false)` = already actively paused, absent/foreign route, or a stale
    /// generation token. The implementation must perform the generation check
    /// and pause mutation under the same row/mutation fence so a delayed pause
    /// cannot attach to a same-UUID replacement route. An active pause's
    /// evidence is never overwritten (sticky); a route paused again AFTER a
    /// resume records the NEW pause's evidence. A paused route still matches
    /// but every cost lever is suppressed — requests flow to the originally-
    /// requested model until [`RoutingStore::resume_route`].
    async fn pause_route(
        &self,
        _org_id: Uuid,
        _route_id: Uuid,
        _expected_revision: i64,
        _pause: NewRoutePause,
    ) -> Result<bool, RoutingStoreError> {
        Err(RoutingStoreError::Backend(
            "pause unsupported by this store".into(),
        ))
    }
    /// Clear a route's sticky pause only when its current definition
    /// generation matches `expected_revision`. `Ok(true)` = an active pause
    /// was cleared; `Ok(false)` = the route was not paused, was not owned by
    /// `org_id`, or the generation is stale. The implementation must fence the
    /// route generation and pause mutation together so an old resume cannot
    /// clear a same-UUID replacement's pause. This is the ONLY thing that
    /// clears a pause. The pause record is RETAINED with `resumed_at` stamped
    /// — that timestamp is the watermark the auto-pause verdict window is
    /// bounded by, so a resumed route is re-evaluated only on verdicts recorded
    /// AFTER the resume (the frozen pre-pause window can never instantly
    /// re-pause it).
    async fn resume_route(
        &self,
        _org_id: Uuid,
        _route_id: Uuid,
        _expected_revision: i64,
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

/// A route as seen by a management surface.
///
/// Runtime routing deliberately returns only typed, canonical [`Route`] values:
/// an invalid row must never be guessed into an executable rule. Management is
/// different. Operators need to see a legacy or manually-corrupted row in
/// order to repair or delete it, so this view preserves the stored JSON and
/// reports its current activation assessment alongside it.
///
/// `when`/`then` retain the public gateway API's established field names. The
/// cloud control plane uses its database names (`conditions`/`target`) at its
/// own boundary; both surfaces report the same schema, hash, and activation
/// semantics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RouteManagementView {
    pub id: Uuid,
    /// Opaque generation token required for a destructive route mutation.
    /// `None` means this store cannot establish revision-backed management
    /// evidence, so a caller must fail closed rather than delete the row.
    pub revision: Option<i64>,
    pub name: String,
    /// Kept signed so a manually-written negative database value can be shown
    /// faithfully instead of being coerced into a plausible route priority.
    pub priority: i64,
    pub enabled: bool,
    #[serde(rename = "when")]
    pub conditions: serde_json::Value,
    #[serde(rename = "then")]
    pub target: serde_json::Value,
    /// A pause suppresses the action while retaining matching/attribution. It
    /// is separate from canonical validity, so clients can distinguish a
    /// healthy-but-paused route from an invalid one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    /// The schema against which this exact stored representation was assessed.
    pub schema_version: u32,
    /// Stable identity of a canonical definition. `None` means the stored
    /// definition failed validation and is not eligible to execute.
    pub canonical_hash: Option<String>,
    pub activation: RouteManagementActivation,
}

/// Current canonical activation assessment for a [`RouteManagementView`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct RouteManagementActivation {
    /// `active` = canonical + enabled (subject to the separate `paused`
    /// execution overlay), `disabled` = canonical but disabled, and `invalid`
    /// = malformed/legacy/manual row that the runtime refuses to execute.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<RouteValidationIssue>,
}

/// Raw route fields read from a management backend before any typed decoding.
///
/// This stays private so runtime code continues to deal exclusively in typed
/// [`Route`] values. It is the crucial boundary that stops management reads
/// from dropping invalid rows through `Option<Route>`.
#[derive(Debug, Clone)]
struct RouteManagementRecord {
    id: Uuid,
    revision: Option<i64>,
    name: String,
    priority: i64,
    enabled: bool,
    conditions: serde_json::Value,
    target: serde_json::Value,
    paused: bool,
}

impl RouteManagementRecord {
    fn into_view(self) -> RouteManagementView {
        let Self {
            id,
            revision,
            name,
            priority,
            enabled,
            conditions,
            target,
            paused,
        } = self;

        // Use the gateway-shaped entry point here so the field paths in this
        // public API are `when.*` / `then.*`, matching the JSON we return.
        // (The cloud control plane calls its split-column adapter and reports
        // `conditions.*` / `target.*` at that separate boundary.)
        let assessment = canonicalize_route_value(serde_json::json!({
            "schema_version": ROUTE_SCHEMA_VERSION,
            "name": name.clone(),
            "priority": priority,
            "enabled": enabled,
            "when": conditions.clone(),
            "then": target.clone(),
        }));

        match assessment {
            Ok(canonical) => RouteManagementView {
                id,
                revision,
                name,
                priority,
                enabled,
                conditions,
                target,
                paused,
                schema_version: canonical.schema_version,
                canonical_hash: Some(canonical.canonical_hash),
                activation: RouteManagementActivation {
                    state: if enabled { "active" } else { "disabled" },
                    issues: Vec::new(),
                },
            },
            Err(issues) => RouteManagementView {
                id,
                revision,
                name,
                priority,
                enabled,
                conditions,
                target,
                paused,
                schema_version: ROUTE_SCHEMA_VERSION,
                canonical_hash: None,
                activation: RouteManagementActivation {
                    state: "invalid",
                    issues,
                },
            },
        }
    }
}

fn management_view_from_route_with_revision(
    route: Route,
    revision: Option<i64>,
) -> Result<RouteManagementView, RoutingStoreError> {
    let conditions = serde_json::to_value(&route.when).map_err(|error| {
        RoutingStoreError::Backend(format!("serialize route conditions: {error}"))
    })?;
    let target = serde_json::to_value(&route.then)
        .map_err(|error| RoutingStoreError::Backend(format!("serialize route target: {error}")))?;
    Ok(RouteManagementRecord {
        id: route.id,
        revision,
        name: route.name,
        priority: i64::from(route.priority),
        enabled: route.enabled,
        conditions,
        target,
        paused: route.paused,
    }
    .into_view())
}

fn management_view_from_route(route: Route) -> Result<RouteManagementView, RoutingStoreError> {
    management_view_from_route_with_revision(route, None)
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
    /// Test/dev equivalent of the database-owned route generation token.
    revisions: RwLock<HashMap<Uuid, i64>>,
    /// Each insert or direct test replacement receives a fresh generation so a
    /// stale delete cannot match a re-created UUID in focused non-DB tests.
    next_revision: AtomicI64,
    /// Serializes definition writes with their revision updates. Runtime reads
    /// need not take this lock; delete compares and removes under this fence.
    mutations: Mutex<()>,
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

    fn allocate_revision(&self) -> i64 {
        self.next_revision
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .expect("in-memory route revision overflow")
    }

    /// Replace the routes for an org. Useful for plant-and-assert tests.
    pub fn set_routes(&self, org_id: Uuid, routes: Vec<Route>) {
        let _mutation = self
            .mutations
            .lock()
            .expect("inmemory route mutation lock poisoned");
        let new_ids: Vec<Uuid> = routes.iter().map(|route| route.id).collect();
        let mut g = self.inner.write().expect("inmemory routing store poisoned");
        let old_ids: Vec<Uuid> = g
            .insert(org_id, routes)
            .unwrap_or_default()
            .into_iter()
            .map(|route| route.id)
            .collect();
        drop(g);
        let mut revisions = self
            .revisions
            .write()
            .expect("inmemory route revisions poisoned");
        for id in old_ids {
            revisions.remove(&id);
        }
        for id in new_ids {
            revisions.insert(id, self.allocate_revision());
        }
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

    /// Whether a definition write currently owns this exact route generation.
    /// Call only while `mutations` is held so the route and revision cannot be
    /// separately replaced between the two checks.
    fn route_matches_generation(&self, org_id: Uuid, id: Uuid, expected_revision: i64) -> bool {
        if expected_revision < 1 {
            return false;
        }
        if self
            .revisions
            .read()
            .expect("inmemory route revisions poisoned")
            .get(&id)
            .copied()
            != Some(expected_revision)
        {
            return false;
        }
        self.inner
            .read()
            .expect("inmemory routing store poisoned")
            .get(&org_id)
            .is_some_and(|routes| routes.iter().any(|route| route.id == id))
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

    async fn list_management_for_org(
        &self,
        org_id: Uuid,
    ) -> Result<Vec<RouteManagementView>, RoutingStoreError> {
        let routes = self.list_all_for_org(org_id).await?;
        let revisions = self
            .revisions
            .read()
            .expect("inmemory route revisions poisoned");
        routes
            .into_iter()
            .map(|route| {
                let revision = revisions.get(&route.id).copied();
                management_view_from_route_with_revision(route, revision)
            })
            .collect()
    }

    async fn create_route(&self, org_id: Uuid, spec: NewRoute) -> Result<Route, RoutingStoreError> {
        let _mutation = self
            .mutations
            .lock()
            .expect("inmemory route mutation lock poisoned");
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
        drop(g);
        self.revisions
            .write()
            .expect("inmemory route revisions poisoned")
            .insert(route.id, self.allocate_revision());
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

    async fn get_management_route(
        &self,
        org_id: Uuid,
        id: Uuid,
    ) -> Result<Option<RouteManagementView>, RoutingStoreError> {
        let route = self.get_route(org_id, id).await?;
        let revisions = self
            .revisions
            .read()
            .expect("inmemory route revisions poisoned");
        route
            .map(|route| {
                let revision = revisions.get(&route.id).copied();
                management_view_from_route_with_revision(route, revision)
            })
            .transpose()
    }

    async fn delete_route(
        &self,
        org_id: Uuid,
        id: Uuid,
        expected_revision: i64,
    ) -> Result<bool, RoutingStoreError> {
        if expected_revision < 1 {
            return Ok(false);
        }
        let _mutation = self
            .mutations
            .lock()
            .expect("inmemory route mutation lock poisoned");
        if self
            .revisions
            .read()
            .expect("inmemory route revisions poisoned")
            .get(&id)
            .copied()
            != Some(expected_revision)
        {
            return Ok(false);
        }
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
            self.revisions
                .write()
                .expect("inmemory route revisions poisoned")
                .remove(&id);
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
        expected_revision: i64,
        pause: NewRoutePause,
    ) -> Result<bool, RoutingStoreError> {
        let _mutation = self
            .mutations
            .lock()
            .expect("inmemory route mutation lock poisoned");
        // Generation + ownership guard: a delayed pause must not attach to a
        // replacement that reused this UUID. Mirrors the Postgres
        // `SELECT .. FOR UPDATE` generation fence.
        if !self.route_matches_generation(org_id, route_id, expected_revision) {
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

    async fn resume_route(
        &self,
        org_id: Uuid,
        route_id: Uuid,
        expected_revision: i64,
    ) -> Result<bool, RoutingStoreError> {
        let _mutation = self
            .mutations
            .lock()
            .expect("inmemory route mutation lock poisoned");
        if !self.route_matches_generation(org_id, route_id, expected_revision) {
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
    /// Runtime reads skip rows whose `conditions` or `target` JSON fails to
    /// decode — a single malformed row must not knock out routing for the
    /// org. Each enabled invalid row observed at a runtime store refresh emits
    /// the low-cardinality `tt_route_invalid_persisted_rows_total` counter and
    /// a payload-free structured warning so it is not operationally silent.
    /// Management reads deliberately return those rows with `activation:
    /// invalid` and field-addressed issues so an operator can repair or delete
    /// them. Wrap in [`crate::cache::CachingRoutingStore`] to amortize the
    /// runtime SELECT across hot-path requests.
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
                "SELECT r.id, r.revision, r.name, r.priority, r.enabled, r.conditions, r.target, \
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

        async fn list_management_for_org(
            &self,
            org_id: Uuid,
        ) -> Result<Vec<RouteManagementView>, RoutingStoreError> {
            let rows = sqlx::query_as::<_, MgmtRouteRow>(
                "SELECT r.id, r.revision, r.name, r.priority, r.enabled, r.conditions, r.target, \
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
                .map(MgmtRouteRow::into_management_view)
                .collect())
        }

        async fn create_route(
            &self,
            org_id: Uuid,
            spec: crate::store::NewRoute,
        ) -> Result<Route, RoutingStoreError> {
            // Route HTTP writes reject this in canonical validation. Keep the
            // storage boundary fail-closed as well because CLI/internal
            // callers can invoke the store directly.
            let database_priority = i32::try_from(spec.priority).map_err(|_| {
                RoutingStoreError::Backend(
                    "route priority exceeds the database-supported range".into(),
                )
            })?;
            let conditions = serde_json::to_value(&spec.when)
                .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            let target = serde_json::to_value(&spec.then)
                .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            let row = sqlx::query_as::<_, MgmtRouteRow>(
                // A freshly-created route can have no pause row (new id) —
                // FALSE AS paused keeps the RETURNING shape join-free.
                "INSERT INTO routes (org_id, name, priority, conditions, target, enabled) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 RETURNING id, revision, name, priority, enabled, conditions, target, FALSE AS paused",
            )
            .bind(org_id)
            .bind(&spec.name)
            .bind(database_priority)
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
                "SELECT r.id, r.revision, r.name, r.priority, r.enabled, r.conditions, r.target, \
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

        async fn get_management_route(
            &self,
            org_id: Uuid,
            id: Uuid,
        ) -> Result<Option<RouteManagementView>, RoutingStoreError> {
            let row = sqlx::query_as::<_, MgmtRouteRow>(
                "SELECT r.id, r.revision, r.name, r.priority, r.enabled, r.conditions, r.target, \
                        (p.route_id IS NOT NULL) AS paused \
                 FROM routes r LEFT JOIN route_pauses p ON p.route_id = r.id AND p.resumed_at IS NULL \
                 WHERE r.org_id = $1 AND r.id = $2",
            )
            .bind(org_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            Ok(row.map(MgmtRouteRow::into_management_view))
        }

        async fn delete_route(
            &self,
            org_id: Uuid,
            id: Uuid,
            expected_revision: i64,
        ) -> Result<bool, RoutingStoreError> {
            if expected_revision < 1 {
                return Ok(false);
            }
            // The pause record (active OR resumed watermark) is GC'd with its
            // route — `route_pauses` deliberately has no FK to the cloud-owned
            // `routes` table, so the cleanup is explicit here. Crucially, the
            // route DELETE is revision-guarded before the pause cleanup can
            // run: a stale delete must not clear the pause of a newer route
            // generation. A recreated route gets a fresh revision, so an old
            // observed token cannot remove it.
            let removed: bool = sqlx::query_scalar(
                "WITH deleted AS ( \
                    DELETE FROM routes WHERE org_id = $1 AND id = $2 AND revision = $3 \
                    RETURNING id \
                 ), gc AS ( \
                    DELETE FROM route_pauses \
                    WHERE org_id = $1 AND route_id IN (SELECT id FROM deleted) \
                 ) \
                 SELECT EXISTS(SELECT 1 FROM deleted)",
            )
            .bind(org_id)
            .bind(id)
            .bind(expected_revision)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;
            Ok(removed)
        }

        async fn pause_route(
            &self,
            org_id: Uuid,
            route_id: Uuid,
            expected_revision: i64,
            pause: crate::store::NewRoutePause,
        ) -> Result<bool, RoutingStoreError> {
            if expected_revision < 1 {
                return Ok(false);
            }
            // Lock the matching route generation before touching the separate
            // no-FK pause row. A plain INSERT..SELECT revision predicate is
            // not sufficient: a concurrent DELETE + same-UUID reinsert could
            // occur after its SELECT snapshot but before its INSERT. FOR UPDATE
            // serializes definition replacement/patches with this pause write.
            // An ACTIVE pause (resumed_at IS NULL) is sticky — the conditional
            // ON CONFLICT update touches nothing and reports Ok(false), keeping
            // the first pause's evidence. A RESUMED watermark row is overwritten
            // by the new pause's evidence (re-pause after resume), resetting
            // resumed_at to NULL.
            let res = sqlx::query(
                "WITH matched AS ( \
                   SELECT r.id, r.org_id FROM routes r \
                    WHERE r.org_id = $1 AND r.id = $2 AND r.revision = $3 \
                    FOR UPDATE \
                 ) \
                 INSERT INTO route_pauses \
                   (route_id, org_id, paused_by, reason, pass_rate, verdicts_in_window) \
                 SELECT m.id, m.org_id, $4, $5, $6, $7 FROM matched m \
                 ON CONFLICT (route_id) DO UPDATE SET \
                   paused_at = now(), paused_by = EXCLUDED.paused_by, \
                   reason = EXCLUDED.reason, pass_rate = EXCLUDED.pass_rate, \
                   verdicts_in_window = EXCLUDED.verdicts_in_window, resumed_at = NULL \
                 WHERE route_pauses.resumed_at IS NOT NULL",
            )
            .bind(org_id)
            .bind(route_id)
            .bind(expected_revision)
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
            expected_revision: i64,
        ) -> Result<bool, RoutingStoreError> {
            if expected_revision < 1 {
                return Ok(false);
            }
            // The record is RETAINED with resumed_at stamped: that timestamp
            // is the watermark `RECENT_CLASSIFIED_SQL` (tt-core) bounds the
            // auto-pause verdict window by, so a resumed route is re-evaluated
            // only on post-resume verdicts. Lock the matching definition
            // generation before updating the no-FK pause row, so a stale resume
            // cannot clear a same-UUID replacement route's pause. Resuming an
            // already-resumed (or never-paused) route touches nothing → Ok(false).
            let res = sqlx::query(
                "WITH matched AS ( \
                   SELECT r.id, r.org_id FROM routes r \
                    WHERE r.org_id = $1 AND r.id = $2 AND r.revision = $3 \
                    FOR UPDATE \
                 ) \
                 UPDATE route_pauses AS pause SET resumed_at = now() \
                 FROM matched \
                 WHERE pause.org_id = matched.org_id \
                   AND pause.route_id = matched.id \
                   AND pause.resumed_at IS NULL",
            )
            .bind(org_id)
            .bind(route_id)
            .bind(expected_revision)
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
            let canonical = match crate::canonicalize_route_parts(
                Some(crate::ROUTE_SCHEMA_VERSION),
                self.name.clone(),
                self.priority,
                true,
                self.conditions.0,
                self.target.0,
            ) {
                Ok(route) => route,
                Err(issues) => {
                    // Pre-P1 legacy/corrupt rows cannot be allowed to execute
                    // under a guessed interpretation. The control plane exposes
                    // the same issues as `activation: invalid`. Runtime emits
                    // only a bounded metric plus route identity / issue count;
                    // raw malformed JSON and validation messages stay out of
                    // gateway logs and remain available through management.
                    metrics::counter!("tt_route_invalid_persisted_rows_total").increment(1);
                    tracing::warn!(
                        event = "route_invalid_persisted_row_skipped",
                        route_id = %self.id,
                        issue_count = issues.len(),
                        "enabled persisted route is invalid under the canonical schema; not activating it"
                    );
                    return None;
                }
            };
            let route = canonical.route;
            Some(Route {
                id: self.id,
                name: route.name,
                priority: route.priority,
                enabled: route.enabled,
                when: route.when,
                then: route.then,
                paused: self.paused,
            })
        }
    }

    /// Like [`RouteRow`] but carries `enabled` (management lists disabled routes too).
    #[derive(sqlx::FromRow)]
    struct MgmtRouteRow {
        id: Uuid,
        revision: i64,
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
            let canonical = match crate::canonicalize_route_parts(
                Some(crate::ROUTE_SCHEMA_VERSION),
                self.name.clone(),
                self.priority,
                self.enabled,
                self.conditions.0,
                self.target.0,
            ) {
                Ok(route) => route,
                Err(issues) => {
                    tracing::warn!(route_id = %self.id, ?issues, "route is invalid under the canonical schema; omitting it from typed legacy accessor");
                    return None;
                }
            };
            let route = canonical.route;
            Some(Route {
                id: self.id,
                name: route.name,
                priority: route.priority,
                enabled: route.enabled,
                when: route.when,
                then: route.then,
                paused: self.paused,
            })
        }

        fn into_management_view(self) -> RouteManagementView {
            RouteManagementRecord {
                id: self.id,
                revision: Some(self.revision),
                name: self.name,
                priority: i64::from(self.priority),
                enabled: self.enabled,
                conditions: self.conditions.0,
                target: self.target.0,
                paused: self.paused,
            }
            .into_view()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::OnceLock;

        static TEST_METRICS: OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
            OnceLock::new();

        fn metrics_handle() -> &'static metrics_exporter_prometheus::PrometheusHandle {
            TEST_METRICS.get_or_init(|| {
                metrics_exporter_prometheus::PrometheusBuilder::new()
                    .install_recorder()
                    .expect("the focused routing test owns its process-global metrics recorder")
            })
        }

        /// A bad enabled row remains non-executable and emits the bounded
        /// observability signal. The test owns one process-global recorder,
        /// matching the gateway's recorder model.
        #[test]
        fn invalid_enabled_runtime_row_is_skipped_and_observed() {
            let metrics = metrics_handle();
            let row = RouteRow {
                id: Uuid::now_v7(),
                name: "invalid".into(),
                priority: 100,
                conditions: sqlx::types::Json(serde_json::json!({})),
                target: sqlx::types::Json(serde_json::json!({
                    "target_model": 7,
                })),
                paused: false,
            };

            assert!(
                row.into_route().is_none(),
                "an invalid persisted row must not become an executable route"
            );
            assert!(
                metrics
                    .render()
                    .contains("tt_route_invalid_persisted_rows_total"),
                "runtime invalid-row counter must be exposed to the installed recorder"
            );
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
                document_lane: false,
                content_compress: false,
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
                workflow: None,
            },
            paused: false,
        }
    }

    async fn observed_revision(store: &InMemoryRoutingStore, org: Uuid, id: Uuid) -> i64 {
        store
            .get_management_route(org, id)
            .await
            .expect("read management route")
            .and_then(|route| route.revision)
            .expect("in-memory management read must carry a positive revision")
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
                document_lane: false,
                content_compress: false,
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
                workflow: None,
            },
        };
        let created = s.create_route(org, spec).await.unwrap();
        assert_eq!(created.name, "pin");

        let all = s.list_all_for_org(org).await.unwrap();
        assert_eq!(all.len(), 1);

        let got = s.get_route(org, created.id).await.unwrap();
        assert_eq!(got.unwrap().id, created.id);

        let revision = observed_revision(&s, org, created.id).await;
        assert!(revision >= 1);
        assert!(
            !s.delete_route(org, created.id, revision + 1).await.unwrap(),
            "a stale revision must not delete the current route"
        );
        assert!(s.get_route(org, created.id).await.unwrap().is_some());
        assert!(s.delete_route(org, created.id, revision).await.unwrap());
        assert!(s.get_route(org, created.id).await.unwrap().is_none());
        assert!(!s.delete_route(org, created.id, revision).await.unwrap());
    }

    #[tokio::test]
    async fn in_memory_reinserted_route_id_rejects_prior_generation_delete() {
        let s = InMemoryRoutingStore::new();
        let org = Uuid::now_v7();
        let mut replacement = route("first-generation", 10, "m1");
        let id = replacement.id;
        s.set_routes(org, vec![replacement.clone()]);
        let first_revision = observed_revision(&s, org, id).await;

        assert!(s.delete_route(org, id, first_revision).await.unwrap());

        // A test fixture/manual writer can reuse a UUID. It must still create
        // a new generation, so a delayed delete authorized for the old row
        // cannot remove this replacement definition.
        replacement.name = "replacement-generation".into();
        s.set_routes(org, vec![replacement]);
        let replacement_revision = observed_revision(&s, org, id).await;
        assert!(replacement_revision > first_revision);
        assert!(!s.delete_route(org, id, first_revision).await.unwrap());
        assert_eq!(
            s.get_route(org, id).await.unwrap().unwrap().name,
            "replacement-generation"
        );
        assert!(s.delete_route(org, id, replacement_revision).await.unwrap());
    }

    #[tokio::test]
    async fn in_memory_stale_pause_and_resume_cannot_touch_reinserted_generation() {
        let s = InMemoryRoutingStore::new();
        let org = Uuid::now_v7();
        let mut replacement = route("first-generation", 10, "m1");
        let id = replacement.id;
        s.set_routes(org, vec![replacement.clone()]);
        let first_revision = observed_revision(&s, org, id).await;
        assert!(s.delete_route(org, id, first_revision).await.unwrap());

        replacement.name = "replacement-generation".into();
        s.set_routes(org, vec![replacement]);
        let replacement_revision = observed_revision(&s, org, id).await;
        assert!(replacement_revision > first_revision);

        // An old manual pause must not attach its state to a replacement that
        // deliberately reused this UUID.
        assert!(!s
            .pause_route(org, id, first_revision, pause(PausedBy::Manual))
            .await
            .unwrap());
        assert!(s.pause_record(id).is_none());
        assert!(!s.get_route(org, id).await.unwrap().unwrap().paused);

        assert!(s
            .pause_route(org, id, replacement_revision, pause(PausedBy::Auto),)
            .await
            .unwrap());
        // A delayed old resume likewise leaves the replacement's active pause
        // intact. Only a current-generation token may clear it.
        assert!(!s.resume_route(org, id, first_revision).await.unwrap());
        assert!(s.get_route(org, id).await.unwrap().unwrap().paused);
        assert!(s.resume_route(org, id, replacement_revision).await.unwrap());
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
        let revision = observed_revision(&s, org, created.id).await;

        assert!(
            s.pause_route(org, created.id, revision, pause(PausedBy::Auto))
                .await
                .unwrap(),
            "first pause must report newly-paused"
        );
        assert!(s.get_route(org, created.id).await.unwrap().unwrap().paused);
        assert!(s.list_for_org(org).await.unwrap()[0].paused);
        assert!(s.list_all_for_org(org).await.unwrap()[0].paused);

        // Sticky/idempotent: a second pause changes nothing.
        assert!(
            !s.pause_route(org, created.id, revision, pause(PausedBy::Manual))
                .await
                .unwrap(),
            "already-paused must be Ok(false)"
        );

        assert!(
            s.resume_route(org, created.id, revision).await.unwrap(),
            "resume clears the pause"
        );
        assert!(!s.get_route(org, created.id).await.unwrap().unwrap().paused);
        assert!(!s.list_for_org(org).await.unwrap()[0].paused);
        assert!(
            !s.resume_route(org, created.id, revision).await.unwrap(),
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
            s.pause_route(org, created.id, revision, pause(PausedBy::Manual))
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
        assert!(s.delete_route(org, created.id, revision).await.unwrap());
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
        let revision = observed_revision(&s, org_a, created.id).await;
        // Foreign-org pause is a no-op.
        assert!(!s
            .pause_route(org_b, created.id, revision, pause(PausedBy::Manual))
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
            .pause_route(org_a, created.id, revision, pause(PausedBy::Auto))
            .await
            .unwrap());
        assert!(!s.resume_route(org_b, created.id, revision).await.unwrap());
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
                        document_lane: false,
                        content_compress: false,
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
                        workflow: None,
                    },
                },
            )
            .await
            .unwrap();
        assert!(s.get_route(org_b, created.id).await.unwrap().is_none());
        let revision = observed_revision(&s, org_a, created.id).await;
        assert!(!s.delete_route(org_b, created.id, revision).await.unwrap());
        assert_eq!(s.list_all_for_org(org_b).await.unwrap().len(), 0);
    }
}
