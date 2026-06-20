//! Shared application state passed to every handler.
//!
//! Kept small and `Arc`-friendly — Axum clones state per request. Adding
//! anything heavy here is a design smell; prefer per-request context (extracted
//! from `RequestContext` in `tt-shared`).

use std::sync::Arc;

use tt_auth::{KeyStore, ProviderCredentialStore};
use tt_cache::{EmbeddingProvider, L1Cache, L2Cache};
use tt_routing::CachingRoutingStore;
use tt_telemetry::{body_capture::BodyCaptureWriter, request_logs::RequestLogWriter};

use crate::budget::{BudgetEnforcer, DynamicBudgetEnforcer};
use crate::failover::CircuitBreaker;
use crate::middleware::argon2_cap::{Argon2Cap, Argon2CapConfig, Argon2VerifyCap};
use crate::middleware::key_cache::{KeyVerifyCache, RevocationRegistry, VerifyCache};
use crate::registry::{register_default_providers, ProviderRegistry};
use crate::single_flight::SingleFlight;
use crate::tier_resolver::TierResolver;

/// Default L2 cosine-similarity threshold per ADR-008 / spec §4.4.
/// Below this, a cached entry is considered too far from the query to reuse.
pub const DEFAULT_L2_THRESHOLD: f32 = 0.92;

/// Default L1 TTL — 24 hours. Spec §8.4 caps this per-tier; gateway-level
/// default is conservative until tier resolution lands with auth.
pub const DEFAULT_L1_TTL_SECS: u64 = 24 * 60 * 60;

/// Default FP-tolerance pct for the L2 verify gate's adaptive ratchet (COST-5).
/// Matches the `with_l2_verify` / `TT_L2_FP_TOLERANCE_PCT` default; clamped to
/// the research route-tolerance band `[0.5, 2.0]` at construction.
pub const DEFAULT_L2_VERIFY_TOLERANCE_PCT: f64 = 1.0;

/// The L2 false-positive verify gate (research Phase 2.2). Wired via
/// [`AppState::with_l2_verify`]; absent (`L2Config::verify == None`, the
/// default) the L2 hit path behaves byte-identically to today.
#[derive(Clone)]
pub struct L2VerifyConfig {
    /// Ambiguous band width above the effective threshold. A hit with
    /// `similarity >= t_eff + epsilon` is confident (no verify step). Default
    /// 0.02, clamped to `[0.0, 0.05]` at construction.
    pub epsilon: f32,
    /// Minimum lexical agreement ([`tt_cache::lexical_agreement`]) for an
    /// ambiguous-band hit to be served. Default
    /// [`tt_cache::DEFAULT_LEXICAL_MIN_AGREEMENT`], clamped to `[0.5, 1.0]`.
    pub min_agreement: f32,
    /// Effective FP tolerance pct fed to the adaptive estimator. Default 1.0,
    /// clamped to the research route-tolerance band `[0.5, 2.0]`.
    pub tolerance_pct: f64,
    /// Shared adaptive per-class thresholds (base = this `L2Config`'s
    /// `class_thresholds`). Read on the hit path; fed by judged
    /// ambiguous-band verdicts inside the detached judge task.
    pub gate: Arc<tt_cache::AdaptiveClassThresholds>,
}

/// Volatility-class TTL for L2 inserts: volatile queries (news/realtime/
/// version-ish — see [`crate::cache_volatility::classify_volatility`]) get a
/// SHORTENED L2 TTL. Shorten-only by construction: the ceiling is the base TTL
/// itself, and an explicit per-request `tt_extras` TTL override always wins.
#[derive(Clone, Copy, Debug)]
pub struct L2VolatilityTtl {
    /// Multiplier applied to a volatile entry's L2 TTL. Clamped to `(0.0, 1.0]`
    /// (shorten-only). Default 0.25.
    pub volatile_multiplier: f64,
    /// The shortened TTL never drops below `min(floor_secs, base)`. Default 300.
    pub floor_secs: u64,
}

impl Default for L2VolatilityTtl {
    fn default() -> Self {
        Self {
            volatile_multiplier: 0.25,
            floor_secs: 300,
        }
    }
}

/// L2 lookup wiring. Both fields are `Some` to enable semantic caching;
/// otherwise the gateway skips the L2 branch and goes straight to the provider.
#[derive(Clone)]
pub struct L2Config {
    pub cache: Arc<dyn L2Cache>,
    pub embedder: Arc<dyn EmbeddingProvider>,
    /// Global cosine similarity threshold (0.0 = anything, 1.0 = exact).
    /// Default 0.92. This is the **floor** for every per-class threshold — a
    /// classed lookup can raise the bar above it but never below it.
    pub threshold: f32,
    /// Per-[`tt_cache::TaskClass`] thresholds. Globally-fixed for v1: every class
    /// defaults to [`DEFAULT_L2_THRESHOLD`] (== `threshold`), so behaviour is
    /// identical to the single-threshold gateway until a class is explicitly
    /// raised. Never loosens a class below the global floor.
    pub class_thresholds: tt_cache::ClassThresholds,
    /// FP verify gate. **ON BY DEFAULT** (COST-5(U)): [`AppState::with_l2`]
    /// seeds it from [`tt_cache::LexicalVerifyConfig::default()`] so an
    /// unconfigured gateway verifies ambiguous-band hits (pre-0018 rows fail
    /// open). `None` disables it entirely (today's pre-COST-5 behavior);
    /// `TT_L2_VERIFY` / [`AppState::with_l2_verify`] override the knobs.
    pub verify: Option<L2VerifyConfig>,
    /// Volatility-class TTL. **ON BY DEFAULT** (P2): [`AppState::with_l2`] seeds
    /// it with [`L2VolatilityTtl::default`] so volatile prompts get a shortened
    /// L2 TTL out of the box. Shorten-only, so a misclassification's worst case
    /// is an early re-dispatch (a miss), never a stale answer. `None` disables it
    /// (pre-P2 behavior); override the knobs via
    /// [`AppState::with_l2_volatility_ttl`] or disable via
    /// [`AppState::without_l2_volatility_ttl`] / `TT_L2_VOLATILITY_TTL=0`.
    pub volatility_ttl: Option<L2VolatilityTtl>,
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
    /// Optional encrypted request/response body-capture writer. Capture is
    /// still controlled per org by `request_body_capture_settings`; wiring this
    /// writer only arms the best-effort sink. The chat handler consults the
    /// writer's `is_capture_enabled(org_id)` opt-in probe before persisting a
    /// body or stamping the `x-tokentrimmer-captured` header, so an armed-but-
    /// not-opted-in org neither stores a body nor advertises capture.
    pub body_capture_writer: Option<Arc<dyn BodyCaptureWriter>>,
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
    /// Optional cross-instance API-key revocation registry (SEC-4). Backed by
    /// the same Redis the L1 cache uses; auto-wired by [`AppState::with_l1`].
    ///
    /// When `Some`, the auth middleware checks it on every positive
    /// `verify_cache` hit and forces a re-verify for a key marked revoked on
    /// any replica — closing the per-replica staleness window the local-only
    /// [`KeyVerifyCache::evict_key_id`](crate::middleware::key_cache::KeyVerifyCache::evict_key_id)
    /// leaves open. `None` (no Redis wired — tests, dev) degrades cleanly to
    /// that per-replica behavior bounded by `POSITIVE_TTL_SECS`.
    pub revocation: Option<Arc<RevocationRegistry>>,
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
    /// SEC-6 loopback guard for [`Self::dogfood_enabled`].
    ///
    /// Stamping a fixed org identity onto *anonymous* (tokenless) traffic is
    /// only safe when the gateway cannot be reached by strangers — i.e. it is
    /// bound to loopback. On a shared/public bind it would treat every
    /// anonymous caller as the dogfood org (and, with a credential store wired,
    /// could spend the dogfood org's upstream credentials). The dogfood stamp
    /// therefore **fails closed**: it is applied only when this is `true`.
    ///
    /// Default `false` (fail closed). [`Self::with_dogfood_enabled`] sets it
    /// `true` (the in-process/loopback dev + test path), while the
    /// production-safe [`Self::with_dogfood_enabled_for_bind`] derives it from
    /// the resolved bind address.
    pub dogfood_bind_is_loopback: bool,
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
    /// — there's nowhere to record. Recording sinks never pause routes; the ONE
    /// sanctioned pause path is the opt-in
    /// [`crate::route_autopause::AutoPauseJudgeSink`] appended via
    /// [`AppState::with_route_auto_pause`].
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
    /// Gateway-side rolling latency window — the REAL signal behind the
    /// `upstream_latency_ms_p95_gt` route condition. Every live upstream dispatch
    /// records its observed latency here keyed by `(provider, model)`; the chat
    /// handler queries its live p95 (which is `None` until enough samples exist —
    /// cold start) when evaluating routes. Always present (in-process,
    /// per-instance); see [`tt_routing::LatencyTracker`].
    pub latency_tracker: Arc<tt_routing::LatencyTracker>,
    /// Upstream deadline for a canary **shadow** dispatch (`RouteAction::shadow_model`).
    /// SEPARATE from the request's own timeout and intentionally short (default
    /// 2s) — the shadow result is discarded, so it must never delay the primary
    /// response. The shadow runs concurrently with the primary; this caps how
    /// long the request handler will wait on the shadow before giving up and
    /// recording a shadow error (the primary response is already returned).
    pub shadow_timeout: std::time::Duration,
    /// Hourly in-process cap on L2-hit judge spawns (per gateway instance).
    /// Always present (like `verify_cache`); built from
    /// `judge_config.l2_hit_max_per_hour` and rebuilt whenever a judge config
    /// is attached. Consumed only by L2-hit samples that would otherwise spawn
    /// a judge — the dispatch-path judge is unaffected.
    pub l2_hit_judge_limiter: Arc<crate::quality_sample::L2HitJudgeLimiter>,
    /// Optional route-level netted-savings read-side backing
    /// `GET /v1/routes/:id/savings` (research Phase 2.3 judge-tax netting).
    /// `None` (default) → the endpoint answers 503 until
    /// [`AppState::with_route_savings`] wires a source (a
    /// [`crate::route_savings::PostgresRouteSavingsSource`] in production).
    pub route_savings: Option<Arc<dyn crate::route_savings::RouteSavingsSource>>,
    /// Optional Postgres pool handle used by the `GET /ready` readiness probe
    /// to issue a `SELECT 1` liveness check against the database.
    ///
    /// `None` (the default, and the intentional DB-less degraded mode) means
    /// no DB was configured at boot — `/ready` then treats Postgres as a
    /// non-dependency and does not 503 on it. When a DB URL *was* provided at
    /// boot the wiring layer attaches the pool here via
    /// [`with_db_pool`](Self::with_db_pool), and a dead pool makes `/ready`
    /// answer 503 so an orchestrator (Fly) pulls the process out of rotation.
    /// Other pool-backed handles (`request_log_writer`, `key_store`, …) keep
    /// the pool wrapped behind their trait objects; this is the one raw handle
    /// the probe needs.
    pub db_pool: Option<sqlx::PgPool>,
    /// Optional [`TaskTracker`](tokio_util::task::TaskTracker) for detached
    /// telemetry writes (REL-3). When `Some`, `spawn_request_log` and
    /// `spawn_body_capture` route their futures through the tracker so a
    /// graceful shutdown can drain them via `close()` + `wait()` instead of
    /// abandoning in-flight `request_logs` / body-capture inserts.
    ///
    /// `None` (default) keeps the existing fire-and-forget behavior so all
    /// existing constructors and tests are unaffected.
    pub telemetry_tracker: Option<tokio_util::task::TaskTracker>,
    /// Per-class trust gate for the agent-loop summarize lever (slice 2c-1).
    /// `NeverCommitGate` by default ⇒ summarize is a total no-op until an
    /// operator promotes classes via `TT_SUMMARIZE_TRUSTED_CLASSES`.
    pub summary_gate: Arc<dyn crate::passes::agentic_budget::summarize_judge::SummaryGate>,
}

/// Default deadline for a discarded shadow dispatch (2s). Short by design: the
/// shadow's only purpose is cost/quality measurement, never the served response.
pub const DEFAULT_SHADOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

impl AppState {
    /// Construct from a caller-supplied registry. Tests and embedded uses.
    /// L1, L2, key store, and credential store are disabled by default — wire
    /// them via the corresponding builder methods.
    pub fn new(registry: ProviderRegistry) -> Self {
        let judge_config = crate::quality_sample::JudgeConfig::from_env();
        let l2_hit_judge_limiter = Arc::new(crate::quality_sample::L2HitJudgeLimiter::new(
            judge_config.l2_hit_max_per_hour,
        ));
        Self {
            registry: Arc::new(registry),
            l1: None,
            l2: None,
            key_store: None,
            credential_store: None,
            request_log_writer: None,
            body_capture_writer: None,
            routing_store: None,
            verify_cache: Arc::new(KeyVerifyCache::new()),
            revocation: None,
            argon2_cap: Argon2VerifyCap::new(Argon2CapConfig::from_env()),
            dogfood_enabled: false,
            dogfood_bind_is_loopback: false,
            budget: None,
            tier_resolver: None,
            dynamic_budget: Arc::new(DynamicBudgetEnforcer::new()),
            breaker: Arc::new(CircuitBreaker::default()),
            single_flight: Arc::new(SingleFlight::new()),
            judge_config,
            judge_sink: None,
            judge_band_store: None,
            latency_tracker: Arc::new(tt_routing::LatencyTracker::new()),
            shadow_timeout: DEFAULT_SHADOW_TIMEOUT,
            l2_hit_judge_limiter,
            route_savings: None,
            db_pool: None,
            telemetry_tracker: None,
            summary_gate: Arc::new(crate::passes::agentic_budget::summarize_judge::NeverCommitGate),
        }
    }

    /// Builder-style attach: register the Postgres pool with the readiness
    /// probe (`GET /ready`). Pass the SAME pool the telemetry/auth/routing
    /// stores were built from. Once attached, a dead pool makes `/ready`
    /// return 503 (hard dependency); leaving it unset keeps the DB-less
    /// degraded mode where `/ready` does not gate on Postgres.
    #[must_use]
    pub fn with_db_pool(mut self, pool: sqlx::PgPool) -> Self {
        self.db_pool = Some(pool);
        self
    }

    /// Attach a [`TaskTracker`](tokio_util::task::TaskTracker) so detached
    /// telemetry writes (`request_logs`, body capture) are drained on graceful
    /// shutdown instead of abandoned (REL-3).
    ///
    /// Pass a `tracker.clone()` here; after `axum::serve` returns, call
    /// `tracker.close()` then `tokio::time::timeout(…, tracker.wait()).await`
    /// to drain all in-flight writes before the process exits.
    #[must_use]
    pub fn with_telemetry_tracker(mut self, tracker: tokio_util::task::TaskTracker) -> Self {
        self.telemetry_tracker = Some(tracker);
        self
    }

    /// Builder-style attach: set the process-wide summary trust gate. Defaults
    /// to `NeverCommitGate`; production wires `RatchetSummaryGate::from_env()`
    /// (slice 2c-2: the operator allowlist opens a class, the live judge shuts
    /// it on a windowed pass-rate dip).
    #[must_use]
    pub fn with_summary_gate(
        mut self,
        gate: Arc<dyn crate::passes::agentic_budget::summarize_judge::SummaryGate>,
    ) -> Self {
        self.summary_gate = gate;
        self
    }

    /// Construct the default production state: registry pre-seeded with all
    /// in-tree providers. As new provider crates land, extend
    /// [`register_default_providers`] — this constructor stays stable.
    /// L1/L2/auth stay disabled until the corresponding backends are wired at startup.
    pub fn with_default_providers() -> Self {
        let mut registry = ProviderRegistry::new();
        register_default_providers(&mut registry);
        Self::new(registry).with_summary_gate(Arc::new(
            crate::passes::agentic_budget::summarize_judge::RatchetSummaryGate::from_env(),
        ))
    }

    /// Builder-style attach: enable L1 exact-match cache with the given backend.
    ///
    /// Also wires the cross-instance API-key [`RevocationRegistry`] (SEC-4) from
    /// the SAME backend — L1 *is* the shared Redis, so a revoke fanned out via
    /// the registry reaches every replica that has L1 enabled. An explicit
    /// [`with_revocation_registry`](Self::with_revocation_registry) called after
    /// this overrides the auto-wired registry (e.g. to point it at a separate
    /// backend).
    pub fn with_l1(mut self, cache: Arc<dyn L1Cache>, ttl_secs: Option<u64>) -> Self {
        self.revocation = Some(Arc::new(RevocationRegistry::new(cache.clone())));
        self.l1 = Some(L1Config {
            cache,
            ttl_secs: ttl_secs.unwrap_or(DEFAULT_L1_TTL_SECS),
        });
        self
    }

    /// Builder-style attach: wire the cross-instance API-key revocation registry
    /// (SEC-4) on a specific [`L1Cache`] backend, independent of the L1 request
    /// cache. [`with_l1`](Self::with_l1) already auto-wires the registry from
    /// the L1 backend; use this only to point revocation at a different backend
    /// or to enable revocation without L1 request caching.
    #[must_use]
    pub fn with_revocation_registry(mut self, backend: Arc<dyn L1Cache>) -> Self {
        self.revocation = Some(Arc::new(RevocationRegistry::new(backend)));
        self
    }

    /// Revoke an API key everywhere: evict it from this replica's verify cache
    /// (immediate, local) AND publish a cross-instance revocation marker so
    /// peer replicas force a re-verify on their next positive hit (SEC-4). The
    /// marker publish is a no-op when no [`revocation`](Self::revocation)
    /// registry is wired — peers then fall back to the `POSITIVE_TTL_SECS`
    /// window. Call this from the key-revocation path instead of a bare
    /// `verify_cache.evict_key_id`.
    pub async fn revoke_key(&self, key_id: uuid::Uuid) {
        self.verify_cache.evict_key_id(key_id);
        if let Some(reg) = self.revocation.as_ref() {
            reg.mark_revoked(key_id).await;
        }
    }

    /// Builder-style attach: enable L2 semantic cache with the given backend.
    pub fn with_l2(
        mut self,
        cache: Arc<dyn L2Cache>,
        embedder: Arc<dyn EmbeddingProvider>,
        threshold: Option<f32>,
    ) -> Self {
        let threshold = threshold.unwrap_or(DEFAULT_L2_THRESHOLD);
        self.l2 = Some(L2Config {
            cache,
            embedder,
            threshold,
            // v1: every task class at the global threshold (no per-org config, no
            // A/B). The floor is the configured global threshold, itself clamped
            // up to `DEFAULT_L2_THRESHOLD`, so a class can only ever raise the bar,
            // never lower it below today's value.
            class_thresholds: tt_cache::ClassThresholds::with_global_floor(threshold),
            // COST-5(U): the FP verify gate is ON by default for the ambiguous
            // band, sourcing its lexical knobs from
            // [`tt_cache::LexicalVerifyConfig::default()`] (the documented
            // default-ON gate: a 0.02 band verified at
            // `DEFAULT_LEXICAL_MIN_AGREEMENT`). An unconfigured gateway thus
            // lexically verifies near-threshold hits at ~zero cost; pre-0018
            // rows (no signature) fail OPEN, so legacy data never turns into a
            // miss. `with_l2_verify` overrides these from env. Volatility TTL is
            // ON by default too (see `volatility_ttl` below).
            verify: Some({
                let lex = tt_cache::LexicalVerifyConfig::default();
                let class_thresholds = tt_cache::ClassThresholds::with_global_floor(threshold);
                L2VerifyConfig {
                    epsilon: lex.epsilon,
                    min_agreement: lex.min_agreement,
                    tolerance_pct: DEFAULT_L2_VERIFY_TOLERANCE_PCT,
                    gate: Arc::new(tt_cache::AdaptiveClassThresholds::new(
                        class_thresholds,
                        tt_cache::FpGateTuning::default(),
                    )),
                }
            }),
            // Volatility-class TTL is ON by default (P2): volatile prompts get a
            // shortened L2 TTL so a stale realtime answer can't be re-served for
            // the full base TTL. Shorten-only by construction — a
            // misclassification's worst case is an early re-dispatch (a cache
            // miss), NEVER a stale answer — so default-on is safe. Disable via
            // [`without_l2_volatility_ttl`](Self::without_l2_volatility_ttl) /
            // `TT_L2_VOLATILITY_TTL=0`; override the knobs via
            // [`with_l2_volatility_ttl`](Self::with_l2_volatility_ttl).
            volatility_ttl: Some(L2VolatilityTtl::default()),
        });
        self
    }

    /// Builder-style attach (post-`with_l2`): enable the L2 false-positive
    /// verify gate. Ambiguous-band hits (`t_eff <= sim < t_eff + epsilon`) are
    /// lexically verified against the entry's one-way signature and rejected
    /// (treated as a miss) below `min_agreement`; judged in-band verdicts feed
    /// an adaptive per-class threshold ratchet targeting `tolerance_pct` FP
    /// (the research route-tolerance band 0.5–2%).
    ///
    /// All inputs are clamped to their documented safe ranges. No-op when L2
    /// itself is not attached.
    #[must_use]
    pub fn with_l2_verify(
        mut self,
        epsilon: f32,
        min_agreement: f32,
        tolerance_pct: f64,
        tuning: tt_cache::FpGateTuning,
    ) -> Self {
        if let Some(l2) = self.l2.as_mut() {
            l2.verify = Some(L2VerifyConfig {
                epsilon: epsilon.clamp(0.0, 0.05),
                min_agreement: min_agreement.clamp(0.5, 1.0),
                tolerance_pct: tolerance_pct.clamp(0.5, 2.0),
                gate: Arc::new(tt_cache::AdaptiveClassThresholds::new(
                    l2.class_thresholds.clone(),
                    tuning,
                )),
            });
        }
        self
    }

    /// Builder-style attach (post-`with_l2`): enable volatility-class TTLs for
    /// L2 inserts. Volatile queries (time-sensitive / realtime-data /
    /// code-version-ish — [`crate::cache_volatility::classify_volatility`])
    /// get `base_ttl * volatile_multiplier`, floor/ceiling-bounded; stable
    /// queries and explicit per-request TTL overrides are untouched. No-op
    /// when L2 itself is not attached.
    #[must_use]
    pub fn with_l2_volatility_ttl(mut self, cfg: L2VolatilityTtl) -> Self {
        if let Some(l2) = self.l2.as_mut() {
            l2.volatility_ttl = Some(L2VolatilityTtl {
                // Shorten-only: a multiplier above 1.0 would EXTEND volatile
                // TTLs; clamp into (0, 1]. A non-positive value falls back to
                // the default rather than zeroing every volatile TTL.
                volatile_multiplier: if cfg.volatile_multiplier > 0.0 {
                    cfg.volatile_multiplier.min(1.0)
                } else {
                    L2VolatilityTtl::default().volatile_multiplier
                },
                floor_secs: cfg.floor_secs,
            });
        }
        self
    }

    /// Builder-style: DISABLE volatility-class TTLs for L2 inserts, reverting to
    /// the pre-P2 behavior where every L2 entry (volatile or not) gets the full
    /// base tier TTL. Volatility TTL is ON by default (set by [`with_l2`]); this
    /// is the escape hatch the `TT_L2_VOLATILITY_TTL=0` env gate selects. No-op
    /// when L2 itself is not attached.
    ///
    /// [`with_l2`]: Self::with_l2
    #[must_use]
    pub fn without_l2_volatility_ttl(mut self) -> Self {
        if let Some(l2) = self.l2.as_mut() {
            l2.volatility_ttl = None;
        }
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

    /// Builder-style attach: enable encrypted body capture for opted-in orgs.
    pub fn with_body_capture_writer(mut self, writer: Arc<dyn BodyCaptureWriter>) -> Self {
        self.body_capture_writer = Some(writer);
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
    /// or a hand-built config (tests). Recording sinks never pause routes —
    /// the one sanctioned pause path is the opt-in
    /// [`crate::route_autopause::AutoPauseJudgeSink`] appended via
    /// [`AppState::with_route_auto_pause`].
    pub fn with_quality_judge(
        mut self,
        sink: Arc<dyn crate::quality_sample::JudgeSink>,
        config: crate::quality_sample::JudgeConfig,
    ) -> Self {
        self.judge_sink = Some(sink);
        self.set_judge_config(config);
        self
    }

    /// Install `config` and rebuild the L2-hit judge limiter from its hourly
    /// cap, so a hand-built config (tests, embedded uses) caps exactly as an
    /// env-built one would.
    fn set_judge_config(&mut self, config: crate::quality_sample::JudgeConfig) {
        self.l2_hit_judge_limiter = Arc::new(crate::quality_sample::L2HitJudgeLimiter::new(
            config.l2_hit_max_per_hour,
        ));
        self.judge_config = config;
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
    /// (zero added latency). The band store never pauses routes — the one
    /// sanctioned pause path is the opt-in
    /// [`crate::route_autopause::AutoPauseJudgeSink`] appended via
    /// [`AppState::with_route_auto_pause`].
    pub fn with_quality_judge_band_store(
        mut self,
        store: Arc<crate::quality_sample::InMemoryJudgeBandStore>,
        config: crate::quality_sample::JudgeConfig,
    ) -> Self {
        self.judge_sink = Some(store.clone() as Arc<dyn crate::quality_sample::JudgeSink>);
        self.judge_band_store = Some(store);
        self.set_judge_config(config);
        self
    }

    /// Builder-style attach: wire the sampled judge to BOTH the in-process
    /// band store (the `/v1/preview` enrichment read-side, exactly as
    /// [`AppState::with_quality_judge_band_store`]) AND a durable sink — in
    /// production a [`crate::quality_persist::PostgresJudgeSink`] writing one
    /// `quality_verdicts` row per judged sample, which the Phase-2.3 netting
    /// aggregates per route via
    /// [`crate::route_savings::ROUTE_SAVINGS_SQL`] (a `route_id` GROUP BY,
    /// not a per-request trace join).
    ///
    /// Outcomes fan out through a [`crate::quality_sample::FanoutJudgeSink`]:
    /// band store first, then the persistent sink. Neither recording sink
    /// ever pauses routes — the one sanctioned pause path is the opt-in
    /// [`crate::route_autopause::AutoPauseJudgeSink`] appended via
    /// [`AppState::with_route_auto_pause`] (call it AFTER this builder so the
    /// durable row precedes the window read); the judge still fires only for
    /// the deterministic sample of eligible requests, after the user response
    /// is returned.
    pub fn with_quality_judge_persistent(
        mut self,
        store: Arc<crate::quality_sample::InMemoryJudgeBandStore>,
        persistent: Arc<dyn crate::quality_sample::JudgeSink>,
        config: crate::quality_sample::JudgeConfig,
    ) -> Self {
        self.judge_sink = Some(Arc::new(crate::quality_sample::FanoutJudgeSink::new(vec![
            store.clone() as Arc<dyn crate::quality_sample::JudgeSink>,
            persistent,
        ])));
        self.judge_band_store = Some(store);
        self.set_judge_config(config);
        self
    }

    /// Builder-style attach: enable the route-level netted-savings read-side
    /// behind `GET /v1/routes/:id/savings`. Production wires a
    /// [`crate::route_savings::PostgresRouteSavingsSource`]; tests seed a
    /// [`crate::route_savings::InMemoryRouteSavingsSource`]. Until called,
    /// the endpoint answers 503 (aggregation not configured) — wiring this
    /// changes no request-path behavior.
    pub fn with_route_savings(
        mut self,
        src: Arc<dyn crate::route_savings::RouteSavingsSource>,
    ) -> Self {
        self.route_savings = Some(src);
        self
    }

    /// Append the auto-pause evaluator to the configured judge sink (wraps the
    /// existing sink in a [`crate::quality_sample::FanoutJudgeSink`]
    /// `[existing, auto_pause]`). MUST be called AFTER
    /// [`AppState::with_quality_judge_persistent`] (or another judge builder)
    /// so verdicts are durably recorded before the evaluator consults the
    /// window — `FanoutJudgeSink` awaits sinks sequentially in order. A no-op
    /// (debug_assert + warn) when no judge sink is configured: with no judge
    /// there are no verdicts, so there is nothing to evaluate.
    ///
    /// This is the ONE sanctioned route-pause path, and it is doubly opt-in:
    /// the operator must wire this builder AND each route must set
    /// `auto_pause: true`.
    pub fn with_route_auto_pause(
        mut self,
        evaluator: Arc<crate::route_autopause::AutoPauseJudgeSink>,
    ) -> Self {
        let Some(existing) = self.judge_sink.take() else {
            debug_assert!(
                false,
                "with_route_auto_pause called before any with_quality_judge* builder"
            );
            tracing::warn!(
                "with_route_auto_pause: no judge sink configured — auto-pause evaluator not wired"
            );
            return self;
        };
        self.judge_sink = Some(Arc::new(crate::quality_sample::FanoutJudgeSink::new(vec![
            existing,
            evaluator as Arc<dyn crate::quality_sample::JudgeSink>,
        ])));
        self
    }

    /// Builder-style: enable dogfood routing mode on a **trusted (loopback)**
    /// bind. The auth middleware will inject a [`crate::DOGFOOD_ORG_ID`]
    /// identity for unauthenticated requests so the pre-seeded dogfood route
    /// fires.
    ///
    /// This variant marks the bind as loopback (see
    /// [`Self::dogfood_bind_is_loopback`]) and is the in-process/loopback dev +
    /// test entry point. A production boot that may bind a shared address MUST
    /// use [`Self::with_dogfood_enabled_for_bind`] instead so the dogfood stamp
    /// fails closed off loopback (SEC-6).
    pub fn with_dogfood_enabled(mut self) -> Self {
        self.dogfood_enabled = true;
        self.dogfood_bind_is_loopback = true;
        self
    }

    /// Builder-style: enable dogfood routing mode, deriving the SEC-6 loopback
    /// guard from the gateway's resolved bind address.
    ///
    /// Pass `bind_ip.is_loopback()`. When the bind is **not** loopback the
    /// dogfood stamp fails closed — `dogfood_enabled` is still recorded but the
    /// auth middleware will not stamp anonymous traffic with the dogfood org
    /// (and logs a loud warning on first use, louder still when a credential
    /// store is wired). This is the production-safe wiring the gateway boot
    /// path should use.
    #[must_use]
    pub fn with_dogfood_enabled_for_bind(mut self, bind_is_loopback: bool) -> Self {
        self.dogfood_enabled = true;
        self.dogfood_bind_is_loopback = bind_is_loopback;
        if !bind_is_loopback {
            tracing::warn!(
                "SEC-6: TT_DOGFOOD_GROQ_ROUTING is enabled but the gateway is NOT bound to \
                 loopback — anonymous-request dogfood-org stamping is DISABLED (fail closed). \
                 Anonymous traffic will not be treated as the dogfood org on a shared bind. \
                 Bind loopback (TT_BIND_ADDR=127.0.0.1) to use dogfood routing."
            );
        }
        self
    }

    /// SEC-6: `true` only when dogfood routing is enabled AND the bind is
    /// loopback. The auth middleware stamps the anonymous dogfood-org identity
    /// exclusively when this holds, so a shared/public bind never grants the
    /// fixed dogfood identity to strangers.
    #[must_use]
    pub fn dogfood_active(&self) -> bool {
        self.dogfood_enabled && self.dogfood_bind_is_loopback
    }

    /// Start background catalogue refreshers (currently: OpenRouter's dynamic
    /// `GET /models` fetch). Call once from the gateway's async `main` after
    /// building the state. Best-effort and non-blocking — a failed fetch is
    /// logged and the provider keeps serving its static baseline, so this never
    /// delays startup or the request path. A no-op when OpenRouter is disabled.
    pub fn spawn_background_refreshers(&self) {
        crate::registry::spawn_openrouter_catalog_refresh(&self.registry);
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

    /// SEC-6: `dogfood_active()` is the AND of `dogfood_enabled` and the
    /// loopback guard, so dogfood org stamping fails closed off loopback.
    #[test]
    fn dogfood_active_requires_enabled_and_loopback() {
        // Default: dogfood off → inactive.
        let off = AppState::with_default_providers();
        assert!(!off.dogfood_enabled);
        assert!(!off.dogfood_bind_is_loopback);
        assert!(!off.dogfood_active());

        // Loopback dev/test path: enabled + loopback → active.
        let loopback = AppState::with_default_providers().with_dogfood_enabled();
        assert!(loopback.dogfood_enabled);
        assert!(loopback.dogfood_bind_is_loopback);
        assert!(loopback.dogfood_active());

        // Production-safe builder with a loopback bind → active.
        let bound_loopback = AppState::with_default_providers().with_dogfood_enabled_for_bind(true);
        assert!(bound_loopback.dogfood_active());

        // Production-safe builder with a NON-loopback bind → enabled but
        // fails closed (inactive).
        let bound_public = AppState::with_default_providers().with_dogfood_enabled_for_bind(false);
        assert!(bound_public.dogfood_enabled);
        assert!(!bound_public.dogfood_bind_is_loopback);
        assert!(
            !bound_public.dogfood_active(),
            "dogfood must fail closed on a non-loopback bind"
        );
    }

    #[test]
    fn default_summary_gate_never_commits() {
        let st = AppState::new(crate::registry::ProviderRegistry::new());
        assert!(!st.summary_gate.is_committable("inspect_diff"));
    }

    #[test]
    fn with_summary_gate_overrides_default() {
        use crate::passes::agentic_budget::summarize_judge::AlwaysCommitGate;
        let st = AppState::new(crate::registry::ProviderRegistry::new())
            .with_summary_gate(std::sync::Arc::new(AlwaysCommitGate));
        assert!(st.summary_gate.is_committable("anything"));
    }

    // The prod gate is `RatchetSummaryGate` (slice 2c-2): an allowlisted,
    // never-judged class is committable (operator opens); a non-allowlisted one
    // is not. Asserted env-free via the gate directly (the `with_default_providers`
    // swap itself is covered by compilation + `default_summary_gate_never_commits`).
    #[test]
    fn ratchet_gate_opens_allowlisted_class() {
        use crate::passes::agentic_budget::summarize_judge::{
            RatchetConfig, RatchetSummaryGate, SummaryGate,
        };
        let g = RatchetSummaryGate::new(
            ["inspect_diff".to_string()].into_iter().collect(),
            RatchetConfig::default(),
        );
        assert!(g.is_committable("inspect_diff"));
        assert!(!g.is_committable("write_file"));
    }
}
