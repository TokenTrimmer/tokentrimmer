//! Pre-auth per-IP rate cap in front of the argon2 key-verify path.
//!
//! ## Why this exists
//!
//! The gateway verifies `tt_live_*` API keys with **argon2** (deliberately
//! ~100 ms/call) *before* authentication completes. The verify cache
//! ([`crate::middleware::key_cache`]) short-circuits *repeated identical*
//! tokens, but an unauthenticated attacker who rotates the 16-bit suffix of a
//! known live prefix presents a fresh token every request — each one misses
//! the cache and forces a full argon2 hash. That is a CPU-DoS amplification
//! vector: a trickle of cheap HTTP requests pins gateway CPU. The surfaced key
//! prefix makes it worse (the attacker doesn't even have to guess the prefix).
//!
//! This module adds a **per-source-IP rate cap that is checked BEFORE the
//! argon2 work**. Past the threshold, key-verification attempts from one IP are
//! shed with `429 Too Many Requests` + `Retry-After` and the expensive verify
//! is never invoked. The cap is consulted by [`crate::middleware::auth`] **only
//! on the cold path** — a cache *hit* (already-verified token) and any
//! non-`tt_live_*` traffic bypass it entirely, so legitimate authenticated
//! traffic is never throttled by this path.
//!
//! Backed by a [`governor`] GCRA keyed limiter (the same crate the cloud
//! `tt-api` abuse-control limiter uses), keyed by client IP.
//!
//! ## Caveat: in-memory, per-instance
//!
//! The limiter state lives in this process's memory. With more than one gateway
//! instance behind a load balancer the effective cap is
//! `N_instances × threshold`, and a restart resets the counters. That is an
//! accepted trade-off (it still removes the amplification on each instance).
//! **Move the limiter state to a shared store (Redis) before scaling
//! horizontally** so the cap is global across instances — mirroring the same
//! "move to Redis before scaling" caveat noted on the cloud side.

use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU32;
use std::sync::Arc;

use axum::http::HeaderMap;
use governor::{
    clock::{Clock, DefaultClock},
    middleware::NoOpMiddleware,
    state::keyed::DefaultKeyedStateStore,
    Quota, RateLimiter,
};

/// A keyed GCRA limiter over client IPs, generic over the clock `C`. The no-op
/// accounting middleware is pinned to the clock's instant type so a
/// `FakeRelativeClock` (whose instant differs from the default quanta clock)
/// type-checks in tests.
type IpRateLimiter<C> =
    RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, C, NoOpMiddleware<<C as Clock>::Instant>>;

/// Shared, `Arc`-friendly cap state used by [`AppState`](crate::AppState). The
/// production type pins the default quanta clock; tests construct it over a
/// `FakeRelativeClock` for determinism.
pub type Argon2Cap = Arc<Argon2VerifyCap<DefaultClock>>;

/// Default per-IP key-verification attempts allowed per minute before the cap
/// sheds with 429. Sized so an ordinary client re-authenticating after a cache
/// expiry (verify cache positive TTL is 60 s) never trips it, but a flood of
/// distinct bogus suffixes is bounded to a few argon2 calls/sec per IP.
pub const DEFAULT_VERIFY_PER_MIN: u32 = 60;

/// Tunable cap, overridable via the `TT_ARGON2_CAP_*` env vars. The default is
/// generous for real traffic but low enough to defang a flood.
#[derive(Clone, Copy, Debug)]
pub struct Argon2CapConfig {
    /// Per-IP cold-path key-verify attempts per minute. Default
    /// [`DEFAULT_VERIFY_PER_MIN`].
    pub verify_per_min: u32,
}

impl Default for Argon2CapConfig {
    fn default() -> Self {
        Self {
            verify_per_min: DEFAULT_VERIFY_PER_MIN,
        }
    }
}

impl Argon2CapConfig {
    /// Read overrides from the environment. An unset or unparsable value keeps
    /// the default; `0` is clamped to `1` (a GCRA quota must be non-zero).
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            verify_per_min: env_u32("TT_ARGON2_CAP_VERIFY_PER_MIN", d.verify_per_min),
        }
    }
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|v| v.max(1))
        .unwrap_or(default)
}

/// Per-minute quota allowing `per_min` cells, replenishing smoothly, with a
/// burst capacity equal to the full per-minute allowance. A `0` input is clamped
/// to `1` so construction can't fail.
fn per_minute_quota(per_min: u32) -> Quota {
    let n = NonZeroU32::new(per_min.max(1)).expect("clamped to >= 1");
    Quota::per_minute(n)
}

/// Outcome of consulting the cap for one cold-path verify attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapDecision {
    /// Under the cap — proceed to the argon2 verify.
    Allow,
    /// Over the cap — shed with 429; `retry_after_secs` is the seconds until the
    /// limiter would next admit a cell for this IP (always `>= 1`).
    Reject { retry_after_secs: u64 },
}

/// The per-IP argon2-verify cap. Generic over the clock so tests can drive a
/// [`governor::clock::FakeRelativeClock`] deterministically while production
/// uses the default quanta clock.
pub struct Argon2VerifyCap<C = DefaultClock>
where
    C: Clock,
{
    limiter: IpRateLimiter<C>,
    clock: C,
}

impl Argon2VerifyCap<DefaultClock> {
    /// Production constructor — quanta clock, limit from [`Argon2CapConfig`].
    #[must_use]
    pub fn new(cfg: Argon2CapConfig) -> Arc<Self> {
        Self::with_clock(cfg, DefaultClock::default())
    }

    /// Production constructor reading the limit from the environment.
    #[must_use]
    pub fn from_env() -> Arc<Self> {
        Self::new(Argon2CapConfig::from_env())
    }
}

impl<C> Argon2VerifyCap<C>
where
    C: Clock + Clone,
{
    /// Construct with an explicit clock + config — used by tests to inject a
    /// `FakeRelativeClock`.
    #[must_use]
    pub fn with_clock(cfg: Argon2CapConfig, clock: C) -> Arc<Self> {
        Arc::new(Self {
            limiter: RateLimiter::dashmap_with_clock(
                per_minute_quota(cfg.verify_per_min),
                clock.clone(),
            ),
            clock,
        })
    }

    /// Consult the cap for one cold-path verify attempt from `ip`. Charges a
    /// cell on [`CapDecision::Allow`]; reports a `Retry-After` on reject.
    ///
    /// This is the **only** place the cap is consumed, and the caller invokes it
    /// strictly before the argon2 work, so a rejected attempt never reaches the
    /// hash.
    #[must_use]
    pub fn check(&self, ip: IpAddr) -> CapDecision {
        match self.limiter.check_key(&ip) {
            Ok(()) => CapDecision::Allow,
            Err(not_until) => {
                let retry_after_secs = not_until.wait_time_from(self.clock.now()).as_secs().max(1);
                CapDecision::Reject { retry_after_secs }
            }
        }
    }
}

/// Extract the client IP for keying. Behind Cloudflare + Fly the socket peer is
/// the edge, so prefer `cf-connecting-ip`, then the first hop of
/// `x-forwarded-for`, then a stable fallback. A spoofable header is acceptable
/// here: this is abuse-control, not authn. An attacker rotating the header to
/// dodge the per-IP cap still pays argon2 only on distinct cells, and every
/// distinct spoofed IP is bounded independently; a header-less direct flood
/// collapses onto the single fallback bucket and is bounded there.
#[must_use]
pub fn client_ip(headers: &HeaderMap) -> IpAddr {
    if let Some(ip) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    if let Some(ip) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    // No edge header (direct hit / local test): a single shared bucket. Still
    // bounded by the cap, just not per-distinct-client.
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use governor::clock::FakeRelativeClock;
    use std::time::Duration;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn default_is_positive() {
        assert!(Argon2CapConfig::default().verify_per_min >= 1);
    }

    #[test]
    fn env_u32_clamps_zero_and_falls_back() {
        std::env::remove_var("TT_ARGON2_CAP_TEST_UNSET");
        assert_eq!(env_u32("TT_ARGON2_CAP_TEST_UNSET", 42), 42);
        std::env::set_var("TT_ARGON2_CAP_TEST_ZERO", "0");
        assert_eq!(env_u32("TT_ARGON2_CAP_TEST_ZERO", 42), 1);
        std::env::remove_var("TT_ARGON2_CAP_TEST_ZERO");
    }

    #[test]
    fn allows_up_to_threshold_then_rejects() {
        let clock = FakeRelativeClock::default();
        let cap = Argon2VerifyCap::with_clock(Argon2CapConfig { verify_per_min: 3 }, clock.clone());
        let a = ip("1.2.3.4");
        // Burst == per_min: first 3 allowed.
        assert_eq!(cap.check(a), CapDecision::Allow);
        assert_eq!(cap.check(a), CapDecision::Allow);
        assert_eq!(cap.check(a), CapDecision::Allow);
        // 4th rejected with a Retry-After.
        match cap.check(a) {
            CapDecision::Reject { retry_after_secs } => assert!(retry_after_secs >= 1),
            CapDecision::Allow => panic!("4th attempt should be rejected"),
        }
    }

    #[test]
    fn distinct_ips_are_independent() {
        let clock = FakeRelativeClock::default();
        let cap = Argon2VerifyCap::with_clock(Argon2CapConfig { verify_per_min: 1 }, clock.clone());
        // a exhausts its single cell...
        assert_eq!(cap.check(ip("1.1.1.1")), CapDecision::Allow);
        assert!(matches!(
            cap.check(ip("1.1.1.1")),
            CapDecision::Reject { .. }
        ));
        // ...but a different IP still has its own full budget.
        assert_eq!(cap.check(ip("2.2.2.2")), CapDecision::Allow);
    }

    #[test]
    fn bucket_refills_after_a_minute() {
        let clock = FakeRelativeClock::default();
        let cap = Argon2VerifyCap::with_clock(Argon2CapConfig { verify_per_min: 1 }, clock.clone());
        let a = ip("3.3.3.3");
        assert_eq!(cap.check(a), CapDecision::Allow);
        assert!(matches!(cap.check(a), CapDecision::Reject { .. }));
        clock.advance(Duration::from_secs(61));
        assert_eq!(cap.check(a), CapDecision::Allow, "bucket should refill");
    }

    #[test]
    fn client_ip_prefers_cf_then_xff_then_fallback() {
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(&h), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        h.insert(
            "x-forwarded-for",
            "203.0.113.7, 70.41.3.18".parse().unwrap(),
        );
        assert_eq!(client_ip(&h), ip("203.0.113.7"));
        h.insert("cf-connecting-ip", "198.51.100.5".parse().unwrap());
        assert_eq!(client_ip(&h), ip("198.51.100.5"), "cf-connecting-ip wins");
    }
}
