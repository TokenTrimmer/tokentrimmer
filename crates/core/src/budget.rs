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
}

/// Outcome of a pre-flight budget check.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetDecision {
    /// Request may proceed. `spend_remaining_usd` is the headroom under the
    /// monthly cap, or `None` when spend is unlimited.
    Allow { spend_remaining_usd: Option<f64> },
    /// Monthly spend cap reached — no headroom left this month.
    DenySpend,
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
    /// `(year, month)` the spend accumulator belongs to; resets across months.
    month: (i32, u32),
    spend_usd: f64,
    /// Start of the current rate window.
    window_start: DateTime<Utc>,
    window_count: u32,
}

impl OrgState {
    fn fresh(now: DateTime<Utc>) -> Self {
        Self {
            month: (now.year(), now.month()),
            spend_usd: 0.0,
            window_start: now,
            window_count: 0,
        }
    }

    /// Reset the spend accumulator when `now` falls in a new month.
    fn roll_month(&mut self, now: DateTime<Utc>) {
        let ym = (now.year(), now.month());
        if self.month != ym {
            self.month = ym;
            self.spend_usd = 0.0;
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
        });
        let may = t(2026, 5, 31, 23, 0, 0);
        e.record(org(), 2.0, may);
        assert_eq!(e.check(org(), may), BudgetDecision::DenySpend);
        // June → accumulator resets, allowed again.
        let june = t(2026, 6, 1, 0, 0, 0);
        assert!(e.check(org(), june).is_allowed());
    }

    #[test]
    fn orgs_are_isolated() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: Some(1.0),
            max_requests_per_min: None,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        e.record(a, 2.0, now); // org a over cap
        assert_eq!(e.check(a, now), BudgetDecision::DenySpend);
        assert!(e.check(b, now).is_allowed(), "org b must be unaffected");
    }
}
