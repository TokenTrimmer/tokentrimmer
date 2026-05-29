//! API key validation for the Gateway.
//!
//! Keys are stored as `(prefix, argon2_hash)` pairs. Validation: lookup by prefix,
//! verify hash. Results cached in Redis for 60s by the core layer.

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
