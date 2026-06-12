//! Opt-in encrypted request/response body capture for hosted audit/replay.
//!
//! Capture is controlled per org by `request_body_capture_settings`. Stored
//! bodies are encrypted with XChaCha20-Poly1305 using a per-org key derived from
//! `TT_MASTER_KEY`; the plaintext never lands in Postgres.

use async_trait::async_trait;
use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng, Payload},
    AeadCore, XChaCha20Poly1305, XNonce,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

const NONCE_LEN: usize = 24;
const AAD_MAGIC: &[u8] = b"tt-body-capture:v1";
const KDF_DOMAIN: &[u8] = b"|tt-body-capture:key:v1|";

#[derive(Debug, Error)]
pub enum BodyCaptureError {
    #[error("invalid TT_MASTER_KEY: {0}")]
    BadMasterKey(String),
    #[error("body capture crypto failed")]
    Crypto,
    #[error("body capture blob is malformed")]
    Malformed,
    #[error("body capture storage failed: {0}")]
    Storage(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyCaptureKind {
    Request,
    Response,
}

impl BodyCaptureKind {
    fn aad_tag(self) -> u8 {
        match self {
            Self::Request => b'q',
            Self::Response => b's',
        }
    }
}

#[derive(Clone)]
pub struct BodyCaptureCodec {
    master_key: [u8; 32],
}

impl std::fmt::Debug for BodyCaptureCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyCaptureCodec")
            .field("master_key", &"[REDACTED]")
            .finish()
    }
}

impl BodyCaptureCodec {
    pub fn new(master_key: [u8; 32]) -> Self {
        Self { master_key }
    }

    /// Build from `TT_MASTER_KEY`. Missing means capture is disabled; malformed
    /// means an operator tried to enable encryption with an unusable root key.
    pub fn from_env() -> Result<Option<Self>, BodyCaptureError> {
        let Ok(hex_key) = std::env::var("TT_MASTER_KEY") else {
            return Ok(None);
        };
        let bytes = hex::decode(hex_key.trim())
            .map_err(|_| BodyCaptureError::BadMasterKey("expected 32 hex-encoded bytes".into()))?;
        let master_key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| BodyCaptureError::BadMasterKey("expected 32 hex-encoded bytes".into()))?;
        Ok(Some(Self { master_key }))
    }

    pub fn encrypt(
        &self,
        org_id: Uuid,
        trace_id: &str,
        kind: BodyCaptureKind,
        plain: &[u8],
    ) -> Result<Vec<u8>, BodyCaptureError> {
        let key = derive_key(&self.master_key, org_id);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ad = aad(org_id, trace_id, kind);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plain,
                    aad: &ad,
                },
            )
            .map_err(|_| BodyCaptureError::Crypto)?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    pub fn decrypt(
        &self,
        org_id: Uuid,
        trace_id: &str,
        kind: BodyCaptureKind,
        blob: &[u8],
    ) -> Result<Vec<u8>, BodyCaptureError> {
        if blob.len() < NONCE_LEN {
            return Err(BodyCaptureError::Malformed);
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = XNonce::from_slice(nonce_bytes);
        let key = derive_key(&self.master_key, org_id);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let ad = aad(org_id, trace_id, kind);
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: &ad,
                },
            )
            .map_err(|_| BodyCaptureError::Crypto)
    }
}

#[derive(Debug, Clone)]
pub struct BodyCaptureRecord {
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub trace_id: String,
    pub endpoint: String,
    pub provider: String,
    pub model: String,
    pub request_json: Vec<u8>,
    pub response_json: Option<Vec<u8>>,
    pub ts: DateTime<Utc>,
}

#[async_trait]
pub trait BodyCaptureWriter: Send + Sync {
    async fn record(&self, record: BodyCaptureRecord) -> Result<(), BodyCaptureError>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BodyCaptureSetting {
    pub enabled: bool,
    pub retention_days: i32,
}

fn derive_key(master: &[u8; 32], org_id: Uuid) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(master);
    h.update(KDF_DOMAIN);
    h.update(org_id.as_bytes());
    h.finalize().into()
}

fn aad(org_id: Uuid, trace_id: &str, kind: BodyCaptureKind) -> Vec<u8> {
    let mut out = Vec::with_capacity(AAD_MAGIC.len() + 16 + trace_id.len() + 1);
    out.extend_from_slice(AAD_MAGIC);
    out.extend_from_slice(org_id.as_bytes());
    out.extend_from_slice(trace_id.as_bytes());
    out.push(kind.aad_tag());
    out
}

#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;

    #[derive(Clone)]
    pub struct PostgresBodyCaptureWriter {
        pool: sqlx::PgPool,
        codec: BodyCaptureCodec,
    }

    impl std::fmt::Debug for PostgresBodyCaptureWriter {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PostgresBodyCaptureWriter")
                .field("pool", &"PgPool { .. }")
                .field("codec", &self.codec)
                .finish()
        }
    }

    impl PostgresBodyCaptureWriter {
        pub fn new(pool: sqlx::PgPool, codec: BodyCaptureCodec) -> Self {
            Self { pool, codec }
        }

        pub fn from_env(pool: sqlx::PgPool) -> Result<Option<Self>, BodyCaptureError> {
            Ok(BodyCaptureCodec::from_env()?.map(|codec| Self { pool, codec }))
        }
    }

    #[async_trait]
    impl BodyCaptureWriter for PostgresBodyCaptureWriter {
        async fn record(&self, record: BodyCaptureRecord) -> Result<(), BodyCaptureError> {
            let setting = match sqlx::query_as::<_, (bool, i32)>(
                "SELECT enabled, retention_days \
                 FROM request_body_capture_settings \
                 WHERE org_id = $1",
            )
            .bind(record.org_id)
            .fetch_optional(&self.pool)
            .await
            {
                Ok(Some((enabled, retention_days))) => BodyCaptureSetting {
                    enabled,
                    retention_days,
                },
                Ok(None) => return Ok(()),
                Err(err) if is_undefined_table(&err) => return Ok(()),
                Err(err) => return Err(BodyCaptureError::Storage(err.to_string())),
            };

            if !setting.enabled {
                return Ok(());
            }

            let retention_days = setting.retention_days.clamp(1, 30);
            let expires_at = record.ts + chrono::Duration::days(i64::from(retention_days));
            let request_enc = self.codec.encrypt(
                record.org_id,
                &record.trace_id,
                BodyCaptureKind::Request,
                &record.request_json,
            )?;
            let response_enc = match record.response_json.as_ref() {
                Some(bytes) => Some(self.codec.encrypt(
                    record.org_id,
                    &record.trace_id,
                    BodyCaptureKind::Response,
                    bytes,
                )?),
                None => None,
            };
            let request_bytes = i32::try_from(record.request_json.len()).unwrap_or(i32::MAX);
            let response_bytes = record
                .response_json
                .as_ref()
                .map(|bytes| i32::try_from(bytes.len()).unwrap_or(i32::MAX));

            let result = sqlx::query(
                r#"INSERT INTO request_body_captures
                   (id, org_id, api_key_id, trace_id, ts, expires_at, endpoint,
                    provider, model, request_enc, response_enc, request_bytes, response_bytes)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                   ON CONFLICT (org_id, trace_id) DO UPDATE SET
                    api_key_id = EXCLUDED.api_key_id,
                    ts = EXCLUDED.ts,
                    expires_at = EXCLUDED.expires_at,
                    endpoint = EXCLUDED.endpoint,
                    provider = EXCLUDED.provider,
                    model = EXCLUDED.model,
                    request_enc = EXCLUDED.request_enc,
                    response_enc = COALESCE(EXCLUDED.response_enc, request_body_captures.response_enc),
                    request_bytes = EXCLUDED.request_bytes,
                    response_bytes = COALESCE(EXCLUDED.response_bytes, request_body_captures.response_bytes)"#,
            )
            .bind(Uuid::now_v7())
            .bind(record.org_id)
            .bind(record.api_key_id)
            .bind(&record.trace_id)
            .bind(record.ts)
            .bind(expires_at)
            .bind(&record.endpoint)
            .bind(&record.provider)
            .bind(&record.model)
            .bind(&request_enc)
            .bind(&response_enc)
            .bind(request_bytes)
            .bind(response_bytes)
            .execute(&self.pool)
            .await;

            match result {
                Ok(_) => Ok(()),
                Err(err) if is_undefined_table(&err) => Ok(()),
                Err(err) => Err(BodyCaptureError::Storage(err.to_string())),
            }
        }
    }

    fn is_undefined_table(err: &sqlx::Error) -> bool {
        matches!(
            err,
            sqlx::Error::Database(db) if db.code().as_deref() == Some("42P01")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_round_trips_request_and_response() {
        let codec = BodyCaptureCodec::new([7u8; 32]);
        let org_id = Uuid::from_u128(42);
        let trace_id = "trace-abc";
        let request = br#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"secret"}]}"#;
        let response = br#"{"choices":[{"message":{"content":"answer"}}]}"#;

        let request_blob = codec
            .encrypt(org_id, trace_id, BodyCaptureKind::Request, request)
            .unwrap();
        let response_blob = codec
            .encrypt(org_id, trace_id, BodyCaptureKind::Response, response)
            .unwrap();

        assert_eq!(
            codec
                .decrypt(org_id, trace_id, BodyCaptureKind::Request, &request_blob)
                .unwrap(),
            request
        );
        assert_eq!(
            codec
                .decrypt(org_id, trace_id, BodyCaptureKind::Response, &response_blob)
                .unwrap(),
            response
        );
    }

    #[test]
    fn aad_binds_trace_and_kind() {
        let codec = BodyCaptureCodec::new([8u8; 32]);
        let org_id = Uuid::from_u128(1);
        let blob = codec
            .encrypt(org_id, "trace-a", BodyCaptureKind::Request, b"hello")
            .unwrap();

        assert!(codec
            .decrypt(org_id, "trace-b", BodyCaptureKind::Request, &blob)
            .is_err());
        assert!(codec
            .decrypt(org_id, "trace-a", BodyCaptureKind::Response, &blob)
            .is_err());
    }
}
