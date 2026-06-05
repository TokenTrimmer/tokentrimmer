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
}

#[derive(Debug, thiserror::Error)]
pub enum RoutingStoreError {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Test / dev backend. Holds a HashMap<org_id, Vec<Route>>; the gateway treats
/// it like any other store.
#[derive(Debug, Default)]
pub struct InMemoryRoutingStore {
    inner: RwLock<HashMap<Uuid, Vec<Route>>>,
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
}

#[async_trait]
impl RoutingStore for InMemoryRoutingStore {
    async fn list_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        let g = self.inner.read().expect("inmemory routing store poisoned");
        Ok(g.get(&org_id).cloned().unwrap_or_default())
    }

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
        Ok(g.get(&org_id)
            .and_then(|v| v.iter().find(|r| r.id == id).cloned()))
    }

    async fn delete_route(&self, org_id: Uuid, id: Uuid) -> Result<bool, RoutingStoreError> {
        let mut g = self.inner.write().expect("inmemory routing store poisoned");
        let Some(v) = g.get_mut(&org_id) else {
            return Ok(false);
        };
        let before = v.len();
        v.retain(|r| r.id != id);
        Ok(v.len() != before)
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
                "SELECT id, name, priority, conditions, target \
                 FROM routes \
                 WHERE org_id = $1 AND enabled = TRUE \
                 ORDER BY priority DESC, created_at ASC",
            )
            .bind(org_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RoutingStoreError::Backend(e.to_string()))?;

            Ok(rows.into_iter().filter_map(RouteRow::into_route).collect())
        }

        async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
            let rows = sqlx::query_as::<_, MgmtRouteRow>(
                "SELECT id, name, priority, enabled, conditions, target \
                 FROM routes WHERE org_id = $1 ORDER BY priority DESC, created_at ASC",
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
            row.into_route()
                .ok_or_else(|| RoutingStoreError::Backend("created route failed to decode".into()))
        }

        async fn get_route(
            &self,
            org_id: Uuid,
            id: Uuid,
        ) -> Result<Option<Route>, RoutingStoreError> {
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
    }

    #[derive(sqlx::FromRow)]
    struct RouteRow {
        id: Uuid,
        name: String,
        priority: i32,
        conditions: sqlx::types::Json<serde_json::Value>,
        target: sqlx::types::Json<serde_json::Value>,
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
                target_model: target.into(),
                fallbacks: Vec::new(),
                force_cache_layer: None,
                disable_cache: false,
                max_cost_usd: None,
            },
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
                target_model: "m1".into(),
                fallbacks: vec![],
                force_cache_layer: None,
                disable_cache: false,
                max_cost_usd: None,
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
                        target_model: "m".into(),
                        fallbacks: vec![],
                        force_cache_layer: None,
                        disable_cache: false,
                        max_cost_usd: None,
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
