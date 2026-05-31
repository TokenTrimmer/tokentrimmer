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

/// Fold a self-serve budget-cap override (from `org_budget_caps`, written by
/// the dashboard "Budget cap" setting — rv-budget-cap-ui §7.6) into `limits`.
///
/// Semantics:
/// * `monthly_cap_usd` — **sets** a spend cap where the tier has none. A paid
///   tier has `monthly_cap_usd: None` (overage-billed); a user-set value adds
///   the hard spend stop the pricing FAQ promises.
/// * `monthly_request_cap` — **only tightens**, never loosens. The Free tier's
///   10 000/mo hard-stop must survive a user setting a *larger* request cap, so
///   we take the `min` of the existing and user values.
///
/// `None` on a field leaves that dimension untouched. Pure + unit-tested; the
/// DB read that produces the arguments is best-effort (see
/// [`PostgresTierResolver::fetch_cap_override`]).
pub fn apply_cap_override(
    limits: &mut BudgetLimits,
    monthly_cap_usd: Option<f64>,
    monthly_request_cap: Option<u32>,
) {
    if let Some(cap) = monthly_cap_usd {
        if cap.is_finite() && cap >= 0.0 {
            limits.monthly_cap_usd = Some(cap);
        }
    }
    if let Some(user_cap) = monthly_request_cap {
        limits.monthly_request_cap = Some(match limits.monthly_request_cap {
            Some(existing) => existing.min(user_cap),
            None => user_cap,
        });
    }
}

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
        // enterprise is not a paid tier (rv-enterprise-dead-tier) so it never
        // reaches this arm — tier_budget_limits collapses it to Free (l2=false).
        (true, None) => match tier {
            "scale" => CallerTier::Scale,
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

    /// Best-effort read of `orgs.is_internal`. Fail-soft → `false` (never
    /// grants unlimited on error). Kept separate from the subscriptions read.
    async fn fetch_is_internal(&self, org_id: Uuid) -> bool {
        let row: Result<Option<(bool,)>, sqlx::Error> =
            sqlx::query_as(r#"SELECT is_internal FROM orgs WHERE id = $1"#)
                .bind(org_id)
                .fetch_optional(&self.pool)
                .await;
        matches!(row, Ok(Some((true,))))
    }

    /// Best-effort read of the per-org budget-cap override from
    /// `org_budget_caps` (rv-budget-cap-ui). Returns `(monthly_cap_usd,
    /// monthly_request_cap)`.
    ///
    /// **Fail-soft, NOT fail-open-to-Free:** any error (table not yet migrated,
    /// DB blip) returns `(None, None)` — i.e. *no override* — and logs `debug`.
    /// A missing/erroring cap table must never collapse a paid org's resolved
    /// tier; it only ever means "the user hasn't set a cap." The query is kept
    /// separate from the `subscriptions` read for exactly this reason.
    async fn fetch_cap_override(&self, org_id: Uuid) -> (Option<f64>, Option<u32>) {
        // (monthly_cap_usd, monthly_request_cap) — both nullable columns.
        type CapRow = (Option<f64>, Option<i32>);
        let row: Result<Option<CapRow>, sqlx::Error> = sqlx::query_as(
            r#"SELECT monthly_cap_usd, monthly_request_cap
               FROM org_budget_caps WHERE org_id = $1"#,
        )
        .bind(org_id)
        .fetch_optional(&self.pool)
        .await;
        match row {
            Ok(Some((usd, req))) => (usd, req.map(|r| r.max(0) as u32)),
            Ok(None) => (None, None),
            Err(e) => {
                tracing::debug!(error = %e, %org_id, "budget-cap read failed — no override applied");
                (None, None)
            }
        }
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
        // Internal orgs (platform-admin SP-0) get unlimited limits regardless
        // of their subscription record. Fail-soft: any DB error → false →
        // normal resolution continues.
        if self.fetch_is_internal(org_id).await {
            return Ok(ResolvedTier {
                caller_tier: CallerTier::Scale, // top tier label; semantics carried by unlimited limits
                limits: BudgetLimits::internal_unlimited(),
            });
        }

        let row: Option<(String, String)> = sqlx::query_as(
            r#"SELECT tier, status FROM subscriptions WHERE org_id = $1"#,
        )
        .bind(org_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| TierResolverError::Db { org_id, source })?;

        // No subscription row → Free default (safe for unregistered orgs);
        // otherwise map (tier, status) → limits.
        let (caller_tier, mut limits) = match row {
            Some((tier, status)) => caller_tier_from_strs(&tier, &status),
            None => {
                let d = ResolvedTier::free_default();
                (d.caller_tier, d.limits)
            }
        };

        // Fold in the self-serve budget cap (best-effort; never downgrades).
        let (cap_usd, cap_req) = self.fetch_cap_override(org_id).await;
        apply_cap_override(&mut limits, cap_usd, cap_req);

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

    // ── apply_cap_override ──────────────────────────────────────────────────

    #[test]
    fn cap_override_sets_spend_cap_on_paid_tier() {
        // Pro has no USD cap; a user-set cap adds the hard stop.
        let (_ct, mut l) = caller_tier_from_strs("pro", "active");
        assert_eq!(l.monthly_cap_usd, None);
        apply_cap_override(&mut l, Some(250.0), None);
        assert_eq!(l.monthly_cap_usd, Some(250.0));
        // Pro's rpm + L2 are untouched.
        assert_eq!(l.max_requests_per_min, Some(600));
        assert!(l.l2_cache);
    }

    #[test]
    fn cap_override_request_cap_only_tightens() {
        // Free has a 10 000/mo hard stop; a *larger* user value must NOT loosen it.
        let mut free = BudgetLimits::free_tier();
        apply_cap_override(&mut free, None, Some(50_000));
        assert_eq!(free.monthly_request_cap, Some(10_000), "must not loosen Free cap");
        // A smaller value tightens.
        let mut free2 = BudgetLimits::free_tier();
        apply_cap_override(&mut free2, None, Some(2_000));
        assert_eq!(free2.monthly_request_cap, Some(2_000));
    }

    #[test]
    fn cap_override_request_cap_sets_where_none() {
        // Pro has no request cap; a user value sets one.
        let (_ct, mut l) = caller_tier_from_strs("pro", "active");
        assert_eq!(l.monthly_request_cap, None);
        apply_cap_override(&mut l, None, Some(100_000));
        assert_eq!(l.monthly_request_cap, Some(100_000));
    }

    #[test]
    fn cap_override_none_is_noop() {
        let (_ct, before) = caller_tier_from_strs("team", "active");
        let mut after = before;
        apply_cap_override(&mut after, None, None);
        assert_eq!(after.monthly_cap_usd, before.monthly_cap_usd);
        assert_eq!(after.monthly_request_cap, before.monthly_request_cap);
        assert_eq!(after.max_requests_per_min, before.max_requests_per_min);
    }

    #[test]
    fn cap_override_rejects_nonfinite_or_negative_usd() {
        let mut l = BudgetLimits::free_tier();
        apply_cap_override(&mut l, Some(f64::NAN), None);
        assert_eq!(l.monthly_cap_usd, None, "NaN must be ignored");
        apply_cap_override(&mut l, Some(-5.0), None);
        assert_eq!(l.monthly_cap_usd, None, "negative must be ignored");
        apply_cap_override(&mut l, Some(0.0), None);
        assert_eq!(l.monthly_cap_usd, Some(0.0), "zero is a valid (hard-stop) cap");
    }

    // ── is_internal DB path ─────────────────────────────────────────────────
    //
    // `fetch_is_internal` and the `resolve()` short-circuit are exercised by
    // the SP-0 manual verification (see docs/superpowers/plans). No DB test is
    // added here because the existing `PostgresTierResolver` tests require a
    // live DATABASE_URL and are not run locally without one; adding a flaky DB
    // test would break the offline test suite. The pure unit coverage is
    // provided by `budget::tests::internal_unlimited_has_no_caps_and_l2_enabled`.

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
