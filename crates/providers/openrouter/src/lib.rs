//! OpenRouter provider adapter (OpenAI-compatible).
//!
//! Thin wrapper over [`tt_provider_compat::OpenAICompatibleProvider`] with
//! OpenRouter's models, pricing, and default endpoint baked in.
//!
//! OpenRouter is a pass-through gateway aggregating 300+ models. This adapter
//! serves a small static set as a baseline and, once
//! [`OpenRouterProvider::refresh_models`] has run, layers the full live
//! catalogue (fetched from `GET /models`) on top — see "Dynamic model list".
//!
//! # BYOK fee
//!
//! OpenRouter charges a 5% BYOK (Bring Your Own Key) fee on top of the
//! underlying model cost. This is stored in [`CompatConfig::fee_multiplier`]
//! as `1.05`. The fee is **not** applied at request time; the billing layer in
//! `tt-core` applies it when computing the final USD charge for the dashboard.
//!
//! # Dynamic model list
//!
//! OpenRouter exposes a `GET /models` endpoint that returns the full catalogue
//! dynamically, **with per-model pricing** (prompt/completion price per token).
//! [`OpenRouterProvider::refresh_models`] fetches it, parses the model list +
//! pricing, and caches it in an [`std::sync::RwLock`]. Call it at startup and
//! periodically (e.g. from a background task). [`Provider::models`] and
//! [`Provider::pricing`] then prefer the dynamic catalogue and fall back to the
//! static baseline, so a fetch that has never succeeded (or has just failed)
//! still serves the static list — a failed fetch returns `Err` and **never**
//! panics or clears the cache.
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_openrouter::OpenRouterProvider;
//! use tt_provider_compat::ClientConfig;
//!
//! # async fn run() {
//! let provider = OpenRouterProvider::new(ClientConfig::default());
//! // Populate the dynamic catalogue (ignore a transient fetch failure —
//! // the static baseline keeps serving).
//! let _ = provider.refresh_models().await;
//! # }
//! ```

mod dynamic;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use tt_provider_compat::client::build_client;
use tt_provider_compat::{ClientConfig, CompatConfig, OpenAICompatibleProvider};
use tt_shared::{
    pricing::{ModelInfo, ModelPricing},
    validate_provider_url, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, Provider, ProviderError, RequestContext,
};

pub use dynamic::DynamicCatalog;

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// 5% BYOK fee applied on top of the underlying model cost.
///
/// Stored in [`CompatConfig::fee_multiplier`]; applied by the billing layer,
/// not at request time. See the module-level documentation for details.
const BYOK_FEE_MULTIPLIER: f64 = 1.05;

/// Error fetching/parsing the dynamic `GET /models` catalogue.
///
/// All variants are non-fatal: the caller logs and ignores them, and the
/// provider keeps serving the previously-cached (or static) catalogue.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// The configured/overridden base URL failed the SSRF guard.
    #[error("blocked models URL: {0}")]
    BlockedUrl(String),
    /// Network-level failure reaching `GET /models`.
    #[error("network error fetching /models: {0}")]
    Network(#[from] reqwest::Error),
    /// `GET /models` returned a non-2xx status.
    #[error("/models returned HTTP {status}")]
    Status { status: u16 },
    /// The response body was not the expected `{ "data": [...] }` JSON.
    #[error("failed to parse /models response: {0}")]
    Parse(#[from] serde_json::Error),
}

/// OpenRouter adapter — delegates to the shared OpenAI-compatible provider base.
///
/// Create once with [`OpenRouterProvider::new`] and share across requests.
/// Optionally [`refresh_models`](Self::refresh_models) to layer the live
/// catalogue over the static baseline.
pub struct OpenRouterProvider {
    inner: OpenAICompatibleProvider,
    /// HTTP client used only for the `GET /models` catalogue fetch. Shares the
    /// same redirect-disabled, rustls conventions as the inner request client.
    models_client: Client,
    /// Default base URL for the catalogue fetch (the same one the inner client
    /// uses for chat requests).
    default_base_url: String,
    /// Whether the SSRF guard allows local/loopback URLs (tests only).
    allow_local: bool,
    /// The live catalogue, empty until the first successful
    /// [`refresh_models`](Self::refresh_models). Consulted before the static
    /// baseline by [`models`](Provider::models) / [`pricing`](Provider::pricing).
    dynamic: Arc<RwLock<DynamicCatalog>>,
}

impl OpenRouterProvider {
    /// Construct a new OpenRouter adapter from the given HTTP client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed.
    pub fn new(client_cfg: ClientConfig) -> Self {
        Self::build(client_cfg, false)
    }

    /// The BYOK fee multiplier for this adapter (always `1.05`).
    ///
    /// Exposed so the billing layer can retrieve it without accessing private
    /// fields. See [`CompatConfig::fee_multiplier`] for semantics.
    pub fn fee_multiplier(&self) -> f64 {
        self.inner.fee_multiplier()
    }

    /// Create an adapter that skips SSRF URL validation for tests targeting a
    /// local mock server.
    ///
    /// # Warning
    ///
    /// Do not use in production code. This bypasses the SSRF guard.
    #[doc(hidden)]
    pub fn new_allow_local(client_cfg: ClientConfig) -> Self {
        Self::build(client_cfg, true)
    }

    fn build(client_cfg: ClientConfig, allow_local: bool) -> Self {
        let cfg = CompatConfig {
            id: "openrouter",
            default_base_url: DEFAULT_BASE_URL.to_string(),
            models: models(),
            pricing_table: pricing_table(),
            fee_multiplier: BYOK_FEE_MULTIPLIER,
            allow_local,
        };
        let models_client = build_client(&client_cfg)
            .unwrap_or_else(|e| panic!("failed to build OpenRouter models HTTP client: {e}"));
        Self {
            inner: OpenAICompatibleProvider::new(client_cfg, cfg),
            models_client,
            default_base_url: DEFAULT_BASE_URL.to_string(),
            allow_local,
            dynamic: Arc::new(RwLock::new(DynamicCatalog::default())),
        }
    }

    /// Fetch the live OpenRouter catalogue from the default `GET /models`
    /// endpoint, parse the model list + per-model pricing, and cache it.
    ///
    /// Call at startup and on a periodic refresh (e.g. a background task). On
    /// success returns the number of models cached. On **any** failure (network,
    /// non-2xx, parse) returns a [`RefreshError`] and leaves the previously
    /// cached (or static baseline) catalogue intact — it never panics and never
    /// clears the cache, so a transient outage cannot break model availability
    /// or pricing.
    pub async fn refresh_models(&self) -> Result<usize, RefreshError> {
        let base = self.default_base_url.clone();
        self.refresh_models_at(&base).await
    }

    /// Like [`refresh_models`](Self::refresh_models) but against an explicit
    /// `base_url` (used by tests' mock server, and by deployments that point
    /// OpenRouter at a custom endpoint). Hits `{base_url}/models`.
    ///
    /// Applies the same SSRF/url-guard convention as the request path:
    /// production instances reject private/loopback URLs; test instances built
    /// with [`new_allow_local`](Self::new_allow_local) permit them.
    #[doc(hidden)]
    pub async fn refresh_models_at(&self, base_url: &str) -> Result<usize, RefreshError> {
        validate_provider_url(base_url, self.allow_local)
            .map_err(|e| RefreshError::BlockedUrl(e.to_string()))?;

        let url = format!("{base_url}/models");
        let response = self.models_client.get(&url).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(RefreshError::Status {
                status: status.as_u16(),
            });
        }

        let body = response.text().await?;
        let catalog = DynamicCatalog::parse(&body, chrono::Utc::now())?;
        let count = catalog.len();

        // Swap the new catalogue in. A poisoned lock (a thread panicked while
        // holding it) is recovered from rather than propagated — the cache is
        // just data and we'd rather overwrite it than poison the provider.
        match self.dynamic.write() {
            Ok(mut guard) => *guard = catalog,
            Err(poisoned) => *poisoned.into_inner() = catalog,
        }
        Ok(count)
    }

    /// Snapshot of the dynamic catalogue (empty until the first successful
    /// refresh). Exposed for diagnostics / tests.
    #[must_use]
    pub fn dynamic_catalog(&self) -> DynamicCatalog {
        match self.dynamic.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    /// Models served by this adapter. Prefers the live catalogue once
    /// [`refresh_models`](Self::refresh_models) has populated it; falls back to
    /// the static baseline otherwise.
    fn models(&self) -> Vec<ModelInfo> {
        let guard = match self.dynamic.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if guard.is_empty() {
            self.inner.models()
        } else {
            guard.models()
        }
    }

    /// Per-model pricing. Prefers the live per-model rates fetched from
    /// `GET /models` (accurate for OpenRouter's full catalogue); falls back to
    /// the static baseline table for the curated subset.
    ///
    /// This is on the request cost path, so it does a single keyed lookup under
    /// a short-lived read lock rather than cloning the whole catalogue.
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let dynamic = match self.dynamic.read() {
            Ok(g) => g.pricing(model),
            Err(p) => p.into_inner().pricing(model),
        };
        dynamic.or_else(|| self.inner.pricing(model))
    }

    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        self.inner.dropped_params(req)
    }

    fn fee_multiplier(&self) -> f64 {
        self.inner.fee_multiplier()
    }

    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.inner.chat_completion(req, ctx).await
    }

    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.inner.chat_completion_stream(req, ctx).await
    }

    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported(
            "openrouter embeddings — not in v1 scope".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Static model catalogue (baseline; superseded by the dynamic fetch)
// ---------------------------------------------------------------------------

fn models() -> Vec<ModelInfo> {
    tt_shared::model_catalog::model_catalog().for_provider("openrouter")
}

/// Build the OpenRouter model→rate map from the shared versioned pricing
/// catalog. These are upstream list prices; OpenRouter's 5% BYOK fee is applied
/// separately via `fee_multiplier`, not baked into the rate. Fed into the
/// OpenAI-compatible inner client at construction.
fn pricing_table() -> HashMap<String, ModelPricing> {
    tt_shared::pricing::catalog()
        .latest_for_provider("openrouter")
        .into_iter()
        .collect()
}
