//! Shared application state passed to every handler.
//!
//! Kept small and `Arc`-friendly — Axum clones state per request. Adding
//! anything heavy here is a design smell; prefer per-request context (extracted
//! from `RequestContext` in `tt-shared`).

use std::sync::Arc;

use tt_cache::{EmbeddingProvider, L2Cache};

use crate::registry::{register_default_providers, ProviderRegistry};

/// Default L2 cosine-similarity threshold per ADR-008 / spec §4.4.
/// Below this, a cached entry is considered too far from the query to reuse.
pub const DEFAULT_L2_THRESHOLD: f32 = 0.92;

/// L2 lookup wiring. Both fields are `Some` to enable semantic caching;
/// otherwise the gateway skips the L2 branch and goes straight to the provider.
#[derive(Clone)]
pub struct L2Config {
    pub cache: Arc<dyn L2Cache>,
    pub embedder: Arc<dyn EmbeddingProvider>,
    /// Cosine similarity threshold (0.0 = anything, 1.0 = exact). Default 0.92.
    pub threshold: f32,
}

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<ProviderRegistry>,
    /// Optional L2 semantic cache. `None` disables L2 lookup (Free tier,
    /// tests, dev environments without an embedding backend).
    pub l2: Option<L2Config>,
}

impl AppState {
    /// Construct from a caller-supplied registry. Tests and embedded uses.
    /// L2 is disabled by default — wire it via [`AppState::with_l2`].
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            registry: Arc::new(registry),
            l2: None,
        }
    }

    /// Construct the default production state: registry pre-seeded with all
    /// in-tree providers. As new provider crates land, extend
    /// [`register_default_providers`] — this constructor stays stable.
    /// L2 stays disabled until the embedding backend is wired at startup.
    pub fn with_default_providers() -> Self {
        let mut registry = ProviderRegistry::new();
        register_default_providers(&mut registry);
        Self::new(registry)
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
}
