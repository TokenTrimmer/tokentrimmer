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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tt_shared::{
    context::ProviderCredentials,
    messages::{EmbeddingInput, EmbeddingsRequest},
    Provider, ProviderError,
};
use unicode_normalization::UnicodeNormalization as _;

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
            budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
            trace_id: Uuid::new_v4(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: self.credentials.clone(),
            tag: None,
            deadline: None,
            run_id: None,
            node_id: None,
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

// ---------------------------------------------------------------------------
// MemoizingEmbedder — in-process LRU memoization (COST-4)
// ---------------------------------------------------------------------------

/// Default capacity of the in-process embedding memoization cache. A few
/// hundred entries is enough to absorb the dominant repeated-prompt case
/// (identical context text re-embedded turn after turn) while staying cheap
/// (each `text-embedding-3-small` vector is ~6 KiB, so 512 entries ≈ 3 MiB).
pub const DEFAULT_EMBED_CACHE_CAPACITY: usize = 512;

/// Normalize embedding input for cache identity (NFC + trailing-whitespace
/// trim), mirroring the L1 cache-key normalization in [`crate::key`]. Two
/// inputs that differ only in Unicode encoding form or trailing whitespace
/// share one cached vector — the same cache-identity contract the gateway
/// already applies at the L1 key layer.
fn normalize_embed_input(text: &str) -> String {
    let nfc: String = text.nfc().collect();
    nfc.trim_end_matches([' ', '\t', '\n', '\r']).to_owned()
}

/// 32-byte SHA-256 content key of the normalized embedding input. Collision-
/// resistant so distinct prompts never share a cached vector.
fn embed_cache_key(text: &str) -> [u8; 32] {
    Sha256::digest(normalize_embed_input(text).as_bytes()).into()
}

/// A bounded least-recently-used map from a content key to a cached embedding
/// vector. Eviction targets the entry with the smallest access tick (true LRU);
/// the O(n) victim scan runs only when an insert overflows the bound, which is
/// negligible at the few-hundred-entry scale this cache operates at.
struct EmbedLru {
    cap: usize,
    clock: u64,
    map: HashMap<[u8; 32], (Vec<f32>, u64)>,
}

impl EmbedLru {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            clock: 0,
            map: HashMap::new(),
        }
    }

    /// Return a clone of the cached vector for `key`, marking it most-recently
    /// used. `None` on a miss.
    fn get(&mut self, key: &[u8; 32]) -> Option<Vec<f32>> {
        self.clock += 1;
        let tick = self.clock;
        let entry = self.map.get_mut(key)?;
        entry.1 = tick;
        Some(entry.0.clone())
    }

    /// Insert `value` under `key`, evicting the least-recently-used entry first
    /// when the bound would be exceeded by a new key.
    fn put(&mut self, key: [u8; 32], value: Vec<f32>) {
        self.clock += 1;
        let tick = self.clock;
        if !self.map.contains_key(&key) && self.map.len() >= self.cap {
            if let Some(victim) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| *k)
            {
                self.map.remove(&victim);
            }
        }
        self.map.insert(key, (value, tick));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

/// An [`EmbeddingProvider`] that wraps another provider with a bounded,
/// thread-safe, in-process LRU memoization layer keyed on a hash of the
/// normalized input text.
///
/// Repeated identical text — the dominant case inside a loop or a hot prompt —
/// reuses the one already-computed vector instead of re-calling the embedding
/// API, cutting both latency and embedding spend. The wrapper preserves the
/// public [`EmbeddingProvider`] contract verbatim (same `embed` signature, same
/// `model()`), so it is a drop-in around an existing embedder (e.g.
/// `MemoizingEmbedder::new(Arc::new(OpenAIEmbedder::new(..)))`).
///
/// The cache is scoped to the wrapped embedder, so every entry belongs to one
/// embedding model — the key need not include the model name.
///
/// # Example
///
/// ```rust
/// use std::sync::Arc;
/// use tt_cache::embed::{EmbeddingProvider, MemoizingEmbedder, MockEmbedder};
///
/// # #[tokio::main]
/// # async fn main() {
/// let inner = Arc::new(MockEmbedder { fixed_vec: vec![1.0, 0.0], model: "m".into() });
/// let embedder = MemoizingEmbedder::new(inner);
/// let a = embedder.embed("hello").await.unwrap();
/// let b = embedder.embed("hello").await.unwrap(); // served from cache
/// assert_eq!(a, b);
/// # }
/// ```
pub struct MemoizingEmbedder {
    inner: Arc<dyn EmbeddingProvider>,
    cache: Mutex<EmbedLru>,
}

impl MemoizingEmbedder {
    /// Wrap `inner` with a memoization cache of [`DEFAULT_EMBED_CACHE_CAPACITY`]
    /// entries.
    #[must_use]
    pub fn new(inner: Arc<dyn EmbeddingProvider>) -> Self {
        Self::with_capacity(inner, DEFAULT_EMBED_CACHE_CAPACITY)
    }

    /// Wrap `inner` with a memoization cache bounded to `capacity` entries
    /// (floored at 1).
    #[must_use]
    pub fn with_capacity(inner: Arc<dyn EmbeddingProvider>, capacity: usize) -> Self {
        Self {
            inner,
            cache: Mutex::new(EmbedLru::new(capacity)),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for MemoizingEmbedder {
    /// Return the memoized vector for `text` when present; otherwise call the
    /// wrapped embedder and cache the result.
    ///
    /// The cache lock is never held across the `.await`: a miss releases the
    /// lock, embeds, then re-acquires to store. Two concurrent misses for the
    /// same text may both call the inner embedder (no single-flight) — correct,
    /// since they compute the same vector — but the common sequential
    /// repeated-prompt path calls the inner embedder exactly once.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let key = embed_cache_key(text);
        if let Some(hit) = self
            .cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
        {
            return Ok(hit);
        }
        let vector = self.inner.embed(text).await?;
        self.cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .put(key, vector.clone());
        Ok(vector)
    }

    fn model(&self) -> &str {
        self.inner.model()
    }
}

#[cfg(test)]
mod memoize_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An embedder that counts its underlying calls and returns a deterministic
    /// length-keyed vector, so a test can prove the memoizer suppressed a call.
    struct CountingEmbedder {
        calls: AtomicUsize,
        model: String,
    }

    impl CountingEmbedder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                model: "mock-v1".to_string(),
            })
        }
        fn count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl EmbeddingProvider for CountingEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![text.len() as f32, 1.0])
        }
        fn model(&self) -> &str {
            &self.model
        }
    }

    /// Repeated identical text calls the underlying embedder exactly once; a
    /// distinct text calls it again.
    #[tokio::test]
    async fn repeated_text_calls_inner_once() {
        let counter = CountingEmbedder::new();
        let mem = MemoizingEmbedder::new(counter.clone());

        let v1 = mem.embed("hello world").await.unwrap();
        let v2 = mem.embed("hello world").await.unwrap();
        assert_eq!(v1, v2, "the cached vector must match the original");
        assert_eq!(
            counter.count(),
            1,
            "identical text must hit the cache (one underlying embed call)"
        );

        let _ = mem.embed("a different prompt").await.unwrap();
        assert_eq!(
            counter.count(),
            2,
            "distinct text must miss the cache and call the inner embedder"
        );
        // model() is forwarded from the wrapped embedder.
        assert_eq!(mem.model(), "mock-v1");
    }

    /// Inputs differing only by Unicode form / trailing whitespace normalize to
    /// one cache entry (same cache-identity contract as the L1 key layer).
    #[tokio::test]
    async fn trailing_whitespace_and_nfc_share_one_entry() {
        let counter = CountingEmbedder::new();
        let mem = MemoizingEmbedder::new(counter.clone());

        let first = mem.embed("same text").await.unwrap();
        let second = mem.embed("same text\n  ").await.unwrap();
        assert_eq!(
            counter.count(),
            1,
            "trailing-whitespace-only variant must reuse the cached vector"
        );
        assert_eq!(
            first, second,
            "the normalized variant must return the first cached vector"
        );
    }

    /// LRU eviction: filling past capacity evicts the least-recently-used entry;
    /// a recently-touched entry survives.
    #[tokio::test]
    async fn eviction_drops_least_recently_used() {
        let counter = CountingEmbedder::new();
        let mem = MemoizingEmbedder::with_capacity(counter.clone(), 2);

        mem.embed("A").await.unwrap(); // calls=1, cache {A}
        mem.embed("B").await.unwrap(); // calls=2, cache {A,B}
        mem.embed("A").await.unwrap(); // hit, A now most-recently-used (B is LRU)
        assert_eq!(counter.count(), 2, "A must be served from cache");

        // Inserting C overflows cap=2 → evict the LRU entry (B), keeping A.
        mem.embed("C").await.unwrap(); // calls=3, cache {A,C}
        assert_eq!(counter.count(), 3);

        // A survived the C-insert eviction → still a cache hit (no new call).
        mem.embed("A").await.unwrap();
        assert_eq!(
            counter.count(),
            3,
            "the recently-used entry must survive eviction"
        );
        // C is also present.
        mem.embed("C").await.unwrap();
        assert_eq!(counter.count(), 3, "the just-inserted entry is cached");

        // B was the evicted victim → re-embedding it calls the inner embedder.
        mem.embed("B").await.unwrap(); // calls=4
        assert_eq!(counter.count(), 4, "the evicted entry must be recomputed");
    }

    /// The bound is honored: the cache never exceeds its capacity.
    #[tokio::test]
    async fn cache_never_exceeds_capacity() {
        let counter = CountingEmbedder::new();
        let mem = MemoizingEmbedder::with_capacity(counter.clone(), 3);
        for i in 0..50 {
            mem.embed(&format!("prompt-{i}")).await.unwrap();
        }
        let len = mem.cache.lock().unwrap().len();
        assert_eq!(len, 3, "the cache must stay within its bound");
    }
}
