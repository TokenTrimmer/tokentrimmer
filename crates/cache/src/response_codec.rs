//! Optional at-rest encryption for cached LLM responses (SEC-2).
//!
//! Cache entries store **verbatim provider responses** — the same prose that
//! routinely contains the PII / secrets the opt-in body-capture feature already
//! protects ([`tt_telemetry::body_capture`]). By default both cache tiers store
//! those responses in plaintext:
//!
//! - the **L2** semantic cache binds the response into the `cache_entries.response`
//!   JSONB column ([`crate::l2::PostgresL2Cache`]);
//! - the **L1** exact-match cache serializes the whole [`crate::L1Entry`] envelope
//!   (response included) into Redis ([`crate::redis_impl::RedisL1Cache`]).
//!
//! [`ResponseCodec`] is the at-rest encryption primitive both tiers can opt into
//! via a `with_response_codec(...)` builder. It is a self-contained equivalent of
//! [`tt_telemetry::body_capture::BodyCaptureCodec`] (same XChaCha20-Poly1305 AEAD,
//! same per-org-key-derived-from-`TT_MASTER_KEY` shape) kept inside `tt-cache` so
//! the low-level cache crate does not take a dependency on the heavy telemetry
//! crate. A **distinct KDF domain + AAD magic** give it independent per-org keys:
//! a body-capture blob and a cache blob derived from the same master key can
//! never be cross-decrypted.
//!
//! # What stays plaintext
//!
//! Only the **response payload** is encrypted. The L2 **embedding vector stays
//! plaintext** — cosine similarity must still work — as do the embedding model,
//! token counts, costs, and the 64-bit lexical sketch. The encrypted blob is the
//! same data the verbatim response already exposed; the metadata columns were
//! never the sensitive surface.
//!
//! # Back-compat / fail-open on legacy plaintext
//!
//! Both seal formats are self-describing, so a codec-enabled cache reads a
//! pre-codec **plaintext** row transparently (it is not an envelope → returned
//! as-is). A blob that IS an envelope but fails to decrypt (wrong key / wrong
//! org) is treated as a cache MISS rather than served as garbage — the lookup
//! skips it. Turning the codec OFF again leaves legacy plaintext readable and
//! simply stops decrypting (encrypted rows become misses until they expire).

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng, Payload},
    AeadCore, XChaCha20Poly1305, XNonce,
};

/// XChaCha20-Poly1305 nonce length (bytes).
const NONCE_LEN: usize = 24;

/// Per-org key-derivation domain. **Distinct** from body-capture's domain so a
/// cache key and a body-capture key derived from the same `TT_MASTER_KEY` are
/// independent (no cross-decryption between the two stores).
const KDF_DOMAIN: &[u8] = b"|tt-cache:response:key:v1|";

/// AAD magic bound into every cache ciphertext (domain separation in the AEAD
/// associated data, mirroring body-capture's `AAD_MAGIC`).
const AAD_MAGIC: &[u8] = b"tt-cache:response:v1";

/// Leading bytes of the **L1** binary envelope (`MAGIC || nonce || ciphertext`).
/// A legacy plaintext L1 value is a JSON-serialized [`crate::L1Entry`], which
/// always begins with `{` (`0x7b`); the leading `0x00` here can never collide,
/// so a reader distinguishes an envelope from legacy bytes by this prefix alone.
const L1_ENVELOPE_MAGIC: &[u8] = b"\x00tt-l1-enc:v1\x00";

/// JSON key marking the **L2** envelope object. A legacy plaintext response is a
/// `ChatCompletionResponse` object, which never carries this key, so a reader
/// distinguishes an envelope from a legacy row by its presence alone.
const L2_ENVELOPE_KEY: &str = "__tt_cache_enc__";

/// Errors from the cache response codec.
#[derive(Debug, thiserror::Error)]
pub enum ResponseCodecError {
    /// `TT_MASTER_KEY` was present but not 32 hex-encoded bytes.
    #[error("invalid TT_MASTER_KEY: {0}")]
    BadMasterKey(String),
    /// Encryption or authenticated decryption failed.
    #[error("cache response crypto failed")]
    Crypto,
    /// A stored blob was too short / structurally malformed to be a ciphertext.
    #[error("cache response blob is malformed")]
    Malformed,
}

/// Per-org at-rest encryption for cached responses. Cheap to clone; holds only
/// the 32-byte master key. See the module docs for the at-rest posture.
#[derive(Clone)]
pub struct ResponseCodec {
    master_key: [u8; 32],
}

impl std::fmt::Debug for ResponseCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseCodec")
            .field("master_key", &"[REDACTED]")
            .finish()
    }
}

impl ResponseCodec {
    /// Build from a raw 32-byte master key.
    #[must_use]
    pub fn new(master_key: [u8; 32]) -> Self {
        Self { master_key }
    }

    /// Build from `TT_MASTER_KEY` (the same root key body-capture uses). Missing
    /// means at-rest cache encryption is disabled (`Ok(None)` — today's plaintext
    /// behavior); malformed means an operator tried to enable it with an unusable
    /// root key (`Err`). The derived per-org cache key uses a cache-specific KDF
    /// domain, so it is independent of the body-capture key for the same org.
    pub fn from_env() -> Result<Option<Self>, ResponseCodecError> {
        let Ok(hex_key) = std::env::var("TT_MASTER_KEY") else {
            return Ok(None);
        };
        let bytes = hex::decode(hex_key.trim()).map_err(|_| {
            ResponseCodecError::BadMasterKey("expected 32 hex-encoded bytes".into())
        })?;
        let master_key: [u8; 32] = bytes.try_into().map_err(|_| {
            ResponseCodecError::BadMasterKey("expected 32 hex-encoded bytes".into())
        })?;
        Ok(Some(Self { master_key }))
    }

    // -- Low-level AEAD ------------------------------------------------------

    fn derive_key(&self, org_id: Uuid) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.master_key);
        h.update(KDF_DOMAIN);
        h.update(org_id.as_bytes());
        h.finalize().into()
    }

    /// Associated data binds the org and a caller-supplied context (the L2 row id
    /// or the L1 cache key) so a sealed blob cannot be replayed under a different
    /// org or row.
    fn aad(org_id: Uuid, context: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(AAD_MAGIC.len() + 16 + context.len());
        out.extend_from_slice(AAD_MAGIC);
        out.extend_from_slice(org_id.as_bytes());
        out.extend_from_slice(context);
        out
    }

    /// Encrypt `plain` under the per-org key, returning `nonce || ciphertext`.
    fn seal_raw(
        &self,
        org_id: Uuid,
        context: &[u8],
        plain: &[u8],
    ) -> Result<Vec<u8>, ResponseCodecError> {
        let key = self.derive_key(org_id);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ad = Self::aad(org_id, context);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plain,
                    aad: &ad,
                },
            )
            .map_err(|_| ResponseCodecError::Crypto)?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypt a `nonce || ciphertext` blob produced by [`Self::seal_raw`].
    fn open_raw(
        &self,
        org_id: Uuid,
        context: &[u8],
        blob: &[u8],
    ) -> Result<Vec<u8>, ResponseCodecError> {
        if blob.len() < NONCE_LEN {
            return Err(ResponseCodecError::Malformed);
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = XNonce::from_slice(nonce_bytes);
        let key = self.derive_key(org_id);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let ad = Self::aad(org_id, context);
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: &ad,
                },
            )
            .map_err(|_| ResponseCodecError::Crypto)
    }

    // -- L2 JSONB envelope ---------------------------------------------------

    /// Seal a response payload for the L2 `response` JSONB column. The returned
    /// [`Value`] is a self-describing envelope object `{ __tt_cache_enc__, blob }`
    /// (the blob is hex of `nonce || ciphertext`). AAD binds the org and the row
    /// `id`.
    pub fn seal_response_json(
        &self,
        org_id: Uuid,
        id: Uuid,
        plain: &[u8],
    ) -> Result<Value, ResponseCodecError> {
        let blob = self.seal_raw(org_id, id.as_bytes(), plain)?;
        Ok(json!({
            L2_ENVELOPE_KEY: 1,
            "blob": hex::encode(blob),
        }))
    }

    /// Outcome of inspecting an L2 `response` JSONB value with a codec.
    /// [`Plaintext`](L2Open::Plaintext) means it is not our envelope (a legacy
    /// row) — use the value as-is; [`Decrypted`](L2Open::Decrypted) carries the
    /// recovered plaintext; [`Undecryptable`](L2Open::Undecryptable) means it IS
    /// our envelope but did not authenticate (wrong key/org/row) — skip it.
    pub fn open_response_json(&self, org_id: Uuid, id: Uuid, value: &Value) -> L2Open {
        let Some(hex_blob) = value
            .as_object()
            .filter(|o| o.contains_key(L2_ENVELOPE_KEY))
            .and_then(|o| o.get("blob"))
            .and_then(Value::as_str)
        else {
            // Not our envelope → a legacy plaintext response row.
            return L2Open::Plaintext;
        };
        let Ok(blob) = hex::decode(hex_blob) else {
            return L2Open::Undecryptable;
        };
        match self.open_raw(org_id, id.as_bytes(), &blob) {
            Ok(plain) => L2Open::Decrypted(plain),
            Err(_) => L2Open::Undecryptable,
        }
    }

    /// `true` when `value` is a cache-response encryption envelope (regardless of
    /// whether it would decrypt). Lets a codec-less reader detect that a row is
    /// encrypted (and therefore unservable) without holding a key.
    #[must_use]
    pub fn is_encrypted_json(value: &Value) -> bool {
        value
            .as_object()
            .is_some_and(|o| o.contains_key(L2_ENVELOPE_KEY))
    }

    // -- L1 binary envelope --------------------------------------------------

    /// Seal raw L1 value bytes (a serialized [`crate::L1Entry`]) into the binary
    /// envelope `MAGIC || nonce || ciphertext`. AAD binds the org and the full
    /// L1 cache key string.
    pub fn seal_l1_value(
        &self,
        org_id: Uuid,
        key: &str,
        plain: &[u8],
    ) -> Result<Vec<u8>, ResponseCodecError> {
        let body = self.seal_raw(org_id, key.as_bytes(), plain)?;
        let mut out = Vec::with_capacity(L1_ENVELOPE_MAGIC.len() + body.len());
        out.extend_from_slice(L1_ENVELOPE_MAGIC);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// `true` when `bytes` carry the L1 envelope magic prefix (an encrypted L1
    /// value). Legacy plaintext L1 values are JSON and never start with the magic.
    #[must_use]
    pub fn is_encrypted_l1(bytes: &[u8]) -> bool {
        bytes.starts_with(L1_ENVELOPE_MAGIC)
    }

    /// Inspect stored L1 bytes. Not an envelope → [`L1Open::Plaintext`] (legacy
    /// row, return as-is). An envelope that authenticates → [`L1Open::Decrypted`];
    /// one that does not → [`L1Open::Undecryptable`] (treat as a miss).
    #[must_use]
    pub fn open_l1_value(&self, org_id: Uuid, key: &str, bytes: &[u8]) -> L1Open {
        let Some(body) = bytes.strip_prefix(L1_ENVELOPE_MAGIC) else {
            return L1Open::Plaintext;
        };
        match self.open_raw(org_id, key.as_bytes(), body) {
            Ok(plain) => L1Open::Decrypted(plain),
            Err(_) => L1Open::Undecryptable,
        }
    }
}

/// Result of [`ResponseCodec::open_response_json`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L2Open {
    /// The value is not an envelope — a legacy plaintext response row.
    Plaintext,
    /// The envelope authenticated; carries the recovered response bytes.
    Decrypted(Vec<u8>),
    /// The value is an envelope but did not authenticate (wrong key/org/row).
    Undecryptable,
}

/// Result of [`ResponseCodec::open_l1_value`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L1Open {
    /// The bytes are not an envelope — a legacy plaintext L1 value.
    Plaintext,
    /// The envelope authenticated; carries the recovered L1 value bytes.
    Decrypted(Vec<u8>),
    /// The bytes are an envelope but did not authenticate.
    Undecryptable,
}

/// Extract the owning org from a namespaced L1 cache key.
///
/// The gateway namespaces every L1 key as `"{org_id}:{request_hash}"`
/// (`tt_core`'s `namespaced_l1_key`) and prefixes negative-cache keys with
/// `"neg:"` (→ `"neg:{org_id}:{request_hash}"`). Retained agent transcripts
/// use `"tt:agent-run:{org_id}:{run_id}"`. This parses the org from every
/// owned shape so encryption and account-purge indexing use the same tenant.
///
/// Returns [`Uuid::nil`] for any key whose prefix is not a UUID — the value is
/// still encrypted, just under the deployment-wide (nil-org) key, so a key whose
/// format we do not recognize is never silently left in plaintext.
#[must_use]
pub fn org_from_l1_key(key: &str) -> Uuid {
    if let Some(agent_key) = key.strip_prefix("tt:agent-run:") {
        let org = agent_key.split(':').next().unwrap_or("");
        return Uuid::parse_str(org).unwrap_or_else(|_| Uuid::nil());
    }
    let candidate = key.strip_prefix("neg:").unwrap_or(key);
    let first_segment = candidate.split(':').next().unwrap_or("");
    Uuid::parse_str(first_segment).unwrap_or_else(|_| Uuid::nil())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codec() -> ResponseCodec {
        ResponseCodec::new([7u8; 32])
    }

    #[test]
    fn l2_round_trips_response_json() {
        let codec = codec();
        let org = Uuid::from_u128(1);
        let id = Uuid::from_u128(2);
        let plain = br#"{"choices":[{"message":{"content":"secret answer"}}]}"#;

        let sealed = codec.seal_response_json(org, id, plain).unwrap();
        assert!(ResponseCodec::is_encrypted_json(&sealed));
        assert!(
            !sealed.to_string().contains("secret answer"),
            "sealed envelope must not leak plaintext"
        );

        assert_eq!(
            codec.open_response_json(org, id, &sealed),
            L2Open::Decrypted(plain.to_vec())
        );
    }

    #[test]
    fn l2_plaintext_value_is_detected_as_legacy() {
        let codec = codec();
        let legacy: Value = serde_json::from_slice(
            br#"{"id":"chatcmpl","choices":[{"message":{"content":"hi"}}]}"#,
        )
        .unwrap();
        assert!(!ResponseCodec::is_encrypted_json(&legacy));
        assert_eq!(
            codec.open_response_json(Uuid::from_u128(1), Uuid::from_u128(2), &legacy),
            L2Open::Plaintext
        );
    }

    #[test]
    fn l2_wrong_org_cannot_decrypt() {
        let codec = codec();
        let id = Uuid::from_u128(2);
        let sealed = codec
            .seal_response_json(Uuid::from_u128(1), id, b"payload")
            .unwrap();
        // Same codec/key, different org → AEAD (and key) bind org, so it fails.
        assert_eq!(
            codec.open_response_json(Uuid::from_u128(999), id, &sealed),
            L2Open::Undecryptable
        );
    }

    #[test]
    fn l2_wrong_row_id_cannot_decrypt() {
        let codec = codec();
        let org = Uuid::from_u128(1);
        let sealed = codec
            .seal_response_json(org, Uuid::from_u128(2), b"payload")
            .unwrap();
        // AAD binds the row id → a blob copied onto another row fails to open.
        assert_eq!(
            codec.open_response_json(org, Uuid::from_u128(3), &sealed),
            L2Open::Undecryptable
        );
    }

    #[test]
    fn l2_wrong_key_cannot_decrypt() {
        let id = Uuid::from_u128(2);
        let org = Uuid::from_u128(1);
        let sealed = codec().seal_response_json(org, id, b"payload").unwrap();
        let other = ResponseCodec::new([9u8; 32]);
        assert_eq!(
            other.open_response_json(org, id, &sealed),
            L2Open::Undecryptable
        );
    }

    #[test]
    fn l1_round_trips_value_bytes() {
        let codec = codec();
        let org = Uuid::from_u128(5);
        let key = format!("{org}:abc123");
        let plain = br#"{"response":{"choices":[]},"baseline_cost_usd":0.1}"#;

        let sealed = codec.seal_l1_value(org, &key, plain).unwrap();
        assert!(ResponseCodec::is_encrypted_l1(&sealed));
        assert!(
            !sealed.windows(8).any(|w| w == b"baseline"),
            "sealed L1 envelope must not leak plaintext"
        );
        assert_eq!(
            codec.open_l1_value(org, &key, &sealed),
            L1Open::Decrypted(plain.to_vec())
        );
    }

    #[test]
    fn l1_plaintext_bytes_are_legacy() {
        let codec = codec();
        let legacy = br#"{"response":{},"version":1}"#;
        assert!(!ResponseCodec::is_encrypted_l1(legacy));
        assert_eq!(
            codec.open_l1_value(Uuid::from_u128(5), "k", legacy),
            L1Open::Plaintext
        );
    }

    #[test]
    fn l1_wrong_org_or_key_cannot_decrypt() {
        let codec = codec();
        let org = Uuid::from_u128(5);
        let key = format!("{org}:abc123");
        let sealed = codec.seal_l1_value(org, &key, b"payload").unwrap();

        // Different org (key string carries org, but exercise both axes).
        assert_eq!(
            codec.open_l1_value(Uuid::from_u128(6), &key, &sealed),
            L1Open::Undecryptable
        );
        // Different key string (different request hash) → AAD mismatch.
        let other_key = format!("{org}:zzz999");
        assert_eq!(
            codec.open_l1_value(org, &other_key, &sealed),
            L1Open::Undecryptable
        );
    }

    #[test]
    fn org_from_l1_key_parses_namespaced_and_negative() {
        let org = Uuid::from_u128(123456789);
        assert_eq!(org_from_l1_key(&format!("{org}:deadbeef")), org);
        assert_eq!(org_from_l1_key(&format!("neg:{org}:deadbeef")), org);
        assert_eq!(
            org_from_l1_key(&format!("tt:agent-run:{org}:{}", Uuid::new_v4())),
            org
        );
        // Unrecognized prefix → nil org (still encrypted, never plaintext).
        assert_eq!(org_from_l1_key("not-a-uuid:hash"), Uuid::nil());
        assert_eq!(org_from_l1_key("neg:not-a-uuid:hash"), Uuid::nil());
    }

    #[test]
    fn l1_malformed_short_envelope_is_undecryptable() {
        let codec = codec();
        // Magic present but body shorter than a nonce → Malformed → Undecryptable.
        let mut bytes = L1_ENVELOPE_MAGIC.to_vec();
        bytes.extend_from_slice(&[0u8; 4]);
        assert_eq!(
            codec.open_l1_value(Uuid::from_u128(5), "k", &bytes),
            L1Open::Undecryptable
        );
    }
}
