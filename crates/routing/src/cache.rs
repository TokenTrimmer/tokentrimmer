//! Per-org TTL cache in front of a [`crate::store::RoutingStore`].
//!
//! The chat handler evaluates routes on every request, so we cache the
//! per-org engine for ~60s. Cache miss falls through to the underlying store;
//! a single in-flight refresh is *not* deduplicated — at 60s TTL with a small
//! row count the redundant SELECT under brief contention is cheaper than
//! reaching for a `tokio::sync::Mutex` per org.
//!
//! Cache invalidation is time-based only: changes made through tt-api take
//! effect on the next refresh (≤ TTL). The dashboard surfaces this as
//! "routes refresh every ~60 seconds".

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use uuid::Uuid;

use crate::store::{RoutingStore, RoutingStoreError};
use crate::{Route, RoutingEngine};

/// Default per-org TTL.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Cached {
    engine: Arc<RoutingEngine>,
    expires_at: Instant,
}

/// Wraps any [`RoutingStore`] with a per-org [`RoutingEngine`] cache.
///
/// The cache also implements [`RoutingStore`] so callers can swap it in
/// without changing wiring. Prefer [`CachingRoutingStore::engine_for`] when
/// you actually want the pre-built engine — that's the hot path.
#[derive(Debug)]
pub struct CachingRoutingStore {
    inner: Arc<dyn RoutingStore>,
    ttl: Duration,
    cache: tokio::sync::RwLock<HashMap<Uuid, Cached>>,
}

impl CachingRoutingStore {
    pub fn new(inner: Arc<dyn RoutingStore>) -> Self {
        Self::with_ttl(inner, DEFAULT_TTL)
    }

    pub fn with_ttl(inner: Arc<dyn RoutingStore>, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            cache: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Hot path. Returns the cached engine for `org_id` when fresh, otherwise
    /// refreshes from the underlying store. Errors from the backend propagate
    /// — callers should treat them as "no routes" and dispatch unchanged.
    pub async fn engine_for(&self, org_id: Uuid) -> Result<Arc<RoutingEngine>, RoutingStoreError> {
        // Cheap fresh-cache check.
        {
            let g = self.cache.read().await;
            if let Some(entry) = g.get(&org_id) {
                if entry.expires_at > Instant::now() {
                    return Ok(Arc::clone(&entry.engine));
                }
            }
        }

        // Refresh.
        let routes = self.inner.list_for_org(org_id).await?;
        let engine = Arc::new(RoutingEngine::with_routes(routes));
        let mut g = self.cache.write().await;
        g.insert(
            org_id,
            Cached {
                engine: Arc::clone(&engine),
                expires_at: Instant::now() + self.ttl,
            },
        );
        Ok(engine)
    }

    /// Manually drop the cached entry for `org_id`. Used by tests; could
    /// later be wired to an admin "force refresh" endpoint.
    pub async fn invalidate(&self, org_id: Uuid) {
        let mut g = self.cache.write().await;
        g.remove(&org_id);
    }
}

#[async_trait]
impl RoutingStore for CachingRoutingStore {
    async fn list_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        let engine = self.engine_for(org_id).await?;
        Ok(engine.routes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::InMemoryRoutingStore;
    use crate::{RouteAction, RouteConditions};

    fn route(name: &str, target: &str) -> Route {
        Route {
            id: Uuid::now_v7(),
            name: name.into(),
            priority: 10,
            enabled: true,
            when: RouteConditions::default(),
            then: RouteAction {
                target_model: target.into(),
            },
        }
    }

    #[tokio::test]
    async fn caches_within_ttl() {
        let backing = Arc::new(InMemoryRoutingStore::new());
        let org = Uuid::now_v7();
        backing.set_routes(org, vec![route("a", "m1")]);

        let cache = CachingRoutingStore::with_ttl(
            backing.clone() as Arc<dyn RoutingStore>,
            Duration::from_secs(60),
        );

        let e1 = cache.engine_for(org).await.unwrap();
        // Mutate backing — cached engine should NOT see it.
        backing.set_routes(org, vec![route("b", "m2"), route("c", "m3")]);
        let e2 = cache.engine_for(org).await.unwrap();
        // Same Arc back from cache.
        assert!(Arc::ptr_eq(&e1, &e2));
        assert_eq!(e2.routes().len(), 1);
    }

    #[tokio::test]
    async fn refreshes_after_ttl_expires() {
        let backing = Arc::new(InMemoryRoutingStore::new());
        let org = Uuid::now_v7();
        backing.set_routes(org, vec![route("a", "m1")]);

        let cache = CachingRoutingStore::with_ttl(
            backing.clone() as Arc<dyn RoutingStore>,
            Duration::from_millis(50),
        );

        let e1 = cache.engine_for(org).await.unwrap();
        assert_eq!(e1.routes().len(), 1);

        // Bump backing + wait past TTL.
        backing.set_routes(org, vec![route("b", "m2"), route("c", "m3")]);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let e2 = cache.engine_for(org).await.unwrap();
        assert_eq!(e2.routes().len(), 2);
    }

    #[tokio::test]
    async fn invalidate_forces_refresh() {
        let backing = Arc::new(InMemoryRoutingStore::new());
        let org = Uuid::now_v7();
        backing.set_routes(org, vec![route("a", "m1")]);

        let cache = CachingRoutingStore::with_ttl(
            backing.clone() as Arc<dyn RoutingStore>,
            Duration::from_secs(3600),
        );
        let _ = cache.engine_for(org).await.unwrap();

        backing.set_routes(org, vec![route("b", "m2")]);
        cache.invalidate(org).await;
        let e = cache.engine_for(org).await.unwrap();
        assert_eq!(e.routes()[0].name, "b");
    }

    #[tokio::test]
    async fn empty_org_caches_too() {
        let backing = Arc::new(InMemoryRoutingStore::new());
        let cache = CachingRoutingStore::with_ttl(
            backing as Arc<dyn RoutingStore>,
            Duration::from_secs(60),
        );
        let e = cache.engine_for(Uuid::now_v7()).await.unwrap();
        assert!(e.routes().is_empty());
    }
}
