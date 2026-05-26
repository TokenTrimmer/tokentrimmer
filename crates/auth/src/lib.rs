//! API key validation for the Gateway.
//!
//! Keys are stored as `(prefix, argon2_hash)` pairs. Validation: lookup by prefix,
//! verify hash. Results cached in Redis for 60s by the core layer.

use uuid::Uuid;

pub mod keys;
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
