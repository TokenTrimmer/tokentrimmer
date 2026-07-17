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

use crate::store::{RouteManagementView, RoutingStore, RoutingStoreError};
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

    async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        self.inner.list_all_for_org(org_id).await
    }

    async fn list_management_for_org(
        &self,
        org_id: Uuid,
    ) -> Result<Vec<RouteManagementView>, RoutingStoreError> {
        // Never synthesize a management view from the cached runtime engine:
        // the engine intentionally omits invalid rows. Delegate to the raw
        // store so a legacy/manual row stays visible for repair.
        self.inner.list_management_for_org(org_id).await
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

    async fn get_management_route(
        &self,
        org_id: Uuid,
        id: Uuid,
    ) -> Result<Option<RouteManagementView>, RoutingStoreError> {
        self.inner.get_management_route(org_id, id).await
    }

    async fn delete_route(
        &self,
        org_id: Uuid,
        id: Uuid,
        expected_revision: i64,
    ) -> Result<bool, RoutingStoreError> {
        let removed = self
            .inner
            .delete_route(org_id, id, expected_revision)
            .await?;
        if removed {
            self.invalidate(org_id).await;
        }
        Ok(removed)
    }

    async fn pause_route(
        &self,
        org_id: Uuid,
        route_id: Uuid,
        expected_revision: i64,
        pause: crate::store::NewRoutePause,
    ) -> Result<bool, RoutingStoreError> {
        let paused = self
            .inner
            .pause_route(org_id, route_id, expected_revision, pause)
            .await?;
        if paused {
            // Invalidate so the pause takes effect immediately on THIS replica;
            // other replicas converge within the TTL (≤ 60s by default) — same
            // contract as create/delete above.
            self.invalidate(org_id).await;
        }
        Ok(paused)
    }

    async fn resume_route(
        &self,
        org_id: Uuid,
        route_id: Uuid,
        expected_revision: i64,
    ) -> Result<bool, RoutingStoreError> {
        let resumed = self
            .inner
            .resume_route(org_id, route_id, expected_revision)
            .await?;
        if resumed {
            // Same immediate-on-this-replica / ≤TTL-elsewhere convergence as
            // pause_route.
            self.invalidate(org_id).await;
        }
        Ok(resumed)
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

    /// A pause/resume through the caching store invalidates the org's cached
    /// engine, so the change takes effect immediately on this replica (no TTL
    /// wait) — mirrors `created_route_applies_immediately_without_ttl_wait`.
    #[tokio::test]
    async fn caching_store_pause_invalidates_engine() {
        let backing = Arc::new(InMemoryRoutingStore::new());
        let org = Uuid::now_v7();
        let cache = CachingRoutingStore::with_ttl(
            backing as Arc<dyn RoutingStore>,
            Duration::from_secs(3600), // long TTL: only invalidation can refresh
        );
        let created = cache
            .create_route(
                org,
                crate::store::NewRoute {
                    name: "down".into(),
                    priority: 10,
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
        // Warm the cache with the rewrite-active engine.
        assert!(!cache.engine_for(org).await.unwrap().routes()[0].paused);
        let revision = cache
            .get_management_route(org, created.id)
            .await
            .unwrap()
            .and_then(|route| route.revision)
            .expect("in-memory management reads carry a revision");

        // Pause via the caching store → immediately reflected (no TTL wait).
        assert!(cache
            .pause_route(
                org,
                created.id,
                revision,
                crate::store::NewRoutePause {
                    paused_by: crate::store::PausedBy::Auto,
                    reason: "auto: test".into(),
                    pass_rate: Some(0.5),
                    verdicts_in_window: Some(20),
                },
            )
            .await
            .unwrap());
        assert!(
            cache.engine_for(org).await.unwrap().routes()[0].paused,
            "pause must invalidate the cached engine"
        );

        // Resume → immediately reflected again.
        assert!(cache.resume_route(org, created.id, revision).await.unwrap());
        assert!(
            !cache.engine_for(org).await.unwrap().routes()[0].paused,
            "resume must invalidate the cached engine"
        );
    }

    #[tokio::test]
    async fn create_invalidates_so_engine_sees_it() {
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
            .create_route(
                org,
                crate::store::NewRoute {
                    name: "x".into(),
                    priority: 10,
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
        // Without invalidation the long-TTL cache would still say 0.
        assert_eq!(cache.engine_for(org).await.unwrap().routes().len(), 1);
    }
}
