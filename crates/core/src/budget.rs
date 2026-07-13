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

/// What the gateway does when an org's month-to-date spend reaches its
/// `monthly_cap_usd` (CO-4). Persisted on `org_budget_caps.breach_policy`
/// (migration 0039); read by the tier resolver alongside the cap + folded into
/// [`BudgetLimits::breach_policy`]. The enforcement lives in the auth
/// pre-flight (`check()`) + `complete_once` (the shadow/panel branches).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BreachPolicy {
    /// Stop accepting new requests (the auth pre-flight returns `DenySpend` at
    /// `spend >= cap` → 429). Today's behavior + the default. The pricing FAQ's
    /// "set a cap and we'll stop."
    #[default]
    Block,
    /// Keep serving (no 429), but skip the spend-amplifying routes (`shadow_model`
    /// and `panel`) until the next month — a breach no longer doubles spend via
    /// the shadow dispatch. Restricts the auth pre-flight to `DenySpend` only at
    /// `spend > cap` (an exactly-at-cap request is allowed, then shadow-skipped).
    PauseShadow,
}

impl BreachPolicy {
    /// Parse the `org_budget_caps.breach_policy` string. Fail-soft → `Block`
    /// (today's behavior) on any unknown/erroring value — never crashes.
    pub fn from_db_str(s: &str) -> Self {
        match s.trim() {
            "pause_shadow" => Self::PauseShadow,
            _ => Self::Block,
        }
    }
    /// Should the pre-flight block a request whose org is EXACTLY at the cap
    /// (`spend == cap`)? `Block` → yes (`>=` denies); `PauseShadow` → no (`>`
    /// denies; the at-cap request reaches `complete_once` to be shadow-skipped).
    pub fn denies_at_cap(self) -> bool {
        matches!(self, Self::Block)
    }
    /// Should `complete_once` skip the spend-amplifying routes (shadow_model +
    /// panel) for an at-breach org? Only `PauseShadow`; `Block` orgs never reach
    /// `complete_once` at-breach (the pre-flight denied them).
    pub fn skips_shadow_at_breach(self) -> bool {
        matches!(self, Self::PauseShadow)
    }
}

/// Per-org limits. `None` on a field disables that dimension.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetLimits {
    /// Monthly spend cap in USD. `None` = unlimited spend.
    pub monthly_cap_usd: Option<f64>,
    /// Max requests per rolling 60s window. `None` = unlimited rate.
    pub max_requests_per_min: Option<u32>,
    /// Max **billed** requests per calendar month — only requests that hit the
    /// provider (non-cached) count toward this. Cache hits are excluded
    /// (P0-1): the pricing page promises cache hits do not consume included
    /// requests. `None` = unlimited monthly billed volume. Resets with the
    /// spend accumulator at the month boundary.
    pub monthly_request_cap: Option<u32>,
    /// Max **served** requests per calendar month — EVERY served request counts,
    /// cache hits included (P0-3 COGS guard). Once cache hits stop counting
    /// toward `monthly_request_cap`, a pathological high-hit-rate free tenant
    /// could otherwise serve unbounded traffic at pure infra cost; this ceiling
    /// (defaulting to ~10x `monthly_request_cap`) bounds that. `None` =
    /// unlimited served volume (paid tiers). Resets with the billed counter.
    pub monthly_served_cap: Option<u32>,
    /// Whether the L2 semantic cache is available for this tier.
    /// Free is L1-only (`false`); Pro/Team/Scale enable L2 (`true`).
    pub l2_cache: bool,
    /// CO-4: the org's budget-breach policy (migration 0039). Carries through
    /// `ResolvedTier.limits`. `Block` (the default) — the off-by-default path
    /// is byte-identical to pre-CO-4 (the field defaults + the auth-pre-flight's
    /// `>=` threshold are unchanged).
    pub breach_policy: BreachPolicy,
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
            // P0-3 COGS guard: ~10x the billed cap. A pure-cache-hit free
            // tenant bills nothing but still consumes infra; bound served
            // volume at 100 000/month.
            monthly_served_cap: Some(100_000),
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
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
            monthly_served_cap: None,
            l2_cache: true,
            breach_policy: BreachPolicy::Block,
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
            monthly_served_cap: None,  // paid tiers have no COGS ceiling
            l2_cache: true,
            breach_policy: BreachPolicy::Block,
        },
        "team" | "scale" => BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None, // unlimited rpm for team/scale
            monthly_request_cap: None,
            monthly_served_cap: None,
            l2_cache: true,
            breach_policy: BreachPolicy::Block,
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
    /// Monthly **billed** request-count cap reached — no billable (provider-hit)
    /// requests left this month. Cache hits do not contribute to this cap.
    DenyMonthlyRequests,
    /// Monthly **served** request-count cap reached (P0-3 COGS guard) — the org
    /// has served `monthly_served_cap` requests (cache hits included) this
    /// month. Distinct from [`BudgetDecision::DenyMonthlyRequests`] so the
    /// caller can return a different error reason: this is an abuse/COGS guard,
    /// not the advertised billed quota. A pure-cache-hit tenant trips this.
    DenyServed,
    /// Per-minute request rate exceeded; client should retry after the given
    /// number of seconds (when the window rolls over).
    DenyRate { retry_after_secs: u64 },
    /// Per-**key** monthly spend cap reached (P2): the specific API key has
    /// accumulated spend at or above its `api_key_budget_caps.monthly_cap_usd`
    /// this month. Distinct from [`BudgetDecision::DenySpend`] (the org-wide
    /// cap) so the caller can return a key-scoped reason — the org may still
    /// have headroom, but this individual key is capped.
    DenyKeySpend,
    /// Per-**key** monthly billed-request cap reached (P2): the specific API key
    /// has hit its `api_key_budget_caps.monthly_request_cap` this month. Like
    /// [`BudgetDecision::DenyMonthlyRequests`] but scoped to the key. Cache hits
    /// do not contribute (mirrors the billed-cap semantics).
    DenyKeyRequests,
}

/// A per-API-key spend cap, mirroring cloud `api_key_budget_caps`
/// (`key_budget_caps::KeyBudgetCap`). `None` on a dimension = that dimension is
/// uncapped *at the key level* (the org cap / tier default still applies).
///
/// This is the gateway-side replica of the cloud config row. The DB read that
/// produces it lives in [`crate::tier_resolver`]; the precedence rule against
/// the org cap is [`compose_key_cap`], the gateway mirror of cloud
/// `key_budget_caps::compose_caps` (tighter-of, min).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct KeyBudgetCap {
    /// Monthly USD spend cap for this key. `None` = no per-key spend cap.
    pub monthly_cap_usd: Option<f64>,
    /// Monthly billed-request cap for this key. `None` = no per-key request cap.
    pub monthly_request_cap: Option<u32>,
}

impl KeyBudgetCap {
    /// `true` when both dimensions are unset — the "no per-key override" row the
    /// gateway treats as absent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.monthly_cap_usd.is_none() && self.monthly_request_cap.is_none()
    }
}

/// Fold an org-level cap and a per-key cap into the single effective cap, taking
/// the **tighter (min)** of the two on each dimension. This is the gateway
/// mirror of cloud `key_budget_caps::compose_caps`: a present cap on either side
/// binds, the smaller wins when both are present, and `None` on both leaves the
/// dimension uncapped. A per-key cap therefore only ever *tightens* what the org
/// already allows — it never loosens an org cap.
///
/// The gateway does NOT actually call this to produce one merged number for the
/// org accumulator (the per-key cap is enforced against a separate per-`(org,
/// key)` accumulator); it is the documented, unit-tested precedence rule the
/// per-key cap honours. NaN-safe via `f64::min` (NaN caps are filtered upstream
/// in `tier_resolver`).
#[must_use]
pub fn compose_key_cap(org: KeyBudgetCap, key: KeyBudgetCap) -> KeyBudgetCap {
    KeyBudgetCap {
        monthly_cap_usd: tighter_f64(org.monthly_cap_usd, key.monthly_cap_usd),
        monthly_request_cap: tighter_u32(org.monthly_request_cap, key.monthly_request_cap),
    }
}

/// The tighter (smaller) of two optional USD caps. `None` = uncapped, so a
/// present value always wins over `None`.
fn tighter_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// The tighter (smaller) of two optional request caps.
fn tighter_u32(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

impl BudgetDecision {
    /// `true` only for [`BudgetDecision::Allow`].
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, BudgetDecision::Allow { .. })
    }
}

/// Pre-flight gate + post-request spend/request recorder, keyed per org.
///
/// ## Two-phase request counting (P0-1)
///
/// A request's cache-hit status is only known *after* the L1/L2/negative-cache
/// lookup, but [`BudgetEnforcer::check`] runs *before* any work (in the auth
/// middleware). So request counting is split in two:
///
/// * [`check`](BudgetEnforcer::check) does pre-flight deny/allow gating
///   (spend/billed-cap/served-cap/rate) and counts the attempt against the
///   per-minute rate window — but does **not** advance the monthly billed or
///   served counters.
/// * [`settle`](BudgetEnforcer::settle) runs once the cache-hit status is
///   known and advances the monthly counters: `served_request_count` always,
///   and `month_request_count` (billed) only when the request was **not** a
///   cache hit. This keeps cache hits off the advertised billed quota while
///   still bounding total served volume (the P0-3 COGS guard).
pub trait BudgetEnforcer: Send + Sync {
    /// Pre-flight: may `org_id` make a request at `now`? Counts the attempt
    /// against the per-minute rate window. Does **not** advance the monthly
    /// billed/served counters — that is [`settle`](BudgetEnforcer::settle)'s job
    /// (run after cache resolution).
    fn check(&self, org_id: Uuid, now: DateTime<Utc>) -> BudgetDecision;

    /// Post-resolution: record that one request was served at `now`. Advances
    /// `served_request_count` always; advances the billed `month_request_count`
    /// only when `!cached`. Called once per request after the L1/L2/negative
    /// cache resolves the `cached` status. The per-minute rate window is NOT
    /// touched here — that was already counted in [`check`](BudgetEnforcer::check).
    fn settle(&self, org_id: Uuid, cached: bool, now: DateTime<Utc>);

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
    /// **Billed** requests counted this calendar month (for
    /// `monthly_request_cap`). Advanced by `settle` only for non-cached
    /// requests — cache hits are excluded (P0-1).
    month_request_count: u32,
    /// **Served** requests counted this calendar month (for
    /// `monthly_served_cap`, the P0-3 COGS guard). Advanced by `settle` for
    /// EVERY served request, cache hits included.
    served_request_count: u32,
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
            served_request_count: 0,
            window_start: now,
            window_count: 0,
        }
    }

    /// Reset the monthly accumulators (spend + billed + served request counts)
    /// when `now` falls in a new month.
    fn roll_month(&mut self, now: DateTime<Utc>) {
        let ym = (now.year(), now.month());
        if self.month != ym {
            self.month = ym;
            self.spend_usd = 0.0;
            self.month_request_count = 0;
            self.served_request_count = 0;
        }
    }

    /// Post-resolution counter advance shared by both enforcer impls:
    /// served always, billed only when not a cache hit.
    fn settle(&mut self, cached: bool) {
        self.served_request_count = self.served_request_count.saturating_add(1);
        if !cached {
            self.month_request_count = self.month_request_count.saturating_add(1);
        }
    }
}

/// The pre-flight monthly gates shared by both enforcer impls: deny when the
/// billed cap is reached, or when the served (COGS) ceiling is reached. Returns
/// `None` when neither cap is tripped. Reads only — counters are advanced by
/// `settle`, not here.
fn monthly_caps_decision(st: &OrgState, limits: &BudgetLimits) -> Option<BudgetDecision> {
    if let Some(mcap) = limits.monthly_request_cap {
        if st.month_request_count >= mcap {
            return Some(BudgetDecision::DenyMonthlyRequests);
        }
    }
    if let Some(scap) = limits.monthly_served_cap {
        if st.served_request_count >= scap {
            return Some(BudgetDecision::DenyServed);
        }
    }
    None
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

        // Spend cap — pre-flight on accumulated spend. CO-4: the BreachPolicy
        // gates the threshold. Block denies at spend >= cap (429) — today's
        // behavior + the default. PauseShadow denies only at spend > cap so an
        // exactly-at-cap request is ALLOWED through to complete_once, which
        // then skips the shadow/panel routes (no doubled-spend dispatch).
        if let Some(cap) = self.limits.monthly_cap_usd {
            let breached = if self.limits.breach_policy.denies_at_cap() {
                st.spend_usd >= cap
            } else {
                st.spend_usd > cap
            };
            if breached {
                return BudgetDecision::DenySpend;
            }
        }

        // Monthly billed-cap + served-ceiling — pre-flight, read-only. The
        // counters advance in `settle` (after cache resolution), not here.
        if let Some(decision) = monthly_caps_decision(st, &self.limits) {
            return decision;
        }

        // Per-minute rate window (counts every attempt, cache hits included).
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

    fn settle(&self, org_id: Uuid, cached: bool, now: DateTime<Utc>) {
        let mut guard = self.state.lock().expect("budget state poisoned");
        let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
        st.roll_month(now);
        st.settle(cached);
    }

    fn record(&self, org_id: Uuid, cost_usd: f64, now: DateTime<Utc>) {
        let mut guard = self.state.lock().expect("budget state poisoned");
        let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
        st.roll_month(now);
        st.spend_usd += cost_usd.max(0.0);
    }
}

impl InMemoryBudgetEnforcer {
    /// `(billed, served)` request counts for `org_id` this month: the values
    /// `settle` advances (`month_request_count`, `served_request_count`).
    ///
    /// Introspection only — exposed (rather than `#[cfg(test)]`) so route-level
    /// integration tests can drive a real request through the handler and assert
    /// that `settle` fired with the right `cached` flag (the missing settle on
    /// the streaming cache-hit path was a route-level bug the enforcer unit
    /// tests could not catch). Not part of the enforcement contract.
    #[must_use]
    pub fn monthly_counts(&self, org_id: Uuid) -> (u32, u32) {
        let guard = self.state.lock().expect("budget state poisoned");
        guard.get(&org_id).map_or((0, 0), |st| {
            (st.month_request_count, st.served_request_count)
        })
    }
}

#[cfg(test)]
impl InMemoryBudgetEnforcer {
    /// `(billed, served)` request counts for `org_id` this month. Test-only
    /// alias of [`InMemoryBudgetEnforcer::monthly_counts`].
    fn counts_for_test(&self, org_id: Uuid) -> (u32, u32) {
        self.monthly_counts(org_id)
    }
}

/// Selects which budget enforcer realized spend is recorded into, mirroring the
/// auth pre-flight check's selection (tier-aware → dynamic_budget; else the global
/// enforcer; else nothing). This keeps the spend cap honest: the check reads the
/// same `OrgState.spend_usd` the record writes.
#[derive(Clone)]
pub enum SpendSink {
    Dynamic(std::sync::Arc<DynamicBudgetEnforcer>),
    Global(std::sync::Arc<dyn BudgetEnforcer>),
    None,
}

impl SpendSink {
    /// Record realized `cost_usd` for `(org_id, api_key_id)` (no-op for the nil
    /// org or `None` sink). The per-key spend is tracked on the dynamic path so
    /// the per-key spend cap (P2) trips against this key's own spend; a nil
    /// `api_key_id` (dogfood/anon) records org-only.
    pub fn record(&self, org_id: Uuid, api_key_id: Uuid, cost_usd: f64, now: DateTime<Utc>) {
        if org_id == Uuid::nil() {
            return;
        }
        match self {
            SpendSink::Dynamic(d) => d.record_keyed(org_id, key_opt(api_key_id), cost_usd, now),
            SpendSink::Global(g) => g.record(org_id, cost_usd, now),
            SpendSink::None => {}
        }
    }

    /// Settle one served request for `(org_id, api_key_id)` (P0-1/P0-3 + P2):
    /// advance the served counter always, and the billed monthly counter only
    /// when `!cached`, on both the org accumulator and (dynamic path) the
    /// per-key accumulator. Called once per request after cache resolution, into
    /// the same enforcer the pre-flight `check` read. No-op for the nil org or
    /// `None` sink.
    pub fn settle(&self, org_id: Uuid, api_key_id: Uuid, cached: bool, now: DateTime<Utc>) {
        if org_id == Uuid::nil() {
            return;
        }
        match self {
            SpendSink::Dynamic(d) => d.settle_keyed(org_id, key_opt(api_key_id), cached, now),
            SpendSink::Global(g) => g.settle(org_id, cached, now),
            SpendSink::None => {}
        }
    }
}

/// Treat a nil `api_key_id` (dogfood/anonymous stamp) as "no per-key dimension"
/// so the per-key accumulator is never keyed on the nil uuid.
fn key_opt(api_key_id: Uuid) -> Option<Uuid> {
    (api_key_id != Uuid::nil()).then_some(api_key_id)
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
    /// Per-ORG accumulators (spend + billed/served counts + rate window). The
    /// org cap is enforced against these.
    state: Mutex<HashMap<Uuid, OrgState>>,
    /// Per-`(org_id, api_key_id)` accumulators for the per-KEY spend/request
    /// caps (P2). Scoped to the individual key so one key's spend never trips a
    /// sibling key's cap under the same org. Only the spend + billed-request
    /// dimensions are read here (the per-key cap surface is USD + billed
    /// requests, mirroring `api_key_budget_caps`); the rate window + served
    /// ceiling remain org-scoped.
    key_state: Mutex<HashMap<(Uuid, Uuid), OrgState>>,
}

impl DynamicBudgetEnforcer {
    /// Create a new empty enforcer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            key_state: Mutex::new(HashMap::new()),
        }
    }

    /// Pre-flight check: may `org_id` make a request at `now` given `limits`?
    /// Mutates internal state (rate window, request count).
    ///
    /// Org-only convenience wrapper — equivalent to
    /// [`check_with_limits_keyed`](Self::check_with_limits_keyed) with no
    /// `api_key_id`/per-key cap. The per-key dimension is not consulted.
    pub fn check_with_limits(
        &self,
        org_id: Uuid,
        limits: &BudgetLimits,
        now: DateTime<Utc>,
    ) -> BudgetDecision {
        self.check_with_limits_keyed(org_id, None, limits, &KeyBudgetCap::default(), now)
    }

    /// Pre-flight check with a per-KEY spend cap (P2). Enforces the org cap
    /// against the per-org accumulator exactly as before, AND — when
    /// `api_key_id` is `Some` and `key_cap` is non-empty — enforces the per-key
    /// cap against a separate per-`(org, api_key)` accumulator.
    ///
    /// Precedence is "tighter-of" ([`compose_key_cap`]): the org cap and the
    /// per-key cap are two independent ceilings. A request is denied if EITHER
    /// trips. The per-key gate is evaluated first, but because the looser cap
    /// simply will not have been reached, the *tighter* ceiling is the one that
    /// surfaces — a key-scoped reason
    /// ([`BudgetDecision::DenyKeySpend`] / [`BudgetDecision::DenyKeyRequests`])
    /// when the key cap is tighter, the org reason ([`BudgetDecision::DenySpend`]
    /// / [`BudgetDecision::DenyMonthlyRequests`]) when the org cap is tighter.
    /// The rate window is advanced on the org accumulator only (rate limiting
    /// stays org-scoped).
    pub fn check_with_limits_keyed(
        &self,
        org_id: Uuid,
        api_key_id: Option<Uuid>,
        limits: &BudgetLimits,
        key_cap: &KeyBudgetCap,
        now: DateTime<Utc>,
    ) -> BudgetDecision {
        // ── Per-key gates (read-only). Evaluated before the org gates; the
        //    looser cap won't have been reached, so the tighter ceiling is what
        //    surfaces (with its scoped reason). Conservative: only consulted
        //    when a key id is present AND a per-key cap is actually set —
        //    otherwise the behaviour is identical to the org-only path.
        if let Some(key_id) = api_key_id {
            if !key_cap.is_empty() {
                let mut kguard = self.key_state.lock().expect("dynamic key state poisoned");
                let kst = kguard
                    .entry((org_id, key_id))
                    .or_insert_with(|| OrgState::fresh(now));
                kst.roll_month(now);
                if let Some(cap) = key_cap.monthly_cap_usd {
                    if kst.spend_usd >= cap {
                        return BudgetDecision::DenyKeySpend;
                    }
                }
                if let Some(mcap) = key_cap.monthly_request_cap {
                    if kst.month_request_count >= mcap {
                        return BudgetDecision::DenyKeyRequests;
                    }
                }
            }
        }

        // ── Org-wide gates (unchanged) ──────────────────────────────────────
        let mut guard = self.state.lock().expect("dynamic budget state poisoned");
        let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
        st.roll_month(now);

        if let Some(cap) = limits.monthly_cap_usd {
            // CO-4: the BreachPolicy gates the threshold. Block denies at
            // spend >= cap (429); PauseShadow denies only at spend > cap so an
            // exactly-at-cap request is ALLOWED through to complete_once,
            // which skips the shadow/panel dispatch (no doubled spend).
            let breached = if limits.breach_policy.denies_at_cap() {
                st.spend_usd >= cap
            } else {
                st.spend_usd > cap
            };
            if breached {
                return BudgetDecision::DenySpend;
            }
        }

        // Monthly billed-cap + served-ceiling — read-only; advanced in
        // `settle_with_limits` after cache resolution.
        if let Some(decision) = monthly_caps_decision(st, limits) {
            return decision;
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

        BudgetDecision::Allow {
            spend_remaining_usd: limits
                .monthly_cap_usd
                .map(|cap| (cap - st.spend_usd).max(0.0)),
        }
    }

    /// Post-resolution: advance the served counter always, and the billed
    /// counter only for non-cached requests. Mirrors
    /// [`BudgetEnforcer::settle`] for the per-org dynamic path. `limits` is
    /// accepted for signature symmetry with `check_with_limits` (counter
    /// advance does not depend on the limit values).
    ///
    /// Org-only convenience wrapper — does NOT advance any per-key accumulator.
    /// Use [`settle_keyed`](Self::settle_keyed) on the request path so the
    /// per-key billed counter tracks the per-key cap.
    pub fn settle_with_limits(
        &self,
        org_id: Uuid,
        _limits: &BudgetLimits,
        cached: bool,
        now: DateTime<Utc>,
    ) {
        self.settle_keyed(org_id, None, cached, now);
    }

    /// Post-resolution settle that advances BOTH the org accumulator and (when
    /// `api_key_id` is `Some`) the per-`(org, api_key)` accumulator with the
    /// same billed/served semantics: served always, billed only when not a
    /// cache hit. The per-key accumulator is updated unconditionally when a key
    /// id is present (no cap value needed — the counter must be ready the
    /// moment a cap is set).
    pub fn settle_keyed(
        &self,
        org_id: Uuid,
        api_key_id: Option<Uuid>,
        cached: bool,
        now: DateTime<Utc>,
    ) {
        {
            let mut guard = self.state.lock().expect("dynamic budget state poisoned");
            let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
            st.roll_month(now);
            st.settle(cached);
        }
        if let Some(key_id) = api_key_id {
            let mut kguard = self.key_state.lock().expect("dynamic key state poisoned");
            let kst = kguard
                .entry((org_id, key_id))
                .or_insert_with(|| OrgState::fresh(now));
            kst.roll_month(now);
            kst.settle(cached);
        }
    }

    /// Record realized spend after the response completes (org accumulator).
    /// Org-only convenience wrapper — does NOT advance any per-key spend.
    pub fn record(&self, org_id: Uuid, cost_usd: f64, now: DateTime<Utc>) {
        self.record_keyed(org_id, None, cost_usd, now);
    }

    /// Record realized spend into BOTH the org accumulator and (when
    /// `api_key_id` is `Some`) the per-`(org, api_key)` accumulator, so the
    /// per-key spend cap trips against this key's own realized spend. The
    /// per-key accumulator is updated unconditionally when a key id is present.
    pub fn record_keyed(
        &self,
        org_id: Uuid,
        api_key_id: Option<Uuid>,
        cost_usd: f64,
        now: DateTime<Utc>,
    ) {
        let add = cost_usd.max(0.0);
        {
            let mut guard = self.state.lock().expect("dynamic budget state poisoned");
            let st = guard.entry(org_id).or_insert_with(|| OrgState::fresh(now));
            st.roll_month(now);
            st.spend_usd += add;
        }
        if let Some(key_id) = api_key_id {
            let mut kguard = self.key_state.lock().expect("dynamic key state poisoned");
            let kst = kguard
                .entry((org_id, key_id))
                .or_insert_with(|| OrgState::fresh(now));
            kst.roll_month(now);
            kst.spend_usd += add;
        }
    }

    /// `(billed, served)` request counts for `org_id` this month: the values
    /// `settle_with_limits` advances. Introspection only (mirrors
    /// [`InMemoryBudgetEnforcer::monthly_counts`]) so route-level integration
    /// tests driving the tier-aware path can assert `settle` fired with the
    /// right `cached` flag. Not part of the enforcement contract.
    #[must_use]
    pub fn monthly_counts(&self, org_id: Uuid) -> (u32, u32) {
        let guard = self.state.lock().expect("dynamic budget state poisoned");
        guard.get(&org_id).map_or((0, 0), |st| {
            (st.month_request_count, st.served_request_count)
        })
    }

    /// `(billed, served)` request counts for `(org_id, api_key_id)` this month
    /// from the per-KEY accumulator — the values `settle_keyed`/`record_keyed`
    /// advance for the key dimension. Introspection only (mirrors
    /// [`Self::monthly_counts`]). `(0, 0)` when the key has no accumulator yet.
    #[must_use]
    pub fn key_monthly_counts(&self, org_id: Uuid, api_key_id: Uuid) -> (u32, u32) {
        let guard = self.key_state.lock().expect("dynamic key state poisoned");
        guard.get(&(org_id, api_key_id)).map_or((0, 0), |st| {
            (st.month_request_count, st.served_request_count)
        })
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

    // ── CO-4: BreachPolicy ────────────────────────────────────────────────────

    #[test]
    fn breach_policy_from_db_str_fail_softs_to_block() {
        assert_eq!(BreachPolicy::from_db_str("block"), BreachPolicy::Block);
        assert_eq!(
            BreachPolicy::from_db_str("pause_shadow"),
            BreachPolicy::PauseShadow
        );
        // Unknown/garbage → Block (today's behavior; never crashes a request).
        assert_eq!(
            BreachPolicy::from_db_str("pause_shadows"),
            BreachPolicy::Block
        );
        assert_eq!(BreachPolicy::from_db_str(""), BreachPolicy::Block);
        assert_eq!(BreachPolicy::from_db_str("DROP TABLE"), BreachPolicy::Block);
    }

    #[test]
    fn breach_policy_threshold_helpers() {
        // Block denies at spend >= cap (today's >= threshold).
        assert!(BreachPolicy::Block.denies_at_cap());
        assert!(!BreachPolicy::Block.skips_shadow_at_breach());
        // PauseShadow denies only at spend > cap (exactly-at-cap allowed through
        // to complete_once for the shadow-skip).
        assert!(!BreachPolicy::PauseShadow.denies_at_cap());
        assert!(BreachPolicy::PauseShadow.skips_shadow_at_breach());
    }

    #[test]
    fn pause_shadow_allows_at_cap_but_blocks_over_cap() {
        // The check() threshold: Block denies at >= , PauseShadow at >.
        let now = Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap();
        let org = Uuid::nil();

        let block = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: Some(10.0),
            monthly_request_cap: None,
            monthly_served_cap: None,
            max_requests_per_min: None,
            l2_cache: true,
            breach_policy: BreachPolicy::Block,
        });
        // Record exactly-cap spend (the enforcer's own state, not a throwaway).
        block.record(org, 10.0, now);
        // Block: spend == cap → DenySpend (429).
        assert!(matches!(block.check(org, now), BudgetDecision::DenySpend));

        let pause = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: Some(10.0),
            monthly_request_cap: None,
            monthly_served_cap: None,
            max_requests_per_min: None,
            l2_cache: true,
            breach_policy: BreachPolicy::PauseShadow,
        });
        pause.record(org, 10.0, now);
        // PauseShadow: spend == cap → Allow (reaches complete_once for the shadow-skip).
        assert!(matches!(
            pause.check(org, now),
            BudgetDecision::Allow { .. }
        ));
        // PauseShadow: spend > cap → DenySpend (over-cap still blocked).
        pause.record(org, 0.01, now); // 10.01 total
        assert!(matches!(pause.check(org, now), BudgetDecision::DenySpend));
    }

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
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
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
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
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
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
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
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
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
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
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
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        // P0-1: the billed counter advances on settle(cached=false), not check.
        for _ in 0..3 {
            assert!(e.check(org(), now).is_allowed());
            e.settle(org(), false, now);
        }
        assert_eq!(e.check(org(), now), BudgetDecision::DenyMonthlyRequests);
    }

    #[test]
    fn monthly_request_cap_resets_next_month() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None,
            monthly_request_cap: Some(1),
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        });
        let may = t(2026, 5, 31, 23, 0, 0);
        // P0-1: settle (not check) advances the billed counter.
        assert!(e.check(org(), may).is_allowed());
        e.settle(org(), false, may);
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
        assert_eq!(
            l.max_requests_per_min, None,
            "internal must have no rpm cap"
        );
        assert_eq!(
            l.monthly_request_cap, None,
            "internal must have no monthly request cap"
        );
        assert!(l.l2_cache, "internal must have L2 cache enabled");
    }

    #[test]
    fn orgs_are_isolated() {
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: Some(1.0),
            max_requests_per_min: None,
            monthly_request_cap: None,
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
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
        assert_eq!(
            l.monthly_request_cap, None,
            "paid tiers bill overage, no hard-stop"
        );
        assert!(l.l2_cache, "Pro has L2 cache");
    }

    #[test]
    fn team_and_scale_have_unlimited_rpm() {
        for tier in &["team", "scale"] {
            let l = tier_budget_limits(tier, "active");
            assert_eq!(
                l.max_requests_per_min, None,
                "{tier} should have unlimited rpm"
            );
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
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        };
        let e = InMemoryBudgetEnforcer::new(cap_only);
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = Uuid::from_u128(99);
        // P0-1: the billed counter advances on settle(cached=false), so each
        // billable request is check()+settle(false).
        for _ in 0..10_000 {
            assert!(e.check(org, now).is_allowed());
            e.settle(org, false, now);
        }
        assert_eq!(
            e.check(org, now),
            BudgetDecision::DenyMonthlyRequests,
            "Free org at 10k cap must be denied"
        );

        // Also verify the free_tier() limits (with rpm) hit the monthly cap
        // when the clock advances to avoid rpm conflicts. Each billable request
        // is check()+settle(false); the 100k served cap is far above 10k so it
        // never fires here.
        let e2 = InMemoryBudgetEnforcer::new(limits);
        let org2 = Uuid::from_u128(199);
        let mut total = 0u32;
        'outer: for minute in 0i64..200 {
            let ts = t(2026, 5, 1, 0, 0, 0) + chrono::Duration::seconds(minute * 61);
            for _ in 0..59u32 {
                if total >= 10_000 {
                    break 'outer;
                }
                assert!(
                    e2.check(org2, ts).is_allowed(),
                    "request {total} at minute {minute}"
                );
                e2.settle(org2, false, ts);
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

    // -----------------------------------------------------------------------
    // SpendSink: record→check land on the same state
    // -----------------------------------------------------------------------

    #[test]
    fn spend_sink_dynamic_records_make_cap_trip() {
        // Build a DynamicBudgetEnforcer, wrap in SpendSink::Dynamic, record
        // enough spend to exceed a $1 monthly cap, then assert check_with_limits
        // returns DenySpend. Proves record→check share the same OrgState.
        let enforcer = std::sync::Arc::new(DynamicBudgetEnforcer::new());
        let sink = SpendSink::Dynamic(enforcer.clone());
        let org = Uuid::from_u128(42);
        let limits = BudgetLimits {
            monthly_cap_usd: Some(1.0),
            max_requests_per_min: None,
            monthly_request_cap: None,
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        };
        let now = t(2026, 6, 1, 0, 0, 0);

        // Before recording: allowed.
        assert!(
            enforcer.check_with_limits(org, &limits, now).is_allowed(),
            "should be allowed before any spend"
        );

        // Record spend that exceeds the cap.
        sink.record(org, Uuid::nil(), 1.5, now);

        // After recording: check on the same enforcer must deny.
        assert_eq!(
            enforcer.check_with_limits(org, &limits, now),
            BudgetDecision::DenySpend,
            "should deny after spend exceeds monthly cap"
        );
    }

    #[test]
    fn spend_sink_nil_org_is_noop() {
        // SpendSink::Dynamic with a nil org_id must not record anything —
        // a real org's check_with_limits must still Allow afterward.
        let enforcer = std::sync::Arc::new(DynamicBudgetEnforcer::new());
        let sink = SpendSink::Dynamic(enforcer.clone());
        let limits = BudgetLimits {
            monthly_cap_usd: Some(1.0),
            max_requests_per_min: None,
            monthly_request_cap: None,
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        };
        let now = t(2026, 6, 1, 0, 0, 0);
        let real_org = Uuid::from_u128(99);

        // Record against nil — should be a no-op.
        sink.record(Uuid::nil(), Uuid::nil(), 5.0, now);

        // A real org with no spend should still be allowed (nil spend not recorded).
        assert!(
            enforcer
                .check_with_limits(real_org, &limits, now)
                .is_allowed(),
            "nil org record must not affect real org"
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

    // -----------------------------------------------------------------------
    // P0-1 / P0-3: two-phase billed/served counting (settle excludes cache
    // hits from the billed monthly cap but counts every served request).
    // -----------------------------------------------------------------------

    /// `monthly_request_cap` + a `monthly_served_cap` at 10x, no rpm/spend caps,
    /// so the billed and served counters can be exercised in isolation.
    fn billed_and_served(billed: u32, served: u32) -> BudgetLimits {
        BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None,
            monthly_request_cap: Some(billed),
            monthly_served_cap: Some(served),
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        }
    }

    #[test]
    fn cache_hit_does_not_advance_billed_but_advances_served() {
        // A request that passes check() and then settles as a cache hit must
        // NOT consume a billed monthly slot, but MUST count as served.
        let e = InMemoryBudgetEnforcer::new(billed_and_served(2, 100));
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = org();

        // Three cache hits in a row: check passes, settle(cached=true).
        for _ in 0..3 {
            assert!(e.check(org, now).is_allowed());
            e.settle(org, true, now);
        }
        // Billed count never advanced → still under the billed cap of 2 even
        // after 3 served requests. A non-cached request must still be allowed
        // and consume the FIRST billed slot.
        assert!(
            e.check(org, now).is_allowed(),
            "billed cap must be untouched by cache hits"
        );
        e.settle(org, false, now);
        // One billed request used; cap is 2, so a second non-cached request OK.
        assert!(e.check(org, now).is_allowed());
        e.settle(org, false, now);
        // Now 2 billed used = cap → next non-cached must be denied.
        assert_eq!(e.check(org, now), BudgetDecision::DenyMonthlyRequests);
    }

    #[test]
    fn non_cached_request_advances_both_counters() {
        let e = InMemoryBudgetEnforcer::new(billed_and_served(5, 50));
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = org();
        assert!(e.check(org, now).is_allowed());
        e.settle(org, false, now);
        let (billed, served) = e.counts_for_test(org);
        assert_eq!(billed, 1, "non-cached must advance billed count");
        assert_eq!(served, 1, "non-cached must advance served count");
    }

    #[test]
    fn billed_cap_still_denies_at_cap_on_non_cached_traffic() {
        let e = InMemoryBudgetEnforcer::new(billed_and_served(3, 1000));
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = org();
        for _ in 0..3 {
            assert!(e.check(org, now).is_allowed());
            e.settle(org, false, now);
        }
        assert_eq!(
            e.check(org, now),
            BudgetDecision::DenyMonthlyRequests,
            "non-cached traffic must still hit the billed cap"
        );
    }

    #[test]
    fn served_ceiling_denies_pure_cache_hit_tenant_at_10x() {
        // A pure-cache-hit tenant: billed never advances, but served does.
        // The served cap (10x the billed cap) must eventually deny.
        let e = InMemoryBudgetEnforcer::new(billed_and_served(3, 30));
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = org();
        for i in 0..30 {
            assert!(
                e.check(org, now).is_allowed(),
                "served request {i} under the 30 ceiling must pass"
            );
            e.settle(org, true, now); // all cache hits
        }
        // 30 served = served cap → next request denied by the COGS guard, even
        // though the billed count is still 0.
        assert_eq!(
            e.check(org, now),
            BudgetDecision::DenyServed,
            "pure-cache-hit tenant must be denied at the served ceiling"
        );
        let (billed, served) = e.counts_for_test(org);
        assert_eq!(billed, 0, "billed must remain 0 for a pure-hit tenant");
        assert_eq!(served, 30, "served must have reached the ceiling");
    }

    #[test]
    fn free_tier_served_cap_is_10x_billed_cap() {
        let l = BudgetLimits::free_tier();
        assert_eq!(l.monthly_request_cap, Some(10_000));
        assert_eq!(
            l.monthly_served_cap,
            Some(100_000),
            "free served cap should default to ~10x the billed cap"
        );
    }

    #[test]
    fn rate_limit_counts_every_attempt_including_hits() {
        // The per-minute rate window must count EVERY attempt — cache hits
        // included — independent of the billed monthly exclusion. check()
        // counts the attempt; settle does not touch the rate window.
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: Some(2),
            monthly_request_cap: Some(10_000),
            monthly_served_cap: Some(100_000),
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = org();
        // Two attempts that settle as cache hits still fill the rate window.
        assert!(e.check(org, now).is_allowed());
        e.settle(org, true, now);
        assert!(e.check(org, now).is_allowed());
        e.settle(org, true, now);
        // Third attempt — window full → rate-denied even though the first two
        // were cache hits that did not bill.
        assert!(
            matches!(e.check(org, now), BudgetDecision::DenyRate { .. }),
            "rate window must count cache-hit attempts"
        );
    }

    #[test]
    fn month_rollover_resets_both_counters() {
        let e = InMemoryBudgetEnforcer::new(billed_and_served(2, 4));
        let may = t(2026, 5, 31, 23, 0, 0);
        let org = org();
        // Exhaust both: 2 billed (non-cached) + 2 served (cache hits) = 4 served.
        e.check(org, may);
        e.settle(org, false, may);
        e.check(org, may);
        e.settle(org, false, may);
        e.check(org, may);
        e.settle(org, true, may);
        e.check(org, may);
        e.settle(org, true, may);
        let (billed, served) = e.counts_for_test(org);
        assert_eq!((billed, served), (2, 4), "both counters accumulated in May");
        // June → both reset.
        let june = t(2026, 6, 1, 0, 0, 0);
        assert!(
            e.check(org, june).is_allowed(),
            "billed cap must reset in June"
        );
        let (billed, served) = e.counts_for_test(org);
        assert_eq!(billed, 0, "billed count must reset at month boundary");
        assert_eq!(served, 0, "served count must reset at month boundary");
    }

    #[test]
    fn dynamic_settle_with_limits_mirrors_in_memory() {
        // The DynamicBudgetEnforcer path must apply the same billed/served
        // semantics via settle_with_limits.
        let e = DynamicBudgetEnforcer::new();
        let limits = billed_and_served(2, 6);
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = org();
        // 6 cache hits → served cap reached, billed untouched.
        for _ in 0..6 {
            assert!(e.check_with_limits(org, &limits, now).is_allowed());
            e.settle_with_limits(org, &limits, true, now);
        }
        assert_eq!(
            e.check_with_limits(org, &limits, now),
            BudgetDecision::DenyServed,
            "dynamic served ceiling must deny a pure-hit tenant"
        );
    }

    #[test]
    fn served_cap_none_disables_the_cogs_guard() {
        // Paid tiers carry no served cap (monthly_served_cap = None) → unbounded
        // served volume, never DenyServed.
        let e = InMemoryBudgetEnforcer::new(BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None,
            monthly_request_cap: None,
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        });
        let now = t(2026, 5, 1, 0, 0, 0);
        let org = org();
        for _ in 0..1000 {
            assert!(e.check(org, now).is_allowed());
            e.settle(org, true, now);
        }
    }

    // -----------------------------------------------------------------------
    // P2: per-API-key spend/request cap enforcement (compose_key_cap +
    // DynamicBudgetEnforcer::check_with_limits_keyed / record_keyed /
    // settle_keyed). The per-key cap binds against a per-(org, key)
    // accumulator, tighter-of-org-and-key (min), scoped to the key.
    // -----------------------------------------------------------------------

    fn key_cap(usd: Option<f64>, req: Option<u32>) -> KeyBudgetCap {
        KeyBudgetCap {
            monthly_cap_usd: usd,
            monthly_request_cap: req,
        }
    }

    // ── compose_key_cap: tighter-of (mirror of cloud compose_caps) ──────────

    #[test]
    fn compose_key_cap_tightens_an_org_cap() {
        // Org allows $100; the key is capped at $25 → effective $25.
        let c = compose_key_cap(key_cap(Some(100.0), None), key_cap(Some(25.0), None));
        assert_eq!(c.monthly_cap_usd, Some(25.0));
    }

    #[test]
    fn compose_key_cap_never_loosens_an_org_cap() {
        // A *larger* per-key value must NOT raise the org's $25 ceiling.
        let c = compose_key_cap(key_cap(Some(25.0), None), key_cap(Some(100.0), None));
        assert_eq!(
            c.monthly_cap_usd,
            Some(25.0),
            "the tighter org cap must win — no double-spend"
        );
    }

    #[test]
    fn compose_key_cap_binds_where_org_has_none() {
        // Paid org has no spend cap; a per-key cap sets the hard stop.
        let c = compose_key_cap(key_cap(None, None), key_cap(Some(40.0), None));
        assert_eq!(c.monthly_cap_usd, Some(40.0));
        // Request caps compose the same way.
        let cr = compose_key_cap(key_cap(None, None), key_cap(None, Some(500)));
        assert_eq!(cr.monthly_request_cap, Some(500));
    }

    #[test]
    fn compose_key_cap_no_caps_is_uncapped() {
        let c = compose_key_cap(KeyBudgetCap::default(), KeyBudgetCap::default());
        assert_eq!(c.monthly_cap_usd, None);
        assert_eq!(c.monthly_request_cap, None);
        assert!(KeyBudgetCap::default().is_empty());
    }

    #[test]
    fn compose_key_cap_request_caps_only_tighten() {
        assert_eq!(
            compose_key_cap(key_cap(None, Some(10_000)), key_cap(None, Some(2_000)))
                .monthly_request_cap,
            Some(2_000)
        );
        assert_eq!(
            compose_key_cap(key_cap(None, Some(2_000)), key_cap(None, Some(10_000)))
                .monthly_request_cap,
            Some(2_000),
            "key request cap must not loosen the org request cap"
        );
    }

    // ── check_with_limits_keyed: enforcement ────────────────────────────────

    /// An org with no USD cap (paid-tier shape) — so the per-key cap is the
    /// only spend ceiling under test.
    fn uncapped_org() -> BudgetLimits {
        BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: None,
            monthly_request_cap: None,
            monthly_served_cap: None,
            l2_cache: false,
            breach_policy: BreachPolicy::Block,
        }
    }

    #[test]
    fn per_key_spend_cap_tighter_than_org_denies_at_key_cap() {
        // Org cap $100, key cap $10. Spend $12 on the key → key cap trips first
        // even though the org still has $88 of headroom.
        let e = DynamicBudgetEnforcer::new();
        let org = Uuid::from_u128(1);
        let key = Uuid::from_u128(7);
        let limits = BudgetLimits {
            monthly_cap_usd: Some(100.0),
            ..uncapped_org()
        };
        let kcap = key_cap(Some(10.0), None);
        let now = t(2026, 6, 1, 0, 0, 0);

        assert!(e
            .check_with_limits_keyed(org, Some(key), &limits, &kcap, now)
            .is_allowed());
        e.record_keyed(org, Some(key), 12.0, now);
        assert_eq!(
            e.check_with_limits_keyed(org, Some(key), &limits, &kcap, now),
            BudgetDecision::DenyKeySpend,
            "the tighter per-key cap must deny while the org still has headroom"
        );
    }

    #[test]
    fn per_key_spend_cap_looser_than_org_is_bounded_by_org() {
        // Org cap $10, key cap $100 (looser). The ORG cap binds — a key cap
        // never loosens the org ceiling.
        let e = DynamicBudgetEnforcer::new();
        let org = Uuid::from_u128(2);
        let key = Uuid::from_u128(8);
        let limits = BudgetLimits {
            monthly_cap_usd: Some(10.0),
            ..uncapped_org()
        };
        let kcap = key_cap(Some(100.0), None);
        let now = t(2026, 6, 1, 0, 0, 0);

        assert!(e
            .check_with_limits_keyed(org, Some(key), &limits, &kcap, now)
            .is_allowed());
        // Spend $12 — over the $10 org cap but under the $100 key cap.
        e.record_keyed(org, Some(key), 12.0, now);
        assert_eq!(
            e.check_with_limits_keyed(org, Some(key), &limits, &kcap, now),
            BudgetDecision::DenySpend,
            "the org cap is the tighter ceiling — org reason, not key reason"
        );
    }

    #[test]
    fn no_per_key_cap_is_org_cap_only_unchanged() {
        // An empty key cap (or no key id) must behave exactly like the org-only
        // path: only the org cap gates.
        let e = DynamicBudgetEnforcer::new();
        let org = Uuid::from_u128(3);
        let key = Uuid::from_u128(9);
        let limits = BudgetLimits {
            monthly_cap_usd: Some(5.0),
            ..uncapped_org()
        };
        let now = t(2026, 6, 1, 0, 0, 0);

        // Empty per-key cap: allowed until org spend trips.
        assert!(e
            .check_with_limits_keyed(org, Some(key), &limits, &KeyBudgetCap::default(), now)
            .is_allowed());
        e.record_keyed(org, Some(key), 6.0, now);
        assert_eq!(
            e.check_with_limits_keyed(org, Some(key), &limits, &KeyBudgetCap::default(), now),
            BudgetDecision::DenySpend,
            "empty key cap → org cap only"
        );
        // And the plain org-only wrapper agrees.
        assert_eq!(
            e.check_with_limits(org, &limits, now),
            BudgetDecision::DenySpend
        );
    }

    #[test]
    fn per_key_counter_is_scoped_to_the_key() {
        // Two keys under the same org, each with a $10 key cap. Spending on key A
        // must NOT push key B over its cap. The org itself is uncapped.
        let e = DynamicBudgetEnforcer::new();
        let org = Uuid::from_u128(4);
        let key_a = Uuid::from_u128(10);
        let key_b = Uuid::from_u128(11);
        let limits = uncapped_org();
        let kcap = key_cap(Some(10.0), None);
        let now = t(2026, 6, 1, 0, 0, 0);

        // Drive key A over its cap.
        e.record_keyed(org, Some(key_a), 12.0, now);
        assert_eq!(
            e.check_with_limits_keyed(org, Some(key_a), &limits, &kcap, now),
            BudgetDecision::DenyKeySpend,
            "key A is over its own cap"
        );
        // Key B, same org, untouched → still allowed.
        assert!(
            e.check_with_limits_keyed(org, Some(key_b), &limits, &kcap, now)
                .is_allowed(),
            "key B must be unaffected by key A's spend"
        );
    }

    #[test]
    fn per_key_request_cap_denies_after_billed_limit() {
        // Per-key billed-request cap of 3, no org request cap. Cache hits do not
        // advance the per-key billed counter (mirrors the org billed semantics).
        let e = DynamicBudgetEnforcer::new();
        let org = Uuid::from_u128(5);
        let key = Uuid::from_u128(12);
        let limits = uncapped_org();
        let kcap = key_cap(None, Some(3));
        let now = t(2026, 6, 1, 0, 0, 0);

        for _ in 0..3 {
            assert!(e
                .check_with_limits_keyed(org, Some(key), &limits, &kcap, now)
                .is_allowed());
            e.settle_keyed(org, Some(key), false, now);
        }
        assert_eq!(
            e.check_with_limits_keyed(org, Some(key), &limits, &kcap, now),
            BudgetDecision::DenyKeyRequests,
            "per-key billed cap must deny after the limit"
        );

        // A fresh key with a cache-hit-only history never advances the billed
        // counter → never trips DenyKeyRequests.
        let key2 = Uuid::from_u128(13);
        for _ in 0..10 {
            assert!(e
                .check_with_limits_keyed(org, Some(key2), &limits, &kcap, now)
                .is_allowed());
            e.settle_keyed(org, Some(key2), true, now); // cache hits
        }
        let (billed, served) = e.key_monthly_counts(org, key2);
        assert_eq!(
            billed, 0,
            "cache hits must not advance per-key billed count"
        );
        assert_eq!(served, 10, "served counter still advances per key");
    }

    #[test]
    fn per_key_cap_resets_next_month() {
        let e = DynamicBudgetEnforcer::new();
        let org = Uuid::from_u128(6);
        let key = Uuid::from_u128(14);
        let limits = uncapped_org();
        let kcap = key_cap(Some(10.0), None);
        let may = t(2026, 5, 31, 23, 0, 0);
        e.record_keyed(org, Some(key), 12.0, may);
        assert_eq!(
            e.check_with_limits_keyed(org, Some(key), &limits, &kcap, may),
            BudgetDecision::DenyKeySpend
        );
        // June → the per-key accumulator resets, allowed again.
        let june = t(2026, 6, 1, 0, 0, 0);
        assert!(e
            .check_with_limits_keyed(org, Some(key), &limits, &kcap, june)
            .is_allowed());
    }

    #[test]
    fn spend_sink_threads_key_id_into_per_key_accumulator() {
        // SpendSink::record/settle must forward api_key_id so the per-key
        // accumulator advances; a nil api_key_id records org-only.
        let enforcer = std::sync::Arc::new(DynamicBudgetEnforcer::new());
        let sink = SpendSink::Dynamic(enforcer.clone());
        let org = Uuid::from_u128(20);
        let key = Uuid::from_u128(21);
        let limits = uncapped_org();
        let kcap = key_cap(Some(10.0), None);
        let now = t(2026, 6, 1, 0, 0, 0);

        sink.record(org, key, 12.0, now);
        assert_eq!(
            enforcer.check_with_limits_keyed(org, Some(key), &limits, &kcap, now),
            BudgetDecision::DenyKeySpend,
            "SpendSink::record must populate the per-key accumulator"
        );
        // settle threads through too.
        sink.settle(org, key, false, now);
        let (billed, _served) = enforcer.key_monthly_counts(org, key);
        assert_eq!(
            billed, 1,
            "SpendSink::settle must advance the per-key billed count"
        );

        // A nil key id records org-only — no per-key accumulator created.
        let org2 = Uuid::from_u128(22);
        sink.record(org2, Uuid::nil(), 5.0, now);
        assert_eq!(
            enforcer.key_monthly_counts(org2, Uuid::nil()),
            (0, 0),
            "nil api_key_id must not create a per-key accumulator"
        );
    }
}
