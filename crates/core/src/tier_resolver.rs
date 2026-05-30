//! Per-org subscription tier resolution for request-time enforcement.
//!
//! ## Responsibility
//!
//! The [`TierResolver`] trait maps an `org_id` to the [`BudgetLimits`] the
//! gateway should enforce and the [`CallerTier`] that should be stamped on
//! [`tt_auth::ApiKeyContext`]. It is the single join point between the
//! subscription state in the cloud DB and per-request enforcement.
//!
//! ## Postgres implementation
//!
//! [`PostgresTierResolver`] issues a `SELECT tier, status FROM subscriptions
//! WHERE org_id = $1` query against the shared cloud-schema pool. No-row →
//! Free (safe default for orgs without a subscription record). The
//! `effective_tier` collapse (canceled/unpaid/incomplete → Free) is applied
//! in [`crate::budget::tier_budget_limits`], which is the gateway-side mirror
//! of `cloud/crates/api/src/tier.rs::effective_tier`.
//!
//! ## Short-TTL in-process cache
//!
//! [`CachedTierResolver`] wraps any resolver with a 30-second TTL
//! [`dashmap::DashMap`] cache, mirroring the pattern used by
//! [`crate::middleware::key_cache::KeyVerifyCache`]. This means:
//!
//! * The DB is NOT hit on every request (avoid per-request latency + load).
//! * A webhook-triggered tier change (upgrade/downgrade/cancel) is picked up
//!   within ~30 s without any explicit cache invalidation. If sub-30s
//!   propagation is ever required, a Redis pub/sub invalidation event can be
//!   layered on top without changing the trait surface.
//!
//! ## Fail-open contract
//!
//! A resolver error (DB blip, pool exhausted, etc.) MUST NOT hard-fail the
//! request. The middleware calls [`TierResolver::resolve`] and, on `Err`,
//! falls back to `(CallerTier::Free, BudgetLimits::free_tier())` and logs a
//! `warn`. Legitimate traffic is never blocked because tier resolution errored.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use thiserror::Error;
use tracing::warn;
use uuid::Uuid;

use tt_shared::CallerTier;

use crate::budget::{tier_budget_limits, BudgetLimits};

/// Errors from tier resolution. The middleware treats all variants as
/// fail-open (warn + Free fallback), so the variant detail is for logging.
#[derive(Debug, Error)]
pub enum TierResolverError {
    /// Database query failed.
    #[error("db error resolving tier for org {org_id}: {source}")]
    Db {
        org_id: Uuid,
        #[source]
        source: sqlx::Error,
    },
}

/// The resolved tier + enforcement limits for a single org.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedTier {
    /// Effective tier after the cancel-downgrade collapse.
    pub caller_tier: CallerTier,
    /// Gateway enforcement limits derived from the tier.
    pub limits: BudgetLimits,
}

impl ResolvedTier {
    /// Safe default — used when resolution fails (fail-open) or when no
    /// subscription record exists.
    pub fn free_default() -> Self {
        Self {
            caller_tier: CallerTier::Free,
            limits: BudgetLimits::free_tier(),
        }
    }
}

/// Resolve the effective tier and [`BudgetLimits`] for a given org.
///
/// Implementors must be `Send + Sync` (shared via `Arc` across async tasks).
#[async_trait]
pub trait TierResolver: Send + Sync {
    /// Return the [`ResolvedTier`] for `org_id`, or an error.
    ///
    /// The middleware ALWAYS falls back to Free on error — implementors do not
    /// need to handle the fallback themselves.
    async fn resolve(&self, org_id: Uuid) -> Result<ResolvedTier, TierResolverError>;
}

// ─── CallerTier helpers ─────────────────────────────────────────────────────

/// Parse the `tier` DB string + `status` DB string into a [`CallerTier`] after
/// applying the cancel-downgrade collapse. Unknown values → Free.
fn caller_tier_from_strs(tier: &str, status: &str) -> (CallerTier, BudgetLimits) {
    let limits = tier_budget_limits(tier, status);
    // Derive the CallerTier from the effective (post-collapse) limits.
    // We detect the effective tier by checking l2_cache + rpm since
    // tier_budget_limits already applied effective_tier collapse.
    let caller_tier = match (limits.l2_cache, limits.max_requests_per_min) {
        (false, _) => CallerTier::Free,
        (true, Some(600)) => CallerTier::Pro,
        (true, None) => match tier {
            "scale" | "enterprise" => CallerTier::Scale,
            _ => CallerTier::Team,
        },
        _ => CallerTier::Free,
    };
    (caller_tier, limits)
}

// ─── Postgres implementation ─────────────────────────────────────────────────

/// Postgres-backed [`TierResolver`].
///
/// Queries `subscriptions WHERE org_id = $1`. No row → Free (an org without
/// a subscription record is treated as Free). Uses the shared pool wired into
/// the `AppState` at startup.
#[derive(Clone)]
pub struct PostgresTierResolver {
    pool: sqlx::PgPool,
}

impl PostgresTierResolver {
    /// Construct from an existing pool.
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl std::fmt::Debug for PostgresTierResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresTierResolver")
            .field("pool", &"PgPool { .. }")
            .finish()
    }
}

#[async_trait]
impl TierResolver for PostgresTierResolver {
    async fn resolve(&self, org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
        let row: Option<(String, String)> = sqlx::query_as(
            r#"SELECT tier, status FROM subscriptions WHERE org_id = $1"#,
        )
        .bind(org_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| TierResolverError::Db { org_id, source })?;

        let Some((tier, status)) = row else {
            // No subscription row → Free default (safe for unregistered orgs).
            return Ok(ResolvedTier::free_default());
        };

        let (caller_tier, limits) = caller_tier_from_strs(&tier, &status);
        Ok(ResolvedTier {
            caller_tier,
            limits,
        })
    }
}

// ─── Short-TTL in-process cache ──────────────────────────────────────────────

/// Cache entry — the resolved value plus the instant it was inserted.
#[derive(Clone)]
struct CacheEntry {
    resolved: ResolvedTier,
    inserted_at: Instant,
}

/// TTL for a cached tier resolution. 30 s is short enough that a
/// webhook-triggered tier change (upgrade/cancel) is picked up within one
/// minute, while long enough to avoid a DB query on every request.
pub const TIER_CACHE_TTL_SECS: u64 = 30;

/// Wraps a [`TierResolver`] with a short-TTL in-process cache keyed by org_id.
///
/// ## Invalidation
///
/// No explicit invalidation is provided. Tier changes (webhook events) are
/// reflected within [`TIER_CACHE_TTL_SECS`] seconds. If sub-30s propagation is
/// ever required, a Redis pub/sub invalidation channel can be added without
/// changing this type's interface.
pub struct CachedTierResolver<R: TierResolver> {
    inner: R,
    cache: DashMap<Uuid, CacheEntry>,
    ttl: Duration,
}

impl<R: TierResolver> CachedTierResolver<R> {
    /// Wrap `inner` with the default [`TIER_CACHE_TTL_SECS`] TTL.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            cache: DashMap::new(),
            ttl: Duration::from_secs(TIER_CACHE_TTL_SECS),
        }
    }

    /// Wrap `inner` with a custom TTL (useful for tests).
    pub fn with_ttl(inner: R, ttl: Duration) -> Self {
        Self {
            inner,
            cache: DashMap::new(),
            ttl,
        }
    }
}

#[async_trait]
impl<R: TierResolver> TierResolver for CachedTierResolver<R> {
    async fn resolve(&self, org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
        let now = Instant::now();
        // Cache hit path.
        if let Some(entry) = self.cache.get(&org_id) {
            if now.duration_since(entry.inserted_at) <= self.ttl {
                return Ok(entry.resolved);
            }
            // Expired — drop the ref and re-query.
        }
        // Cache miss or expired: call the inner resolver.
        let resolved = self.inner.resolve(org_id).await?;
        self.cache.insert(
            org_id,
            CacheEntry {
                resolved,
                inserted_at: Instant::now(),
            },
        );
        Ok(resolved)
    }
}

// ─── Fail-open middleware helper ─────────────────────────────────────────────

/// Resolve the tier for `org_id` using `resolver`, falling back to
/// [`ResolvedTier::free_default()`] on any error (fail-open contract).
///
/// This is the function the auth middleware calls. It logs a `warn` on error so
/// the operator knows DB connectivity is degraded without stopping traffic.
pub async fn resolve_or_free(resolver: &dyn TierResolver, org_id: Uuid) -> ResolvedTier {
    match resolver.resolve(org_id).await {
        Ok(t) => t,
        Err(e) => {
            warn!(
                error = %e,
                %org_id,
                "tier resolution failed — failing open with Free defaults"
            );
            ResolvedTier::free_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // ── Counting stub resolver ──────────────────────────────────────────────

    struct StubResolver {
        result: Result<ResolvedTier, ()>,
        call_count: Arc<AtomicU32>,
    }

    impl StubResolver {
        fn ok(t: ResolvedTier) -> (Self, Arc<AtomicU32>) {
            let counter = Arc::new(AtomicU32::new(0));
            (
                Self {
                    result: Ok(t),
                    call_count: counter.clone(),
                },
                counter,
            )
        }

        fn err() -> (Self, Arc<AtomicU32>) {
            let counter = Arc::new(AtomicU32::new(0));
            (
                Self {
                    result: Err(()),
                    call_count: counter.clone(),
                },
                counter,
            )
        }
    }

    #[async_trait]
    impl TierResolver for StubResolver {
        async fn resolve(&self, _org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.result.clone().map_err(|_| TierResolverError::Db {
                org_id: Uuid::nil(),
                source: sqlx::Error::RowNotFound,
            })
        }
    }

    // ── caller_tier_from_strs ───────────────────────────────────────────────

    #[test]
    fn free_maps_to_caller_free() {
        let (ct, l) = caller_tier_from_strs("free", "active");
        assert_eq!(ct, CallerTier::Free);
        assert!(!l.l2_cache);
        assert_eq!(l.max_requests_per_min, Some(60));
    }

    #[test]
    fn pro_maps_to_caller_pro() {
        let (ct, l) = caller_tier_from_strs("pro", "active");
        assert_eq!(ct, CallerTier::Pro);
        assert!(l.l2_cache);
        assert_eq!(l.max_requests_per_min, Some(600));
    }

    #[test]
    fn team_maps_to_caller_team() {
        let (ct, _l) = caller_tier_from_strs("team", "active");
        assert_eq!(ct, CallerTier::Team);
    }

    #[test]
    fn scale_maps_to_caller_scale() {
        let (ct, _l) = caller_tier_from_strs("scale", "active");
        assert_eq!(ct, CallerTier::Scale);
    }

    #[test]
    fn canceled_pro_collapses_to_free_caller_tier() {
        let (ct, l) = caller_tier_from_strs("pro", "canceled");
        assert_eq!(ct, CallerTier::Free);
        assert!(!l.l2_cache);
    }

    // ── CachedTierResolver caching ──────────────────────────────────────────

    #[tokio::test]
    async fn second_resolve_hits_cache_not_inner() {
        let org = Uuid::from_u128(1);
        let (stub, counter) = StubResolver::ok(ResolvedTier::free_default());
        let cached = CachedTierResolver::new(stub);

        // First call — cache miss, inner called once.
        cached.resolve(org).await.expect("first");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Second call within TTL — cache hit, inner NOT called again.
        cached.resolve(org).await.expect("second");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "inner should not be called on cache hit");
    }

    #[tokio::test]
    async fn expired_entry_re_queries_inner() {
        let org = Uuid::from_u128(2);
        let (stub, counter) = StubResolver::ok(ResolvedTier::free_default());
        // Zero TTL forces every access to be a miss.
        let cached = CachedTierResolver::with_ttl(stub, Duration::ZERO);

        cached.resolve(org).await.expect("first");
        cached.resolve(org).await.expect("second");
        assert_eq!(counter.load(Ordering::SeqCst), 2, "zero-TTL must re-query on every call");
    }

    // ── resolve_or_free: fail-open ──────────────────────────────────────────

    #[tokio::test]
    async fn resolver_error_fails_open_to_free() {
        let org = Uuid::from_u128(3);
        let (stub, _counter) = StubResolver::err();
        let resolved = resolve_or_free(&stub, org).await;
        // Must not panic; must return Free defaults.
        assert_eq!(resolved.caller_tier, CallerTier::Free);
        assert!(!resolved.limits.l2_cache);
        assert_eq!(resolved.limits.max_requests_per_min, Some(60));
    }
}
