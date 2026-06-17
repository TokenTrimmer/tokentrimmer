//! [`AuditWriter`] trait and [`InMemoryAuditWriter`] implementation.
//!
//! Storage backends implement [`AuditWriter`]. The in-memory writer is provided
//! for testing and local CLI demos; a Postgres-backed writer ships in Week 7.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use uuid::Uuid;

use super::{compute_hash, Actor, AuditEntry, AuditError, PayloadFields};

// ─── Free functions ────────────────────────────────────────────────────────────

/// Generate a fresh Ed25519 signing key (OS RNG). Kept here so callers don't
/// need a direct `rand_core` dependency.
pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut rand_core::OsRng)
}

/// Build (hash + Ed25519-sign) a new audit entry chaining onto `prev` (or
/// genesis when `prev` is `None`). Shared by every writer so the chain rules
/// live in exactly one place.
pub fn build_entry(
    signing_key: &SigningKey,
    prev: Option<&AuditEntry>,
    org_id: uuid::Uuid,
    actor: Actor,
    event: String,
    payload: serde_json::Value,
) -> Result<AuditEntry, AuditError> {
    let seq = prev.map_or(0, |p| p.seq + 1);
    let (prev_hash_str, prev_hash_bytes): (String, [u8; 32]) = match prev {
        Some(p) => {
            let decoded = hex::decode(&p.hash).map_err(|e| AuditError::Storage(e.to_string()))?;
            if decoded.len() != 32 {
                return Err(AuditError::Storage(format!(
                    "prev hash decoded to {} bytes, expected 32",
                    decoded.len()
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&decoded);
            (p.hash.clone(), arr)
        }
        None => {
            let zeroes = [0u8; 32];
            (hex::encode(zeroes), zeroes)
        }
    };

    let id = uuid::Uuid::new_v4();
    let timestamp = chrono::Utc::now();
    let fields = PayloadFields {
        id,
        org_id,
        timestamp,
        actor: &actor,
        event: &event,
        payload: &payload,
        seq,
    };
    let hash = compute_hash(&prev_hash_bytes, &fields)?;
    let hash_hex = hash.to_hex().to_string();
    let signature = signing_key
        .try_sign(hash.as_bytes())
        .map_err(|e| AuditError::Signing(e.to_string()))?;
    let signature_hex = hex::encode(signature.to_bytes());

    Ok(AuditEntry {
        id,
        org_id,
        seq,
        timestamp,
        actor,
        event,
        payload,
        prev_hash: prev_hash_str,
        hash: hash_hex,
        signature: signature_hex,
    })
}

// ─── Trait ────────────────────────────────────────────────────────────────────

/// Storage backend for the audit log.
///
/// Implementations are responsible for atomic `prev_hash` lookup + append to
/// prevent races. The in-memory writer uses a `Mutex`; Postgres uses a
/// serializable transaction.
#[async_trait]
pub trait AuditWriter: Send + Sync {
    /// Append a new entry to the chain, returning the constructed [`AuditEntry`].
    async fn write(
        &self,
        org_id: Uuid,
        actor: Actor,
        event: String,
        payload: serde_json::Value,
    ) -> Result<AuditEntry, AuditError>;

    /// Return all entries for `org_id` in insertion order (genesis first).
    async fn list(&self, org_id: Uuid) -> Result<Vec<AuditEntry>, AuditError>;
}

// ─── InMemoryAuditWriter ──────────────────────────────────────────────────────

/// In-process, non-persistent audit writer.
///
/// All chains are held in a `HashMap` keyed by `org_id`. A `std::sync::Mutex`
/// guards the map; lock duration is microseconds so async wakeup overhead would
/// dominate — a regular Mutex is the right call here.
///
/// This implementation is suitable for tests, CLI smoke-testing, and
/// single-process deployments where durability is not required.
pub struct InMemoryAuditWriter {
    signing_key: SigningKey,
    chains: Arc<Mutex<HashMap<Uuid, Vec<AuditEntry>>>>,
}

impl InMemoryAuditWriter {
    /// Create with a freshly generated Ed25519 signing key.
    pub fn new() -> Self {
        let signing_key = SigningKey::generate(&mut rand_core::OsRng);
        Self::with_key(signing_key)
    }

    /// Create with a provided signing key (useful for restoring state or tests
    /// that need to verify signatures against a known key).
    pub fn with_key(signing_key: SigningKey) -> Self {
        Self {
            signing_key,
            chains: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Return the Ed25519 verifying (public) key so callers can run
    /// [`super::verify_chain`] without holding a reference to the writer.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }
}

impl Default for InMemoryAuditWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AuditWriter for InMemoryAuditWriter {
    async fn write(
        &self,
        org_id: Uuid,
        actor: Actor,
        event: String,
        payload: serde_json::Value,
    ) -> Result<AuditEntry, AuditError> {
        // Lock once; all hash-chain state is derived inside the lock to prevent
        // races between concurrent `write` calls for the same org.
        let mut guard = self
            .chains
            .lock()
            .map_err(|_| AuditError::Storage("mutex poisoned".to_string()))?;

        let chain = guard.entry(org_id).or_default();
        let entry = build_entry(
            &self.signing_key,
            chain.last(),
            org_id,
            actor,
            event,
            payload,
        )?;
        chain.push(entry.clone());
        Ok(entry)
    }

    async fn list(&self, org_id: Uuid) -> Result<Vec<AuditEntry>, AuditError> {
        let guard = self
            .chains
            .lock()
            .map_err(|_| AuditError::Storage("mutex poisoned".to_string()))?;
        Ok(guard.get(&org_id).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::Actor;

    #[test]
    fn build_entry_chains_and_verifies() {
        let key = super::generate_signing_key();
        let org = uuid::Uuid::new_v4();

        let g = super::build_entry(
            &key,
            None,
            org,
            Actor::System,
            "genesis".into(),
            serde_json::json!({}),
        )
        .expect("genesis");
        assert_eq!(g.seq, 0);
        assert_eq!(g.prev_hash, "0".repeat(64));

        let next = super::build_entry(
            &key,
            Some(&g),
            org,
            Actor::System,
            "plan.applied".into(),
            serde_json::json!({"k":"v"}),
        )
        .expect("next");
        assert_eq!(next.seq, 1);
        assert_eq!(next.prev_hash, g.hash);

        super::super::verify_chain(&[g, next], &key.verifying_key()).expect("chain verifies");
    }
}
