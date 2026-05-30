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

use crate::Route;

/// Source of truth for an org's enabled routes.
///
/// Implementations return ALL enabled routes for `org_id`; ordering is the
/// caller's problem ([`crate::RoutingEngine::with_routes`] sorts internally).
#[async_trait]
pub trait RoutingStore: Send + Sync + std::fmt::Debug {
    /// Fetch the enabled-and-current route list for `org_id`. Returns
    /// `Ok(vec![])` when the org has no routes — not an error.
    async fn list_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError>;
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
}
