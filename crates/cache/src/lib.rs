//! Cache layer — L1 (Redis exact-match) and L2 (pgvector semantic) implementations.
//!
//! # Modules
//!
//! - [`key`] — SHA-256 cache-key derivation from a normalized [`ChatCompletionRequest`].
//! - [`redis_impl`] — [`RedisL1Cache`]: production Redis-backed L1 implementation.
//! - [`memory`] — [`InMemoryL1Cache`]: in-memory L1 implementation for unit tests.
//! - [`embed`] — [`EmbeddingProvider`] trait, [`OpenAIEmbedder`], and [`MockEmbedder`]
//!   for the L2 embedding pipeline.
//! - [`l2`] — [`L2Cache`] trait, [`InMemoryL2Cache`] (tests), and [`PostgresL2Cache`]
//!   (production, pgvector HNSW).
//! - [`response_codec`] — optional at-rest encryption ([`ResponseCodec`]) for the
//!   verbatim provider responses both tiers cache (SEC-2).
//!
//! # At-rest posture for cached responses (SEC-2)
//!
//! Cached responses are **verbatim LLM output** and routinely carry the same
//! PII / secrets the opt-in body-capture feature already encrypts. By default
//! both tiers store the response in **plaintext** (the L2 `cache_entries.response`
//! JSONB; the L1 [`L1Entry`] envelope in Redis) — back-compatible with every
//! existing cache. Wiring a [`ResponseCodec`] via the `with_response_codec(...)`
//! builder on [`PostgresL2Cache`] / [`InMemoryL2Cache`] / [`RedisL1Cache`] /
//! [`InMemoryL1Cache`] switches that tier to **encrypted-at-rest** (per-org
//! XChaCha20-Poly1305, key derived from `TT_MASTER_KEY`); the embedding vector
//! stays plaintext so similarity search is unaffected. Legacy plaintext rows
//! stay readable (fail-open), so the codec can be enabled on a live cache. See
//! [`response_codec`] for the format and the back-compat contract.
//!
//! [`ChatCompletionRequest`]: tt_shared::messages::ChatCompletionRequest
//! [`RedisL1Cache`]: redis_impl::RedisL1Cache
//! [`InMemoryL1Cache`]: memory::InMemoryL1Cache
//! [`EmbeddingProvider`]: embed::EmbeddingProvider
//! [`OpenAIEmbedder`]: embed::OpenAIEmbedder
//! [`MockEmbedder`]: embed::MockEmbedder
//! [`L2Cache`]: l2::L2Cache
//! [`InMemoryL2Cache`]: l2::InMemoryL2Cache
//! [`PostgresL2Cache`]: l2::PostgresL2Cache

pub mod embed;
pub mod key;
pub mod l1_entry;
pub mod l2;
pub mod lexical;
pub mod memory;
pub mod rate_limit;
pub mod redis_impl;
pub mod response_codec;

// Re-export key L2 types at the crate root for convenience.
pub use embed::{
    EmbedError, EmbeddingProvider, MemoizingEmbedder, MockEmbedder, OpenAIEmbedder,
    DEFAULT_EMBED_CACHE_CAPACITY,
};
pub use key::{
    cache_key, cache_key_with, cache_key_with_policy, AliasMapCanonicalizer, IdentityCanonicalizer,
    ModelCanonicalizer,
};
pub use l1_entry::L1Entry;
pub use l2::{
    class_threshold_for, dedup_collapse, l2_context_text, l2_tool_context_text,
    AdaptiveClassThresholds, CacheEntry, ClassThresholds, DedupActionReport, DedupCluster,
    DedupReport, FpGateTuning, InMemoryL2Cache, JudgeBand, JudgeRecordOutcome, L2Cache,
    PostgresL2Cache, TaskClass, ADAPTIVE_THRESHOLD_CEILING, DEFAULT_THRESHOLD,
};
pub use lexical::{
    lexical_agreement, lexical_sig, lexical_verify_decision, LexicalVerifyConfig,
    LexicalVerifyDecision, DEFAULT_LEXICAL_MIN_AGREEMENT, DEFAULT_VERIFY_EPSILON,
};
pub use rate_limit::{RateLimitDecision, RedisRateLimiter};
pub use response_codec::{org_from_l1_key, L1Open, L2Open, ResponseCodec, ResponseCodecError};

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

/// Errors returned by cache operations.
#[derive(Debug, Error)]
pub enum CacheError {
    /// An error originating from the Redis client.
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    /// An error during JSON serialization / deserialization.
    #[error("serde: {0}")]
    Serde(serde_json::Error),
    /// An error originating from the sqlx database client (L2 Postgres cache).
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// The embedding vector contained a non-finite (NaN/Inf) component. Rejected
    /// at insert so a corrupt vector can't poison similarity ranking.
    #[error("invalid embedding: non-finite component")]
    InvalidEmbedding,
    /// At-rest response encryption / decryption failed (a wired
    /// [`ResponseCodec`](crate::ResponseCodec) could not seal a response on
    /// insert). Decryption failures on the READ path never surface here — an
    /// unreadable encrypted row is skipped as a cache miss, not an error.
    #[error("response codec: {0}")]
    ResponseCodec(#[from] crate::response_codec::ResponseCodecError),
}

/// Progress from one bounded organization-erasure pass.
///
/// Redis implementations may need several passes for a very large tenant;
/// callers must retain their durable cleanup task until `complete` is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L1PurgeResult {
    /// Every indexed key was removed while the organization write fence held.
    pub complete: bool,
    /// Number of indexed values removed by this pass.
    pub deleted: usize,
}

/// L1 exact-match cache contract. Keys are SHA-256 hashes of normalized requests.
#[async_trait]
pub trait L1Cache: Send + Sync {
    /// Retrieve a cached value by its key, returning `None` if absent or expired.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError>;
    /// Store a value under `key` with the given TTL in seconds.
    async fn set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), CacheError>;
    /// Remove a key from the cache.
    async fn delete(&self, key: &str) -> Result<(), CacheError>;
    /// Fence future writes and remove one organization's indexed values.
    async fn purge_org(&self, org_id: Uuid) -> Result<L1PurgeResult, CacheError>;
}
