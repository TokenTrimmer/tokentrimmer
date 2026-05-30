//! API key validation for the Gateway.
//!
//! Keys are stored as `(prefix, argon2_hash)` pairs. Validation: lookup by prefix,
//! verify hash.
//!
//! **Caching:** The `tt-core` gateway layer maintains an in-process TTL cache
//! (keyed by the blake3 hash of the presented token) so argon2 runs at most
//! once per ~60 s per token rather than on every request. Positive results are
//! cached for 60 s; negative results (wrong secret / not-found) for 10 s to
//! mitigate CPU-exhaustion spam. See `crates/core/src/middleware/key_cache.rs`
//! for the revocation-staleness tradeoff documentation.

use uuid::Uuid;

pub mod credentials;
pub mod keys;

/// Postgres-backed [`ProviderCredentialStore`] — gated behind the
/// `postgres` feature so default builds stay free of sqlx and
/// XChaCha20-Poly1305.
#[cfg(feature = "postgres")]
pub mod postgres;
pub use credentials::{
    ChainedProviderCredentialStore, CredentialError, EnvProviderCredentialStore,
    InMemoryProviderCredentialStore, ProviderCredentialStore,
};
pub use keys::{
    issue, revoke_key, verify, ApiKey, Environment, InMemoryKeyStore, IssuedKey, KeyError, KeyStore,
};

/// Context returned after a successful API key verification.
///
/// Carries the minimum identity information needed by downstream middleware.
#[derive(Debug, Clone)]
pub struct ApiKeyContext {
    /// Unique key identifier matching [`ApiKey::id`].
    pub key_id: Uuid,
    /// Organization this key was issued to.
    pub org_id: Uuid,
}
