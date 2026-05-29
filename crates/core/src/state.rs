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

use crate::registry::{register_default_providers, ProviderRegistry};

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
}
