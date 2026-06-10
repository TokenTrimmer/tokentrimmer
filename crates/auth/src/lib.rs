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

use tt_shared::CallerTier;
use uuid::Uuid;

pub mod credentials;
pub mod keys;

/// Postgres-backed [`ProviderCredentialStore`] — gated behind the
/// `postgres` feature so default builds stay free of sqlx and
/// XChaCha20-Poly1305.
#[cfg(feature = "postgres")]
pub mod postgres;
pub use credentials::{
    env_credential_fallback_opted_in, ChainedProviderCredentialStore, CredentialError,
    EnvProviderCredentialStore, InMemoryProviderCredentialStore, ProviderCredentialStore,
    ALLOW_ENV_CREDENTIAL_FALLBACK_VAR,
};
pub use keys::{
    issue, revoke_key, verify, ApiKey, Environment, InMemoryKeyStore, IssuedKey, KeyError, KeyStore,
};

/// Context returned after a successful API key verification.
///
/// Carries the minimum identity information needed by downstream middleware.
///
/// `tier` is set by the cloud tier-resolution layer (`rv-tier-limits-enforcement`).
/// Until that layer is wired, `tier` is always `None` and the gateway defaults
/// to the conservative 24h cache TTL (spec §8.4). The field is `Option` so
/// the production default is preserved without any code change when the cloud
/// later injects the real tier.
#[derive(Debug, Clone)]
pub struct ApiKeyContext {
    /// Unique key identifier matching [`ApiKey::id`].
    pub key_id: Uuid,
    /// Organization this key was issued to.
    pub org_id: Uuid,
    /// Subscription tier, injected by the cloud tier-resolution layer once
    /// `rv-tier-limits-enforcement` is wired. `None` → 24h TTL default.
    pub tier: Option<CallerTier>,
}
