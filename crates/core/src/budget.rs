//! Per-org spend cap + request-rate enforcement — the gateway's "hard spend
//! cap" primitive.
//!
//! ## Pre-flight model
//!
//! A request's cost is only known *after* the provider responds, so the spend
//! cap is enforced **pre-flight on accumulated spend**: once an org's recorded
//! spend for the current month reaches the cap, new requests are rejected. This
//! cannot prevent the single request that crosses the cap (its cost isn't known
//! yet) — it bounds spend to roughly `cap + one_request`. The per-minute rate
//! limit, by contrast, is fully pre-enforceable by counting attempts.
//!
//! The clock is injected via a `now` parameter so the logic is deterministic
//! and unit-testable (no internal `Utc::now()` calls).
//!
//! The trait + in-memory impl live here (public/OSS). A Postgres-backed impl
//! with per-org limits is a cloud follow-up; it just implements
//! [`BudgetEnforcer`].

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Utc};
use uuid::Uuid;

/// Length of the rolling request-rate window.
const RATE_WINDOW_SECS: i64 = 60;

/// Per-org limits. `None` on a field disables that dimension.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetLimits {
    /// Monthly spend cap in USD. `None` = unlimited spend.
    pub monthly_cap_usd: Option<f64>,
    /// Max requests per rolling 60s window. `None` = unlimited rate.
    pub max_requests_per_min: Option<u32>,
    /// Max requests per calendar month. `None` = unlimited monthly volume.
    /// Resets with the spend accumulator at the month boundary.
    pub monthly_request_cap: Option<u32>,
    /// Whether the L2 semantic cache is available for this tier.
    /// Free is L1-only (`false`); Pro/Team/Scale enable L2 (`true`).
    pub l2_cache: bool,
}

impl BudgetLimits {
    /// The Free-tier shape: 60 requests/minute, 10 000 requests/month, no USD
    /// cap (Free is metered by volume, not spend), no L2 cache. Mirrors
    /// `tt_api::tier::limits_for("free")` — see [`tier_budget_limits`] for the
    /// full mapping.
    #[must_use]
    pub fn free_tier() -> Self {
        Self {
            monthly_cap_usd: None,
            max_requests_per_min: Some(60),
            monthly_request_cap: Some(10_000),
            l2_cache: false,
        }
    }

    /// Limits for an INTERNAL org (platform-admin SP-0): everything unbounded,
    /// L2 on. Used by the tier resolver when `orgs.is_internal` is true so the
    /// owner/staff can test without limits.
    #[must_use]
    pub fn internal_unlimited() -> Self {
        Self {
            monthly_cap_usd: None,
            max_requests_per_min: None,
            monthly_request_cap: None,
            l2_cache: true,
        }
    }
}

/// Map a subscription `(tier, status)` pair → the [`BudgetLimits`] the gateway
/// enforces at request time. Applies the same cancel-downgrade rule as
/// `tt_api::tier::effective_tier` (CLOUD canonical source): non-paying statuses
/// collapse to Free.
///
/// **Sync note:** the band numbers below mirror `cloud/crates/api/src/tier.rs`
/// (`limits_for`). When the cloud canonical values change, update both files and
/// keep this comment pointing at the cloud source.
///
/// | Tier  | rpm cap | monthly cap | L2 cache |
/// |-------|---------|-------------|----------|
/// | free  | 60      | 10 000      | no       |
/// | pro   | 600     | unlimited   | yes      |
/// | team  | unlimited | unlimited | yes      |
/// | scale | unlimited | unlimited | yes      |
#[must_use]
pub fn tier_budget_limits(tier: &str, status: &str) -> BudgetLimits {
    // Apply the cancel-downgrade collapse (mirrors cloud effective_tier).
    let effective = match status {
        "active" | "trialing" | "past_due" => tier,
        // canceled, unpaid, incomplete, incomplete_expired → Free.
        _ => "free",
    };
    match effective {
        "pro" => BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: Some(600),
            monthly_request_cap: None, // paid tiers bill overage, no hard-stop
            l2_cache: true,
        },
        "team" | "scale" => BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None, // unlimited rpm for team/scale
            monthly_request_cap: None,
            l2_cache: true,
        },
        // "free", "", "enterprise", unknown → Free caps.
        //
        // `enterprise` is intentionally NOT a paid tier (rv-enterprise-dead-tier,
        // §7.5): no Stripe product exists for it, so a stray
        // `subscriptions.tier='enterprise'` must collapse to Free here, matching
        // `tt_api::tier::limits_for` + `overage::rate_per_request_cents`.
        _ => BudgetLimits::free_tier(),
    }
}

/// Outcome of a pre-flight budget check.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetDecision {
    /// Request may proceed. `spend_remaining_usd` is the headroom under the
    /// monthly cap, or `None` when spend is unlimited.
    Allow { spend_remaining_usd: Option<f64> },
    /// Monthly spend cap reached — no headroom left this month.
    DenySpend,
    /// Monthly request-count cap reached — no requests left this month.
    DenyMonthlyRequests,
    /// Per-minute request rate exceeded; client should retry after the given
    /// number of seconds (when the window rolls over).
    DenyRate { retry_after_secs: u64 },
}

impl BudgetDecision {
    /// `true` only for [`BudgetDecision::Allow`].
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, BudgetDecision::Allow { .. })
    }
}

/// Pre-flight gate + post-request spend recorder, keyed per org.
pub trait BudgetEnforcer: Send + Sync {
    /// Pre-flight: may `org_id` make a request at `now`? Counts the attempt
    /// against the per-minute rate window.
    fn check(&self, org_id: Uuid, now: DateTime<Utc>) -> BudgetDecision;

    /// Record realized spend after the response. `cost_usd` may be `0.0`
    /// (e.g. a cache hit) — that still happened, it just costs nothing.
    fn record(&self, org_id: Uuid, cost_usd: f64, now: DateTime<Utc>);
}

/// In-memory [`BudgetEnforcer`] for single-node / dev / tests. One default
/// [`BudgetLimits`] applies to every org; per-org overrides + persistence are
/// a cloud follow-up.
pub struct InMemoryBudgetEnforcer {
    limits: BudgetLimits,
    state: Mutex<HashMap<Uuid, OrgState>>,
}

struct OrgState {
    /// `(year, month)` the monthly accumulators belong to; reset across months.
    month: (i32, u32),
    spend_usd: f64,
    /// Requests counted this calendar month (for `monthly_request_cap`).
    month_request_count: u32,
    /// Start of the current rate window.
    window_start: DateTime<Utc>,
    window_count: u32,
}

impl OrgState {
    fn fresh(now: DateTime<Utc>) -> Self {
        Self {
            month: (now.year(), now.month()),
            spend_usd: 0.0,
            month_request_count: 0,
            window_start: now,
            window_count: 0,
        }
    }

    /// Reset the monthly accumulators (spend + request count) when `now` falls
    /// in a new month.
    fn roll_month(&mut self, now: DateTime<Utc>) {
        let ym = (now.year(), now.month());
        if self.month != ym {
            self.month = ym;
            self.spend_usd = 0.0;
            self.month_request_count = 0;
        }
    }
}

impl InMemoryBudgetEnforcer {
    #[must_use]
    pub fn new(limits: BudgetLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(HashMap::new()),
        }
    }
}

impl BudgetEnforcer for InMemoryBudgetEnforcer {
    fn check(&self, org_id: Uuid, now: DateTime<Utc>) -> BudgetDecision {
        let mut guard = self.state.lock().expect("budget state poisoned");
        let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
        st.roll_month(now);

        // Spend cap — pre-flight on accumulated spend.
        if let Some(cap) = self.limits.monthly_cap_usd {
            if st.spend_usd >= cap {
                return BudgetDecision::DenySpend;
            }
        }

        // Monthly request-count cap — pre-flight on attempts this month.
        if let Some(mcap) = self.limits.monthly_request_cap {
            if st.month_request_count >= mcap {
                return BudgetDecision::DenyMonthlyRequests;
            }
        }

        // Per-minute rate window (also counts this attempt).
        if let Some(rpm) = self.limits.max_requests_per_min {
            let elapsed = now.signed_duration_since(st.window_start).num_seconds();
            if elapsed >= RATE_WINDOW_SECS {
                st.window_start = now;
                st.window_count = 1;
            } else if st.window_count >= rpm {
                let retry = (RATE_WINDOW_SECS - elapsed).max(0) as u64;
                return BudgetDecision::DenyRate {
                    retry_after_secs: retry,
                };
            } else {
                st.window_count += 1;
            }
        }

        // The request passed every gate — count it toward the monthly volume.
        st.month_request_count = st.month_request_count.saturating_add(1);

        BudgetDecision::Allow {
            spend_remaining_usd: self
                .limits
                .monthly_cap_usd
                .map(|cap| (cap - st.spend_usd).max(0.0)),
        }
    }

    fn record(&self, org_id: Uuid, cost_usd: f64, now: DateTime<Utc>) {
        let mut guard = self.state.lock().expect("budget state poisoned");
        let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
        st.roll_month(now);
        st.spend_usd += cost_usd.max(0.0);
    }
}

/// Budget enforcer where each org's [`BudgetLimits`] are resolved at check
/// time from an external source (the tier resolver). Used by the auth
/// middleware when `AppState.tier_resolver` is set.
///
/// Unlike [`InMemoryBudgetEnforcer`] (which uses one global limit for all
/// orgs), `DynamicBudgetEnforcer` accepts the limits as a parameter to
/// [`DynamicBudgetEnforcer::check_with_limits`] — the caller resolves them
/// (async) from the tier resolver, then passes the result in.
pub struct DynamicBudgetEnforcer {
    state: Mutex<HashMap<Uuid, OrgState>>,
}

impl DynamicBudgetEnforcer {
    /// Create a new empty enforcer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Pre-flight check: may `org_id` make a request at `now` given `limits`?
    /// Mutates internal state (rate window, request count).
    pub fn check_with_limits(
        &self,
        org_id: Uuid,
        limits: &BudgetLimits,
        now: DateTime<Utc>,
    ) -> BudgetDecision {
        let mut guard = self.state.lock().expect("dynamic budget state poisoned");
        let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
        st.roll_month(now);

        if let Some(cap) = limits.monthly_cap_usd {
            if st.spend_usd >= cap {
                return BudgetDecision::DenySpend;
            }
        }

        if let Some(mcap) = limits.monthly_request_cap {
            if st.month_request_count >= mcap {
                return BudgetDecision::DenyMonthlyRequests;
            }
        }

        if let Some(rpm) = limits.max_requests_per_min {
            let elapsed = now.signed_duration_since(st.window_start).num_seconds();
            if elapsed >= RATE_WINDOW_SECS {
                st.window_start = now;
                st.window_count = 1;
            } else if st.window_count >= rpm {
                let retry = (RATE_WINDOW_SECS - elapsed).max(0) as u64;
                return BudgetDecision::DenyRate {
                    retry_after_secs: retry,
                };
            } else {
                st.window_count += 1;
            }
        }

        st.month_request_count = st.month_request_count.saturating_add(1);

        BudgetDecision::Allow {
            spend_remaining_usd: limits
                .monthly_cap_usd
                .map(|cap| (cap - st.spend_usd).max(0.0)),
        }
    }

    /// Record realized spend after the response completes.
    pub fn record(&self, org_id: Uuid, cost_usd: f64, now: DateTime<Utc>) {
        let mut guard = self.state.lock().expect("dynamic budget state poisoned");
        let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
        st.roll_month(now);
        st.spend_usd += cost_usd.max(0.0);
    }
}

impl Default for DynamicBudgetEnforcer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn org() -> Uuid {
        Uuid::from_u128(1)
    }

    #[test]
    fn none_limits_always_allow() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits::default());
        let now = t(2026, 5, 1, 0, 0, 0);
        for _ in 0..1000 {
            assert_eq!(
                e.check(org(), now),
                BudgetDecision::Allow {
                    spend_remaining_usd: None
                }
            );
        }
    }

    #[test]
    fn allows_with_remaining_under_cap() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: Some(10.0),
            max_requests_per_min: None,
            monthly_request_cap: None,
            l2_cache: false,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        e.record(org(), 4.0, now);
        match e.check(org(), now) {
            BudgetDecision::Allow {
                spend_remaining_usd: Some(r),
            } => assert!((r - 6.0).abs() < 1e-9, "remaining = {r}"),
            other => panic!("expected Allow with remaining, got {other:?}"),
        }
    }

    #[test]
    fn denies_when_spend_cap_reached() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: Some(1.0),
            max_requests_per_min: None,
            monthly_request_cap: None,
            l2_cache: false,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        assert!(e.check(org(), now).is_allowed());
        e.record(org(), 1.5, now); // crosses the cap
        assert_eq!(e.check(org(), now), BudgetDecision::DenySpend);
    }

    #[test]
    fn rate_limit_denies_after_window_full() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: Some(2),
            monthly_request_cap: None,
            l2_cache: false,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        assert!(e.check(org(), now).is_allowed());
        assert!(e.check(org(), now).is_allowed());
        match e.check(org(), now) {
            BudgetDecision::DenyRate { retry_after_secs } => {
                assert!(retry_after_secs <= 60 && retry_after_secs > 0);
            }
            other => panic!("expected DenyRate, got {other:?}"),
        }
    }

    #[test]
    fn rate_window_resets_after_60s() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: Some(1),
            monthly_request_cap: None,
            l2_cache: false,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        assert!(e.check(org(), now).is_allowed());
        assert!(matches!(
            e.check(org(), now),
            BudgetDecision::DenyRate { .. }
        ));
        // New 60s window → allowed again.
        let later = t(2026, 5, 1, 0, 1, 1);
        assert!(e.check(org(), later).is_allowed());
    }

    #[test]
    fn monthly_spend_resets_next_month() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: Some(1.0),
            max_requests_per_min: None,
            monthly_request_cap: None,
            l2_cache: false,
        });
        let may = t(2026, 5, 31, 23, 0, 0);
        e.record(org(), 2.0, may);
        assert_eq!(e.check(org(), may), BudgetDecision::DenySpend);
        // June → accumulator resets, allowed again.
        let june = t(2026, 6, 1, 0, 0, 0);
        assert!(e.check(org(), june).is_allowed());
    }

    #[test]
    fn monthly_request_cap_denies_after_limit() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None,
            monthly_request_cap: Some(3),
            l2_cache: false,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        for _ in 0..3 {
            assert!(e.check(org(), now).is_allowed());
        }
        assert_eq!(e.check(org(), now), BudgetDecision::DenyMonthlyRequests);
    }

    #[test]
    fn monthly_request_cap_resets_next_month() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None,
            monthly_request_cap: Some(1),
            l2_cache: false,
        });
        let may = t(2026, 5, 31, 23, 0, 0);
        assert!(e.check(org(), may).is_allowed());
        assert_eq!(e.check(org(), may), BudgetDecision::DenyMonthlyRequests);
        // June → monthly counter resets, allowed again.
        let june = t(2026, 6, 1, 0, 0, 0);
        assert!(e.check(org(), june).is_allowed());
    }

    #[test]
    fn free_tier_shape_is_60_per_min_and_10000_per_month() {
        let l = BudgetLimits::free_tier();
        assert_eq!(l.max_requests_per_min, Some(60));
        assert_eq!(l.monthly_request_cap, Some(10_000));
        assert_eq!(l.monthly_cap_usd, None);
    }

    #[test]
    fn internal_unlimited_has_no_caps_and_l2_enabled() {
        // SP-0 platform-admin bypass: all caps None, L2 on. No DB required.
        let l = BudgetLimits::internal_unlimited();
        assert_eq!(l.monthly_cap_usd, None, "internal must have no USD cap");
        assert_eq!(l.max_requests_per_min, None, "internal must have no rpm cap");
        assert_eq!(l.monthly_request_cap, None, "internal must have no monthly request cap");
        assert!(l.l2_cache, "internal must have L2 cache enabled");
    }

    #[test]
    fn orgs_are_isolated() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: Some(1.0),
            max_requests_per_min: None,
            monthly_request_cap: None,
            l2_cache: false,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        e.record(a, 2.0, now); // org a over cap
        assert_eq!(e.check(a, now), BudgetDecision::DenySpend);
        assert!(e.check(b, now).is_allowed(), "org b must be unaffected");
    }

    // -----------------------------------------------------------------------
    // tier_budget_limits: mapping + effective-tier collapse
    // -----------------------------------------------------------------------

    #[test]
    fn free_tier_limits_for_free_active() {
        let l = tier_budget_limits("free", "active");
        assert_eq!(l.max_requests_per_min, Some(60));
        assert_eq!(l.monthly_request_cap, Some(10_000));
        assert_eq!(l.monthly_cap_usd, None);
        assert!(!l.l2_cache, "Free is L1-only");
    }

    #[test]
    fn pro_tier_limits() {
        let l = tier_budget_limits("pro", "active");
        assert_eq!(l.max_requests_per_min, Some(600));
        assert_eq!(l.monthly_request_cap, None, "paid tiers bill overage, no hard-stop");
        assert!(l.l2_cache, "Pro has L2 cache");
    }

    #[test]
    fn team_and_scale_have_unlimited_rpm() {
        for tier in &["team", "scale"] {
            let l = tier_budget_limits(tier, "active");
            assert_eq!(l.max_requests_per_min, None, "{tier} should have unlimited rpm");
            assert_eq!(l.monthly_request_cap, None);
            assert!(l.l2_cache);
        }
    }

    #[test]
    fn enterprise_is_not_a_paid_tier_collapses_to_free() {
        // rv-enterprise-dead-tier (§7.5): mirrors tt_api::tier::limits_for —
        // a stray tier='enterprise' must grant only Free, never Scale.
        let l = tier_budget_limits("enterprise", "active");
        assert_eq!(l.max_requests_per_min, Some(60));
        assert_eq!(l.monthly_request_cap, Some(10_000));
        assert!(!l.l2_cache, "enterprise must not get L2 (it's Free)");
    }

    #[test]
    fn unknown_tier_falls_back_to_free() {
        let l = tier_budget_limits("mystery", "active");
        assert_eq!(l.max_requests_per_min, Some(60));
        assert_eq!(l.monthly_request_cap, Some(10_000));
        assert!(!l.l2_cache);
    }

    #[test]
    fn canceled_subscription_collapses_to_free() {
        // A canceled Pro org is enforced at Free caps.
        let l = tier_budget_limits("pro", "canceled");
        assert_eq!(l.max_requests_per_min, Some(60));
        assert_eq!(l.monthly_request_cap, Some(10_000));
        assert!(!l.l2_cache);
    }

    #[test]
    fn unpaid_and_incomplete_collapse_to_free() {
        for status in &["unpaid", "incomplete", "incomplete_expired"] {
            let l = tier_budget_limits("team", status);
            assert_eq!(
                l.max_requests_per_min,
                Some(60),
                "status={status} should collapse to Free"
            );
            assert!(!l.l2_cache, "status={status} should lose L2");
        }
    }

    #[test]
    fn past_due_and_trialing_keep_paid_tier() {
        for status in &["past_due", "trialing"] {
            let l = tier_budget_limits("pro", status);
            assert_eq!(
                l.max_requests_per_min,
                Some(600),
                "status={status} should keep Pro rpm"
            );
            assert!(l.l2_cache, "status={status} should keep L2");
        }
    }

    #[test]
    fn free_org_over_monthly_cap_gets_denied() {
        // Simulate a Free org that has reached its 10 000 request cap.
        // Advance the clock by >60 s between batches so the rpm window
        // resets, avoiding a DenyRate before the monthly cap is hit.
        let limits = tier_budget_limits("free", "active");
        // Temporarily disable rpm by using InMemoryBudgetEnforcer with only
        // the monthly cap set, to test the monthly cap in isolation.
        let cap_only = BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None,
            monthly_request_cap: Some(10_000),
            l2_cache: false,
        };
        let e = InMemoryBudgetEnforcer::new(cap_only);
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = Uuid::from_u128(99);
        for _ in 0..10_000 {
            assert!(e.check(org, now).is_allowed());
        }
        assert_eq!(
            e.check(org, now),
            BudgetDecision::DenyMonthlyRequests,
            "Free org at 10k cap must be denied"
        );

        // Also verify the free_tier() limits (with rpm) hit the monthly cap
        // when the clock advances to avoid rpm conflicts.
        let e2 = InMemoryBudgetEnforcer::new(limits);
        let org2 = Uuid::from_u128(199);
        let mut total = 0u32;
        'outer: for minute in 0i64..200 {
            let ts = t(2026, 5, 1, 0, 0, 0) + chrono::Duration::seconds(minute * 61);
            for _ in 0..59u32 {
                if total >= 10_000 {
                    break 'outer;
                }
                assert!(e2.check(org2, ts).is_allowed(), "request {total} at minute {minute}");
                total += 1;
            }
        }
        assert_eq!(total, 10_000, "should have issued exactly 10_000 requests");
        let last_ts = t(2026, 5, 1, 0, 0, 0) + chrono::Duration::seconds(201 * 61);
        assert_eq!(
            e2.check(org2, last_ts),
            BudgetDecision::DenyMonthlyRequests,
            "Free org at 10k cap must be denied (with rpm enabled)"
        );
    }

    #[test]
    fn free_org_over_rpm_gets_denied() {
        let limits = tier_budget_limits("free", "active");
        let e = InMemoryBudgetEnforcer::new(limits);
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = Uuid::from_u128(100);
        for _ in 0..60 {
            assert!(e.check(org, now).is_allowed());
        }
        assert!(
            matches!(e.check(org, now), BudgetDecision::DenyRate { .. }),
            "Free org at 60 rpm must be rate-denied"
        );
    }
}
