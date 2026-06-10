//! Shared application state passed to every handler.
//!
//! Kept small and `Arc`-friendly — Axum clones state per request. Adding
//! anything heavy here is a design smell; prefer per-request context (extracted
//! from `RequestContext` in `tt-shared`).

use std::sync::Arc;

use tt_auth::{KeyStore, ProviderCredentialStore};
use tt_cache::{EmbeddingProvider, L1Cache, L2Cache};
use tt_routing::CachingRoutingStore;
use tt_telemetry::request_logs::RequestLogWriter;

use crate::budget::{BudgetEnforcer, DynamicBudgetEnforcer};
use crate::failover::CircuitBreaker;
use crate::middleware::argon2_cap::{Argon2Cap, Argon2CapConfig, Argon2VerifyCap};
use crate::middleware::key_cache::{KeyVerifyCache, VerifyCache};
use crate::registry::{register_default_providers, ProviderRegistry};
use crate::single_flight::SingleFlight;
use crate::tier_resolver::TierResolver;

/// Default L2 cosine-similarity threshold per ADR-008 / spec §4.4.
/// Below this, a cached entry is considered too far from the query to reuse.
pub const DEFAULT_L2_THRESHOLD: f32 = 0.92;

/// Default L1 TTL — 24 hours. Spec §8.4 caps this per-tier; gateway-level
/// default is conservative until tier resolution lands with auth.
pub const DEFAULT_L1_TTL_SECS: u64 = 24 * 60 * 60;

/// L2 lookup wiring. Both fields are `Some` to enable semantic caching;
/// otherwise the gateway skips the L2 branch and goes straight to the provider.
#[derive(Clone)]
pub struct L2Config {
    pub cache: Arc<dyn L2Cache>,
    pub embedder: Arc<dyn EmbeddingProvider>,
    /// Cosine similarity threshold (0.0 = anything, 1.0 = exact). Default 0.92.
    pub threshold: f32,
}

/// L1 exact-match lookup wiring. Cache hits short-circuit the provider call.
#[derive(Clone)]
pub struct L1Config {
    pub cache: Arc<dyn L1Cache>,
    /// TTL applied to newly-inserted entries. Default 24h.
    pub ttl_secs: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<ProviderRegistry>,
    /// Optional L1 exact-match cache. `None` disables L1 lookup (tests,
    /// dev environments without Redis).
    pub l1: Option<L1Config>,
    /// Optional L2 semantic cache. `None` disables L2 lookup (Free tier,
    /// tests, dev environments without an embedding backend).
    pub l2: Option<L2Config>,
    /// Optional `tt_live_*` API key verifier. When `None`, the auth middleware
    /// passes live keys through without verification (dev mode); when `Some`,
    /// invalid live keys 401.
    pub key_store: Option<Arc<dyn KeyStore>>,
    /// Optional per-org upstream credential lookup. The chat handler uses
    /// this to substitute the customer's real provider API key into the
    /// outbound request. `None` falls back to the legacy synthetic context.
    pub credential_store: Option<Arc<dyn ProviderCredentialStore>>,
    /// Optional `request_logs` writer. The chat handler spawns a
    /// fire-and-forget INSERT after every response. `None` skips the
    /// telemetry write (tests, dev mode without a DB).
    pub request_log_writer: Option<Arc<dyn RequestLogWriter>>,
    /// Optional per-org routing engine source. The chat handler asks for the
    /// org's [`tt_routing::RoutingEngine`] before dispatch; on a match it
    /// rewrites `req.model` and stamps `request_logs.route_id`. `None`
    /// disables routing entirely (tests, dev mode, free-tier orgs).
    pub routing_store: Option<Arc<CachingRoutingStore>>,
    /// In-process TTL cache for argon2 verify results.
    ///
    /// Always present (never `None`). The auth middleware consults this before
    /// calling `tt_auth::verify` so argon2 runs at most once per bearer token
    /// per TTL window rather than on every request.
    ///
    /// See [`crate::middleware::key_cache`] for TTL constants and the
    /// revocation-staleness tradeoff documentation.
    pub verify_cache: VerifyCache,
    /// Pre-auth per-IP rate cap consulted by the auth middleware **immediately
    /// before** the (CPU-expensive) argon2 verify on the cold path — i.e. only
    /// when a `tt_live_*` token misses the verify cache. Past the per-IP
    /// threshold the request is shed with 429 + `Retry-After` and argon2 is
    /// never invoked, so a flood of bogus keys cannot amplify a trickle of HTTP
    /// requests into pinned CPU. Cache hits (already-verified tokens) and
    /// non-`tt_live_*` traffic bypass it, so authenticated traffic is unaffected.
    ///
    /// Always present (in-memory, per-instance — see
    /// [`crate::middleware::argon2_cap`] for the "move to Redis before scaling"
    /// caveat). Limit defaults come from `TT_ARGON2_CAP_*` env (sane defaults).
    pub argon2_cap: Argon2Cap,
    /// When `true`, the auth middleware injects a dogfood [`ApiKeyContext`]
    /// for unauthenticated requests so the dogfood routing route fires.
    /// Enabled by setting `TT_DOGFOOD_GROQ_ROUTING=1` at startup.
    pub dogfood_enabled: bool,
    /// Optional per-org spend cap + request-rate enforcer. The auth middleware
    /// checks it pre-flight (429 on deny) and the chat handler records realized
    /// spend. `None` disables budget enforcement (tests, dev, unmetered orgs).
    pub budget: Option<Arc<dyn BudgetEnforcer>>,
    /// Optional per-org subscription tier resolver. When `Some`, the auth
    /// middleware resolves the org's effective tier, stamps
    /// `ApiKeyContext.tier`, and uses the resolved [`BudgetLimits`] for the
    /// pre-flight budget check (overriding the global `budget` enforcer for
    /// that org). `None` keeps today's behaviour: no tier resolution, the
    /// global enforcer (if any) applies unchanged.
    ///
    /// Tier resolution errors **fail open** — a DB blip falls back to Free
    /// limits and logs a warn rather than 429-ing the request.
    pub tier_resolver: Option<Arc<dyn TierResolver>>,
    /// Per-org dynamic budget state used when `tier_resolver` is `Some`.
    /// Holds the monthly/rate counters per org; the limits are supplied at
    /// check time from the resolved tier. Always present; only active when
    /// `tier_resolver` is set.
    pub dynamic_budget: Arc<DynamicBudgetEnforcer>,
    /// Per-provider circuit breaker shared across requests. Used by the chat
    /// handler when a matched route declares `fallbacks`: a provider that
    /// trips the breaker is skipped during failover until its cooldown
    /// elapses. Always present (default thresholds); failover is a no-op when
    /// no route declares fallbacks.
    pub breaker: Arc<CircuitBreaker>,
    /// In-process single-flight coalescing for the non-streaming cache-miss
    /// path. When N concurrent requests share the same L1 cache key and all
    /// miss, only the first (leader) dispatches to the provider; the others
    /// (followers) wait up to [`crate::single_flight::FOLLOWER_TIMEOUT`] and
    /// then re-read L1 or fall through to their own dispatch.
    ///
    /// Always present. Disabled by setting `l1` to `None` in `AppState`
    /// (single-flight is gated on cache_behavior.do_lookup, which requires L1).
    pub single_flight: Arc<SingleFlight>,
    /// Sampled async quality-judge config (sample rate, judge model, enable
    /// flag). Read from `TT_JUDGE_*` env; defaults keep the judge OFF. The chat
    /// handler consults this on the non-streaming path to decide whether to
    /// sample a rerouted-DOWN request for an off-path quality judge.
    pub judge_config: crate::quality_sample::JudgeConfig,
    /// Optional sink the sampled judge records its [`crate::JudgeOutcome`] into.
    /// `None` (default) disables the judge entirely regardless of `judge_config`
    /// — there's nowhere to record. Record-only: the sink never pauses routes.
    pub judge_sink: Option<Arc<dyn crate::quality_sample::JudgeSink>>,
    /// Optional typed judge-band store read by the `/v1/preview` handler to
    /// enrich route suggestions with the live judge's aggregate
    /// [`tt_preview::QualityRiskBand`] per `(requested → served)` swap — the
    /// production join that lifts a suggestion off the hard-coded `Unknown`.
    ///
    /// `None` (default) leaves suggestions at `Unknown`. When the judge is wired
    /// via [`AppState::with_quality_judge_band_store`], the SAME store is both the
    /// recording `judge_sink` and this read-side, so a recorded outcome flows
    /// end-to-end into a populated band. Record-only: enrichment is advisory.
    pub judge_band_store: Option<Arc<crate::quality_sample::InMemoryJudgeBandStore>>,
}

impl AppState {
    /// Construct from a caller-supplied registry. Tests and embedded uses.
    /// L1, L2, key store, and credential store are disabled by default — wire
    /// them via the corresponding builder methods.
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            registry: Arc::new(registry),
            l1: None,
            l2: None,
            key_store: None,
            credential_store: None,
            request_log_writer: None,
            routing_store: None,
            verify_cache: Arc::new(KeyVerifyCache::new()),
            argon2_cap: Argon2VerifyCap::new(Argon2CapConfig::from_env()),
            dogfood_enabled: false,
            budget: None,
            tier_resolver: None,
            dynamic_budget: Arc::new(DynamicBudgetEnforcer::new()),
            breaker: Arc::new(CircuitBreaker::default()),
            single_flight: Arc::new(SingleFlight::new()),
            judge_config: crate::quality_sample::JudgeConfig::from_env(),
            judge_sink: None,
            judge_band_store: None,
        }
    }

    /// Construct the default production state: registry pre-seeded with all
    /// in-tree providers. As new provider crates land, extend
    /// [`register_default_providers`] — this constructor stays stable.
    /// L1/L2/auth stay disabled until the corresponding backends are wired at startup.
    pub fn with_default_providers() -> Self {
        let mut registry = ProviderRegistry::new();
        register_default_providers(&mut registry);
        Self::new(registry)
    }

    /// Builder-style attach: enable L1 exact-match cache with the given backend.
    pub fn with_l1(mut self, cache: Arc<dyn L1Cache>, ttl_secs: Option<u64>) -> Self {
        self.l1 = Some(L1Config {
            cache,
            ttl_secs: ttl_secs.unwrap_or(DEFAULT_L1_TTL_SECS),
        });
        self
    }

    /// Builder-style attach: enable L2 semantic cache with the given backend.
    pub fn with_l2(
        mut self,
        cache: Arc<dyn L2Cache>,
        embedder: Arc<dyn EmbeddingProvider>,
        threshold: Option<f32>,
    ) -> Self {
        self.l2 = Some(L2Config {
            cache,
            embedder,
            threshold: threshold.unwrap_or(DEFAULT_L2_THRESHOLD),
        });
        self
    }

    /// Builder-style attach: enable API key verification (`tt_live_*`).
    pub fn with_key_store(mut self, store: Arc<dyn KeyStore>) -> Self {
        self.key_store = Some(store);
        self
    }

    /// Builder-style attach: enable per-org upstream credential lookup.
    pub fn with_credential_store(mut self, store: Arc<dyn ProviderCredentialStore>) -> Self {
        self.credential_store = Some(store);
        self
    }

    /// Builder-style attach: enable per-request telemetry rows.
    pub fn with_request_log_writer(mut self, writer: Arc<dyn RequestLogWriter>) -> Self {
        self.request_log_writer = Some(writer);
        self
    }

    /// Builder-style attach: enable per-org routing.
    pub fn with_routing_store(mut self, store: Arc<CachingRoutingStore>) -> Self {
        self.routing_store = Some(store);
        self
    }

    /// Builder-style attach: enable the sampled async quality judge with the
    /// given recording sink and config. The judge scores a deterministic ~2%
    /// sample of rerouted-DOWN chat-completion requests AFTER the user response
    /// is returned (zero added latency) and records the outcome into `sink`.
    ///
    /// Pass [`crate::quality_sample::JudgeConfig::from_env`] to honor `TT_JUDGE_*`,
    /// or a hand-built config (tests). Record-only — the sink never pauses routes.
    pub fn with_quality_judge(
        mut self,
        sink: Arc<dyn crate::quality_sample::JudgeSink>,
        config: crate::quality_sample::JudgeConfig,
    ) -> Self {
        self.judge_sink = Some(sink);
        self.judge_config = config;
        self
    }

    /// Builder-style attach: wire the sampled judge to an in-process
    /// [`crate::quality_sample::InMemoryJudgeBandStore`] used as BOTH the
    /// recording sink AND the read-side the `/v1/preview` handler enriches
    /// suggestions from. This is the production join that lifts a route
    /// suggestion off the hard-coded `Unknown` once the live judge has scored
    /// that `(requested → served)` swap.
    ///
    /// The judge still only fires for the deterministic ~2% sample of
    /// rerouted-DOWN chat completions, AFTER the user response is returned
    /// (zero added latency). Record-only — the store never pauses routes.
    pub fn with_quality_judge_band_store(
        mut self,
        store: Arc<crate::quality_sample::InMemoryJudgeBandStore>,
        config: crate::quality_sample::JudgeConfig,
    ) -> Self {
        self.judge_sink = Some(store.clone() as Arc<dyn crate::quality_sample::JudgeSink>);
        self.judge_band_store = Some(store);
        self.judge_config = config;
        self
    }

    /// Builder-style: enable dogfood routing mode. The auth middleware will
    /// inject a [`crate::DOGFOOD_ORG_ID`] identity for unauthenticated
    /// requests so the pre-seeded dogfood route fires.
    pub fn with_dogfood_enabled(mut self) -> Self {
        self.dogfood_enabled = true;
        self
    }

    /// Builder-style attach: enable per-org spend cap + rate enforcement.
    pub fn with_budget(mut self, enforcer: Arc<dyn BudgetEnforcer>) -> Self {
        self.budget = Some(enforcer);
        self
    }

    /// Builder-style attach: enable per-org subscription tier resolution.
    ///
    /// When set, the auth middleware resolves each request's org tier from
    /// the subscription DB and uses the resolved limits for enforcement.
    /// Resolution errors fail open (Free limits + warn log) rather than
    /// 429-ing the request.
    pub fn with_tier_resolver(mut self, resolver: Arc<dyn TierResolver>) -> Self {
        self.tier_resolver = Some(resolver);
        self
    }

    /// The enforcer realized spend must be recorded into — the SAME selection the
    /// auth pre-flight budget check uses, so `monthly_cap_usd` actually trips.
    pub fn spend_sink(&self) -> crate::budget::SpendSink {
        if self.tier_resolver.is_some() {
            crate::budget::SpendSink::Dynamic(self.dynamic_budget.clone())
        } else if let Some(b) = self.budget.as_ref() {
            crate::budget::SpendSink::Global(b.clone())
        } else {
            crate::budget::SpendSink::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{BudgetEnforcer, BudgetLimits, InMemoryBudgetEnforcer, SpendSink};
    use crate::tier_resolver::{ResolvedTier, TierResolver, TierResolverError};
    use async_trait::async_trait;
    use uuid::Uuid;

    struct StubTierResolver;

    #[async_trait]
    impl TierResolver for StubTierResolver {
        async fn resolve(&self, _org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
            Ok(ResolvedTier::free_default())
        }
    }

    /// spend_sink() returns Dynamic when tier_resolver is set.
    #[test]
    fn spend_sink_is_dynamic_when_tier_resolver_set() {
        let state = AppState::with_default_providers()
            .with_tier_resolver(Arc::new(StubTierResolver) as Arc<dyn TierResolver>);
        assert!(
            matches!(state.spend_sink(), SpendSink::Dynamic(_)),
            "spend_sink must be Dynamic when tier_resolver is set"
        );
    }

    /// spend_sink() returns Global when only the legacy budget enforcer is set.
    #[test]
    fn spend_sink_is_global_when_only_budget_set() {
        let enforcer: Arc<dyn BudgetEnforcer> =
            Arc::new(InMemoryBudgetEnforcer::new(BudgetLimits::default()));
        let state = AppState::with_default_providers().with_budget(enforcer);
        assert!(
            matches!(state.spend_sink(), SpendSink::Global(_)),
            "spend_sink must be Global when only budget is set"
        );
    }

    /// spend_sink() returns None when neither tier_resolver nor budget is set.
    #[test]
    fn spend_sink_is_none_when_nothing_wired() {
        let state = AppState::with_default_providers();
        assert!(
            matches!(state.spend_sink(), SpendSink::None),
            "spend_sink must be None when no enforcer is wired"
        );
    }
}
