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
pub mod l2;
pub mod memory;
pub mod redis_impl;

// Re-export key L2 types at the crate root for convenience.
pub use embed::{EmbedError, EmbeddingProvider, MockEmbedder, OpenAIEmbedder};
pub use l2::{CacheEntry, InMemoryL2Cache, L2Cache, PostgresL2Cache};

use async_trait::async_trait;
use thiserror::Error;

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
}
