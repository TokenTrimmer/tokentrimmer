//! Embedding abstraction for the L2 semantic cache.
//!
//! [`EmbeddingProvider`] is a thin async trait that converts a text string into
//! a floating-point vector. Two implementations are provided:
//!
//! - [`OpenAIEmbedder`] — wraps any [`tt_shared::Provider`] that supports the
//!   `embeddings()` call (e.g. [`tt_provider_openai::OpenAiProvider`]) and uses
//!   the `text-embedding-3-small` model by default (ADR-008 §v1).
//! - [`MockEmbedder`] — returns a fixed, pre-configured vector regardless of
//!   input, for use in unit tests without any I/O.
//!
//! # Swap path
//!
//! Because all callers depend only on the `EmbeddingProvider` trait, the
//! underlying embedding model can be changed without touching the L2 cache
//! logic: instantiate a different [`OpenAIEmbedder`] (e.g. with
//! `text-embedding-3-large`) or provide a custom `impl EmbeddingProvider`.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tt_shared::{
    context::ProviderCredentials,
    messages::{EmbeddingInput, EmbeddingsRequest},
    Provider, ProviderError,
};

// ---------------------------------------------------------------------------
// EmbedError
// ---------------------------------------------------------------------------

/// Errors returned by [`EmbeddingProvider::embed`].
#[derive(Debug, Error)]
pub enum EmbedError {
    /// The underlying provider returned an error.
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    /// The provider returned an empty embedding vector.
    #[error("empty embedding returned by provider")]
    EmptyInput,
}

// ---------------------------------------------------------------------------
// EmbeddingProvider trait
// ---------------------------------------------------------------------------

/// Embed a text string into a dense floating-point vector.
///
/// Implementations are expected to return L2-normalized vectors (as OpenAI
/// `text-embedding-3` models do), but the general cosine-similarity formula
/// in [`crate::l2`] does not assume normalization.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a single text string.
    ///
    /// Returns a vector whose length matches the dimensionality of the
    /// underlying model (e.g. 1536 for `text-embedding-3-small`).
    ///
    /// # Errors
    ///
    /// Returns [`EmbedError::Provider`] if the upstream provider fails, or
    /// [`EmbedError::EmptyInput`] if the provider returns an empty vector.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;

    /// Stable identifier for the embedding model.
    ///
    /// Used in `cache_entries.model` to ensure that similarity searches are
    /// scoped to entries produced by the same embedding space.
    fn model(&self) -> &str;
}

// ---------------------------------------------------------------------------
// OpenAIEmbedder
// ---------------------------------------------------------------------------

/// An [`EmbeddingProvider`] backed by any [`Provider`] that supports the
/// `embeddings()` call.
///
/// Construct with [`OpenAIEmbedder::new`], passing an `Arc<dyn Provider>` and
/// the desired embedding model name (e.g. `"text-embedding-3-small"`).
pub struct OpenAIEmbedder {
    provider: Arc<dyn Provider>,
    model_name: String,
    credentials: ProviderCredentials,
}

impl OpenAIEmbedder {
    /// Create a new embedder wrapping `provider`.
    ///
    /// `model` is the embedding model to request from the provider
    /// (e.g. `"text-embedding-3-small"`).
    /// `credentials` are forwarded verbatim to the provider on every call.
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        credentials: ProviderCredentials,
    ) -> Self {
        Self {
            provider,
            model_name: model.into(),
            credentials,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAIEmbedder {
    /// Embed `text` by calling `provider.embeddings()` with a single-input request.
    ///
    /// # Errors
    ///
    /// - [`EmbedError::Provider`] if the provider call fails.
    /// - [`EmbedError::EmptyInput`] if the response contains no embedding data.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        use tt_shared::context::RequestContext;
        use uuid::Uuid;

        let ctx = RequestContext {
            trace_id: Uuid::new_v4(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: self.credentials.clone(),
            tag: None,
            deadline: None,
        };

        let req = EmbeddingsRequest {
            model: self.model_name.clone(),
            input: EmbeddingInput::Single(text.to_owned()),
            dimensions: None,
            encoding_format: None,
        };

        let resp = self.provider.embeddings(req, &ctx).await?;

        resp.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .filter(|v| !v.is_empty())
            .ok_or(EmbedError::EmptyInput)
    }

    fn model(&self) -> &str {
        &self.model_name
    }
}

// ---------------------------------------------------------------------------
// MockEmbedder
// ---------------------------------------------------------------------------

/// A deterministic [`EmbeddingProvider`] for unit tests.
///
/// Returns [`MockEmbedder::fixed_vec`] for every call to [`embed`], regardless
/// of the input text. This makes tests fully deterministic without any I/O.
///
/// # Example
///
/// ```rust
/// use tt_cache::embed::{EmbeddingProvider, MockEmbedder};
///
/// # #[tokio::main]
/// # async fn main() {
/// let embedder = MockEmbedder {
///     fixed_vec: vec![1.0, 0.0, 0.0],
///     model: "mock-v1".to_string(),
/// };
/// let v = embedder.embed("any text at all").await.unwrap();
/// assert_eq!(v, vec![1.0, 0.0, 0.0]);
/// # }
/// ```
pub struct MockEmbedder {
    /// The vector returned by every `embed()` call.
    pub fixed_vec: Vec<f32>,
    /// Identifier returned by [`EmbeddingProvider::model`].
    pub model: String,
}

#[async_trait]
impl EmbeddingProvider for MockEmbedder {
    /// Always returns `self.fixed_vec`, ignoring `text`.
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self.fixed_vec.clone())
    }

    fn model(&self) -> &str {
        &self.model
    }
}
