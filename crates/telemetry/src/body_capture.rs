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

/// Default per-body size cap (bytes) before truncation. Stored payloads larger
/// than this are cut to this many bytes and an explicit truncation marker is
/// appended; the ORIGINAL length is still recorded honestly in the
/// `request_bytes` / `response_bytes` columns.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

/// Leading bytes of the marker appended to a truncated stored payload. The full
/// marker is `\n[tt-body-capture:truncated original_bytes=<N>]` where `<N>` is
/// the byte length of the ORIGINAL (pre-truncation) plaintext. Lives INSIDE the
/// encrypted payload so a `/logs` reader sees the truncation explicitly on
/// decrypt without a schema change.
pub const TRUNCATION_MARKER_PREFIX: &[u8] = b"\n[tt-body-capture:truncated original_bytes=";

/// Build the full truncation marker for an original length of `original_len`.
fn truncation_marker(original_len: usize) -> Vec<u8> {
    let mut marker = Vec::with_capacity(TRUNCATION_MARKER_PREFIX.len() + 24);
    marker.extend_from_slice(TRUNCATION_MARKER_PREFIX);
    marker.extend_from_slice(original_len.to_string().as_bytes());
    marker.push(b']');
    marker
}

/// Cap `plain` to `max_bytes` for storage, appending an explicit truncation
/// marker when it overflows. Returns the payload to encrypt/store and the
/// ORIGINAL (pre-truncation) byte length to record honestly.
///
/// `max_bytes == 0` disables the cap (no truncation, no marker) — the same
/// pass-through behaviour as a body at or under the cap.
pub fn truncate_for_storage(plain: &[u8], max_bytes: usize) -> (Vec<u8>, usize) {
    let original_len = plain.len();
    if max_bytes == 0 || original_len <= max_bytes {
        return (plain.to_vec(), original_len);
    }
    let marker = truncation_marker(original_len);
    let mut out = Vec::with_capacity(max_bytes + marker.len());
    out.extend_from_slice(&plain[..max_bytes]);
    out.extend_from_slice(&marker);
    (out, original_len)
}

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

/// Outcome of re-keying a single stored body blob from one master key to
/// another. See [`BodyCaptureCodec::rekey_blob`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RekeyOutcome {
    /// The blob decrypted under the OLD key and was re-sealed under the new key.
    /// Carries the fresh `nonce || ciphertext` for the caller to persist.
    Reencrypted(Vec<u8>),
    /// The blob already decrypts under the NEW key — it was migrated by an
    /// earlier, interrupted rotation, so there is nothing to write. This is what
    /// makes [`postgres::PostgresBodyCaptureWriter::reencrypt_all`] idempotent
    /// and resumable: re-running over an already-rotated row is a no-op.
    AlreadyMigrated,
}

#[derive(Clone)]
pub struct BodyCaptureCodec {
    master_key: [u8; 32],
    max_bytes: usize,
}

impl std::fmt::Debug for BodyCaptureCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyCaptureCodec")
            .field("master_key", &"[REDACTED]")
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

impl BodyCaptureCodec {
    pub fn new(master_key: [u8; 32]) -> Self {
        Self {
            master_key,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Override the per-body size cap (bytes). `0` disables truncation.
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// The active per-body size cap (bytes); `0` means no cap.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Build from `TT_MASTER_KEY`. Missing means capture is disabled; malformed
    /// means an operator tried to enable encryption with an unusable root key.
    /// The per-body cap is read from `TT_BODY_CAPTURE_MAX_BYTES` (default
    /// [`DEFAULT_MAX_BYTES`]); an unparseable value falls back to the default.
    pub fn from_env() -> Result<Option<Self>, BodyCaptureError> {
        let Ok(hex_key) = std::env::var("TT_MASTER_KEY") else {
            return Ok(None);
        };
        let bytes = hex::decode(hex_key.trim())
            .map_err(|_| BodyCaptureError::BadMasterKey("expected 32 hex-encoded bytes".into()))?;
        let master_key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| BodyCaptureError::BadMasterKey("expected 32 hex-encoded bytes".into()))?;
        let max_bytes = std::env::var("TT_BODY_CAPTURE_MAX_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_BYTES);
        Ok(Some(Self {
            master_key,
            max_bytes,
        }))
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

    /// Re-seal a single stored body blob from this codec's (OLD) key to `new`'s
    /// key, preserving the `(org_id, trace_id, kind)` AAD binding.
    ///
    /// This is the body-capture analogue of the per-row step performed by
    /// `tt_auth::postgres::PostgresProviderCredentialStore::reencrypt_all`: it
    /// lets a `TT_MASTER_KEY` rotation re-key `request_body_captures` instead of
    /// leaving every stored body permanently undecryptable. DB-free and
    /// reusable — [`postgres::PostgresBodyCaptureWriter::reencrypt_all`] calls it
    /// once per stored column.
    ///
    /// - decrypts `blob` under `self` (the OLD key) and re-encrypts under `new`
    ///   with a fresh nonce → [`RekeyOutcome::Reencrypted`];
    /// - if that fails but `blob` decrypts under `new`, the row was already
    ///   migrated by an interrupted run → [`RekeyOutcome::AlreadyMigrated`]
    ///   (idempotent / resumable);
    /// - if it decrypts under NEITHER key, returns the underlying error
    ///   ([`BodyCaptureError::Malformed`] for a too-short blob, otherwise
    ///   [`BodyCaptureError::Crypto`]) so a corrupt or foreign-keyed row surfaces
    ///   loudly rather than being silently dropped.
    pub fn rekey_blob(
        &self,
        new: &BodyCaptureCodec,
        org_id: Uuid,
        trace_id: &str,
        kind: BodyCaptureKind,
        blob: &[u8],
    ) -> Result<RekeyOutcome, BodyCaptureError> {
        match self.decrypt(org_id, trace_id, kind, blob) {
            Ok(plain) => {
                let resealed = new.encrypt(org_id, trace_id, kind, &plain)?;
                Ok(RekeyOutcome::Reencrypted(resealed))
            }
            // A truncated blob can never be a valid ciphertext under any key.
            Err(BodyCaptureError::Malformed) => Err(BodyCaptureError::Malformed),
            // Didn't open under the OLD key: a resumed rotation may already have
            // re-sealed it under the NEW key. Anything that opens under neither
            // is corrupt (or sealed under a third key) and must not be lost.
            Err(old_err) => match new.decrypt(org_id, trace_id, kind, blob) {
                Ok(_) => Ok(RekeyOutcome::AlreadyMigrated),
                Err(_) => Err(old_err),
            },
        }
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

    /// Whether [`record`](Self::record) would actually PERSIST a body for this
    /// org, i.e. the org has opted into capture. Callers consult this BEFORE
    /// advertising capture (the `x-tokentrimmer-captured` response header) so
    /// the signal reflects a body that was truly stored, not merely a writer
    /// that happens to be armed for the deployment.
    ///
    /// Best-effort and side-effect free: on any backend error it MUST return
    /// `false` (do not over-advertise capture). The default impl returns `true`
    /// — appropriate only for sinks that persist everything they are handed
    /// (e.g. in-memory test writers); persistence-gated backends (Postgres)
    /// override this to consult the per-org opt-in row.
    async fn is_capture_enabled(&self, _org_id: Uuid) -> bool {
        true
    }
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

    /// One `request_body_captures` row as fetched for re-encryption:
    /// `(id, org_id, trace_id, request_enc, response_enc)`.
    type CaptureRekeyRow = (Uuid, Uuid, String, Vec<u8>, Option<Vec<u8>>);

    /// Default keyset-pagination batch size for
    /// [`PostgresBodyCaptureWriter::reencrypt_all`]. Each batch is re-keyed in
    /// its own committed transaction so a rotation over a large
    /// `request_body_captures` table makes bounded, resumable progress rather
    /// than holding one table-sized transaction.
    pub const REENCRYPT_BATCH_SIZE: i64 = 500;

    /// Counts from a body-capture re-encryption pass.
    /// See [`PostgresBodyCaptureWriter::reencrypt_all`]. Invariant:
    /// `scanned == reencrypted + already_current`.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ReencryptStats {
        /// Rows examined across every batch.
        pub scanned: usize,
        /// Rows whose ciphertext was re-sealed under the new key (request and/or
        /// response column changed).
        pub reencrypted: usize,
        /// Rows already sealed under the new key, skipped because an interrupted
        /// rotation resumed.
        pub already_current: usize,
    }

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

        /// Fetch the per-org capture setting. `Ok(None)` means no opt-in row
        /// (or the settings table is absent in this deployment) — capture is
        /// off for the org. Errors other than a missing table propagate so the
        /// caller can decide (record() surfaces them; the header check swallows
        /// them as "not enabled").
        async fn fetch_setting(
            &self,
            org_id: Uuid,
        ) -> Result<Option<BodyCaptureSetting>, BodyCaptureError> {
            match sqlx::query_as::<_, (bool, i32)>(
                "SELECT enabled, retention_days \
                 FROM request_body_capture_settings \
                 WHERE org_id = $1",
            )
            .bind(org_id)
            .fetch_optional(&self.pool)
            .await
            {
                Ok(Some((enabled, retention_days))) => Ok(Some(BodyCaptureSetting {
                    enabled,
                    retention_days,
                })),
                Ok(None) => Ok(None),
                Err(err) if is_undefined_table(&err) => Ok(None),
                Err(err) => Err(BodyCaptureError::Storage(err.to_string())),
            }
        }

        /// Re-encrypt every `request_body_captures` row from this writer's
        /// (current/OLD) `TT_MASTER_KEY` to `new_master_key`. This is the
        /// body-capture analogue of
        /// `tt_auth::postgres::PostgresProviderCredentialStore::reencrypt_all`:
        /// rotating `TT_MASTER_KEY` WITHOUT running this pass leaves every stored
        /// body permanently undecryptable (each row is sealed with a per-org key
        /// derived from the master). Run it BEFORE promoting the new key — build
        /// the writer from the current env, call `reencrypt_all(&new_key)`, then
        /// swap `TT_MASTER_KEY` and restart. See `docs/SECRETS.md`.
        ///
        /// Processed in keyset-paginated batches of [`REENCRYPT_BATCH_SIZE`],
        /// each committed in its own transaction, so the pass is:
        /// - **batched** — bounded memory and lock footprint over a large table;
        /// - **resumable** — committed batches survive an interruption, and a
        ///   re-run skips rows already sealed under the new key;
        /// - **idempotent** — running it again after completion re-keys nothing.
        ///
        /// A row that decrypts under NEITHER key rolls back its (uncommitted)
        /// batch and returns an error, so a corrupt row surfaces rather than
        /// being silently dropped; already-committed batches persist and a
        /// resumed run continues past them.
        pub async fn reencrypt_all(
            &self,
            new_master_key: &[u8; 32],
        ) -> Result<ReencryptStats, BodyCaptureError> {
            self.reencrypt_all_batched(new_master_key, REENCRYPT_BATCH_SIZE)
                .await
        }

        /// [`reencrypt_all`](Self::reencrypt_all) with an explicit batch size
        /// (clamped to `>= 1`). Exposed mainly for tests; production callers use
        /// the [`REENCRYPT_BATCH_SIZE`] default via [`reencrypt_all`].
        pub async fn reencrypt_all_batched(
            &self,
            new_master_key: &[u8; 32],
            batch_size: i64,
        ) -> Result<ReencryptStats, BodyCaptureError> {
            let batch_size = batch_size.max(1);
            let new_codec = BodyCaptureCodec::new(*new_master_key);
            let mut stats = ReencryptStats::default();
            // Keyset pagination on the UUID primary key: stable ordering, no
            // OFFSET re-scan, and re-keying never changes `id`, so the cursor
            // walk visits every row exactly once.
            let mut cursor = Uuid::nil();

            loop {
                let rows: Vec<CaptureRekeyRow> = match sqlx::query_as(
                    r#"SELECT id, org_id, trace_id, request_enc, response_enc
                             FROM request_body_captures
                            WHERE id > $1
                            ORDER BY id
                            LIMIT $2"#,
                )
                .bind(cursor)
                .bind(batch_size)
                .fetch_all(&self.pool)
                .await
                {
                    Ok(rows) => rows,
                    // Table absent in this deployment → nothing to rotate.
                    Err(err) if is_undefined_table(&err) => return Ok(stats),
                    Err(err) => return Err(BodyCaptureError::Storage(err.to_string())),
                };

                if rows.is_empty() {
                    break;
                }
                let fetched = rows.len();

                let mut tx = self
                    .pool
                    .begin()
                    .await
                    .map_err(|e| BodyCaptureError::Storage(e.to_string()))?;

                for (id, org_id, trace_id, request_enc, response_enc) in &rows {
                    cursor = *id;
                    stats.scanned += 1;

                    let request_outcome = self.codec.rekey_blob(
                        &new_codec,
                        *org_id,
                        trace_id,
                        BodyCaptureKind::Request,
                        request_enc,
                    )?;
                    let response_outcome = match response_enc {
                        Some(blob) => Some(self.codec.rekey_blob(
                            &new_codec,
                            *org_id,
                            trace_id,
                            BodyCaptureKind::Response,
                            blob,
                        )?),
                        None => None,
                    };

                    let new_request = match request_outcome {
                        RekeyOutcome::Reencrypted(blob) => Some(blob),
                        RekeyOutcome::AlreadyMigrated => None,
                    };
                    let new_response = match response_outcome {
                        Some(RekeyOutcome::Reencrypted(blob)) => Some(blob),
                        _ => None,
                    };

                    // Both columns already current → nothing to write for this
                    // row (the idempotent / resumable case).
                    if new_request.is_none() && new_response.is_none() {
                        stats.already_current += 1;
                        continue;
                    }

                    // Update only the column(s) that actually changed; COALESCE
                    // leaves an already-migrated (or NULL response) column as-is.
                    sqlx::query(
                        r#"UPDATE request_body_captures
                              SET request_enc = COALESCE($1, request_enc),
                                  response_enc = COALESCE($2, response_enc)
                            WHERE id = $3"#,
                    )
                    .bind(&new_request)
                    .bind(&new_response)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| BodyCaptureError::Storage(e.to_string()))?;
                    stats.reencrypted += 1;
                }

                tx.commit()
                    .await
                    .map_err(|e| BodyCaptureError::Storage(e.to_string()))?;

                // A short page means we've reached the end of the table.
                if (fetched as i64) < batch_size {
                    break;
                }
            }

            Ok(stats)
        }
    }

    #[async_trait]
    impl BodyCaptureWriter for PostgresBodyCaptureWriter {
        /// Mirrors the exact gate inside [`record`](Self::record): the org has
        /// an opt-in row AND `enabled = true`. Any backend error returns
        /// `false` so a transient DB hiccup never causes the gateway to claim
        /// capture for a body it did not store.
        async fn is_capture_enabled(&self, org_id: Uuid) -> bool {
            matches!(self.fetch_setting(org_id).await, Ok(Some(s)) if s.enabled)
        }

        async fn record(&self, record: BodyCaptureRecord) -> Result<(), BodyCaptureError> {
            let setting = match self.fetch_setting(record.org_id).await? {
                Some(setting) => setting,
                None => return Ok(()),
            };

            if !setting.enabled {
                return Ok(());
            }

            let retention_days = setting.retention_days.clamp(1, 30);
            let expires_at = record.ts + chrono::Duration::days(i64::from(retention_days));

            // Size cap: cut over-cap bodies and append an explicit truncation
            // marker BEFORE encryption, but record the ORIGINAL length honestly
            // in request_bytes/response_bytes (the marker lives inside the
            // encrypted payload, so /logs replay sees the truncation on decrypt
            // without a schema change).
            let max_bytes = self.codec.max_bytes();
            let (request_store, request_original_len) =
                truncate_for_storage(&record.request_json, max_bytes);
            let request_enc = self.codec.encrypt(
                record.org_id,
                &record.trace_id,
                BodyCaptureKind::Request,
                &request_store,
            )?;
            let response_store = record
                .response_json
                .as_ref()
                .map(|bytes| truncate_for_storage(bytes, max_bytes));
            let response_enc = match response_store.as_ref() {
                Some((store, _)) => Some(self.codec.encrypt(
                    record.org_id,
                    &record.trace_id,
                    BodyCaptureKind::Response,
                    store,
                )?),
                None => None,
            };
            let request_bytes = i32::try_from(request_original_len).unwrap_or(i32::MAX);
            let response_bytes = response_store
                .as_ref()
                .map(|(_, original_len)| i32::try_from(*original_len).unwrap_or(i32::MAX));

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
    fn truncate_passes_through_under_cap() {
        let plain = b"small body";
        let (stored, original_len) = truncate_for_storage(plain, DEFAULT_MAX_BYTES);
        assert_eq!(stored, plain, "under-cap body stored verbatim");
        assert_eq!(original_len, plain.len());
    }

    #[test]
    fn truncate_passes_through_at_exact_cap() {
        let plain = vec![b'x'; 16];
        let (stored, original_len) = truncate_for_storage(&plain, 16);
        assert_eq!(stored, plain, "at-cap body is not truncated");
        assert_eq!(original_len, 16);
    }

    #[test]
    fn truncate_zero_cap_disables() {
        let plain = vec![b'y'; 1024];
        let (stored, original_len) = truncate_for_storage(&plain, 0);
        assert_eq!(stored, plain, "max_bytes=0 disables the cap");
        assert_eq!(original_len, 1024);
    }

    #[test]
    fn truncate_over_cap_appends_marker_and_keeps_original_len() {
        let plain = vec![b'z'; 1000];
        let cap = 100;
        let (stored, original_len) = truncate_for_storage(&plain, cap);
        assert_eq!(original_len, 1000, "original length recorded honestly");
        assert_eq!(&stored[..cap], &plain[..cap], "kept the first cap bytes");
        let marker = &stored[cap..];
        assert!(
            marker.starts_with(TRUNCATION_MARKER_PREFIX),
            "marker appended"
        );
        assert!(
            marker.ends_with(b"1000]"),
            "marker discloses the original length"
        );
    }

    #[test]
    fn truncated_body_round_trips_through_codec_with_marker() {
        // Failing-first contract: an over-cap body, once stored and decrypted,
        // yields the truncated prefix plus the explicit marker, while the
        // original length is preserved out-of-band by the caller.
        let codec = BodyCaptureCodec::new([3u8; 32]).with_max_bytes(64);
        assert_eq!(codec.max_bytes(), 64);
        let org_id = Uuid::from_u128(99);
        let trace_id = "trace-trunc";
        let plain = vec![b'A'; 500];

        let (stored, original_len) = truncate_for_storage(&plain, codec.max_bytes());
        assert_eq!(original_len, 500);

        let blob = codec
            .encrypt(org_id, trace_id, BodyCaptureKind::Request, &stored)
            .unwrap();
        let decrypted = codec
            .decrypt(org_id, trace_id, BodyCaptureKind::Request, &blob)
            .unwrap();

        assert_eq!(&decrypted[..64], &plain[..64], "prefix intact on decrypt");
        let marker = &decrypted[64..];
        assert!(marker.starts_with(TRUNCATION_MARKER_PREFIX));
        assert!(marker.ends_with(b"500]"));
        // The stored payload is exactly cap + marker — never the full original.
        assert!(decrypted.len() < plain.len());
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

    /// A writer that persists everything it is handed inherits the trait
    /// default `is_capture_enabled` → `true` (correct for an always-persisting
    /// sink). Persistence-gated backends (Postgres) override it.
    #[tokio::test]
    async fn default_is_capture_enabled_is_true() {
        struct AlwaysRecord;
        #[async_trait]
        impl BodyCaptureWriter for AlwaysRecord {
            async fn record(&self, _record: BodyCaptureRecord) -> Result<(), BodyCaptureError> {
                Ok(())
            }
        }
        assert!(AlwaysRecord.is_capture_enabled(Uuid::from_u128(7)).await);
    }

    /// Core rotation crypto (no DB): a body blob sealed under the OLD master,
    /// re-keyed to the NEW master, opens under NEW and not OLD. This is the
    /// per-column step `reencrypt_all` performs across `request_body_captures`.
    #[test]
    fn rekey_moves_blob_to_new_master() {
        let old = BodyCaptureCodec::new([1u8; 32]);
        let new = BodyCaptureCodec::new([2u8; 32]);
        let org_id = Uuid::from_u128(5);
        let trace_id = "trace-rotate";
        let plain = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"x"}]}"#;

        let blob_old = old
            .encrypt(org_id, trace_id, BodyCaptureKind::Request, plain)
            .unwrap();
        // Before rotation the blob does NOT open under the new key.
        assert!(new
            .decrypt(org_id, trace_id, BodyCaptureKind::Request, &blob_old)
            .is_err());

        let RekeyOutcome::Reencrypted(blob_new) = old
            .rekey_blob(&new, org_id, trace_id, BodyCaptureKind::Request, &blob_old)
            .unwrap()
        else {
            panic!("expected a freshly re-encrypted blob");
        };

        // Re-sealed blob opens under NEW (same plaintext) and NOT under OLD.
        assert_eq!(
            new.decrypt(org_id, trace_id, BodyCaptureKind::Request, &blob_new)
                .unwrap(),
            plain
        );
        assert!(old
            .decrypt(org_id, trace_id, BodyCaptureKind::Request, &blob_new)
            .is_err());
    }

    /// Idempotent / resumable: a blob already sealed under the NEW key (an
    /// earlier interrupted run migrated it) re-keys to nothing.
    #[test]
    fn rekey_skips_already_migrated_blob() {
        let old = BodyCaptureCodec::new([1u8; 32]);
        let new = BodyCaptureCodec::new([2u8; 32]);
        let org_id = Uuid::from_u128(6);
        let trace_id = "trace-resumed";
        let already = new
            .encrypt(org_id, trace_id, BodyCaptureKind::Response, b"done")
            .unwrap();

        assert_eq!(
            old.rekey_blob(&new, org_id, trace_id, BodyCaptureKind::Response, &already)
                .unwrap(),
            RekeyOutcome::AlreadyMigrated
        );
    }

    /// A blob that opens under NEITHER key (corrupt, or sealed under a third
    /// key) is an error — never silently dropped.
    #[test]
    fn rekey_rejects_blob_under_neither_key() {
        let old = BodyCaptureCodec::new([1u8; 32]);
        let new = BodyCaptureCodec::new([2u8; 32]);
        let foreign = BodyCaptureCodec::new([3u8; 32]);
        let org_id = Uuid::from_u128(7);
        let trace_id = "trace-foreign";
        let blob = foreign
            .encrypt(
                org_id,
                trace_id,
                BodyCaptureKind::Request,
                b"sealed elsewhere",
            )
            .unwrap();

        assert!(old
            .rekey_blob(&new, org_id, trace_id, BodyCaptureKind::Request, &blob)
            .is_err());
        // A truncated blob is malformed under any key.
        assert!(matches!(
            old.rekey_blob(&new, org_id, trace_id, BodyCaptureKind::Request, &[0u8; 4]),
            Err(BodyCaptureError::Malformed)
        ));
    }

    /// Live Postgres re-encryption of `request_body_captures`. Gated behind
    /// `TEST_DATABASE_URL`; run with
    /// `cargo test -p tt-telemetry --features postgres -- --include-ignored`.
    /// The table ships from the OSS migrator (migration 0022) but the cloud
    /// schema may differ, so the test creates the minimal shape it needs and
    /// clears it first for determinism (use a throwaway test database).
    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (Postgres; the test creates + clears its own request_body_captures table) — run with --include-ignored"]
    async fn reencrypt_all_rekeys_request_body_captures() {
        use super::postgres::PostgresBodyCaptureWriter;

        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect TEST_DATABASE_URL");

        // Minimal table shape matching migration 0022. `IF NOT EXISTS` keeps the
        // test idempotent across re-runs; the DELETE isolates this run.
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS request_body_captures (
                 id            uuid PRIMARY KEY,
                 org_id        uuid NOT NULL,
                 api_key_id    uuid NOT NULL,
                 trace_id      text NOT NULL,
                 ts            timestamptz NOT NULL,
                 expires_at    timestamptz NOT NULL,
                 endpoint      text NOT NULL,
                 provider      text NOT NULL,
                 model         text NOT NULL,
                 request_enc   bytea NOT NULL,
                 response_enc  bytea,
                 request_bytes integer NOT NULL,
                 response_bytes integer,
                 created_at    timestamptz NOT NULL DEFAULT now(),
                 UNIQUE (org_id, trace_id)
               )"#,
        )
        .execute(&pool)
        .await
        .expect("create request_body_captures");
        sqlx::query("DELETE FROM request_body_captures")
            .execute(&pool)
            .await
            .expect("clear table");

        let old_key = [11u8; 32];
        let new_key = [22u8; 32];
        let old_codec = BodyCaptureCodec::new(old_key);
        let new_codec = BodyCaptureCodec::new(new_key);
        let org_id = Uuid::now_v7();
        let trace_id = format!("trace-{}", Uuid::now_v7());
        let req_plain = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"secret"}]}"#;
        let resp_plain = br#"{"choices":[{"message":{"content":"answer"}}]}"#;

        let req_enc = old_codec
            .encrypt(org_id, &trace_id, BodyCaptureKind::Request, req_plain)
            .unwrap();
        let resp_enc = old_codec
            .encrypt(org_id, &trace_id, BodyCaptureKind::Response, resp_plain)
            .unwrap();
        let id = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO request_body_captures
                 (id, org_id, api_key_id, trace_id, ts, expires_at, endpoint,
                  provider, model, request_enc, response_enc, request_bytes, response_bytes)
               VALUES ($1, $2, $3, $4, now(), now() + interval '7 days',
                       'chat', 'openai', 'gpt-4o', $5, $6, $7, $8)"#,
        )
        .bind(id)
        .bind(org_id)
        .bind(Uuid::now_v7())
        .bind(&trace_id)
        .bind(&req_enc)
        .bind(&resp_enc)
        .bind(req_plain.len() as i32)
        .bind(resp_plain.len() as i32)
        .execute(&pool)
        .await
        .expect("insert capture row");

        // Build the writer with the OLD codec (mirrors `from_env` before rotation).
        let writer = PostgresBodyCaptureWriter::new(pool.clone(), old_codec.clone());
        let stats = writer
            .reencrypt_all_batched(&new_key, 1)
            .await
            .expect("reencrypt_all");
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.reencrypted, 1);
        assert_eq!(stats.already_current, 0);

        // The row now decrypts under the NEW key and not the OLD one.
        let (req_now, resp_now): (Vec<u8>, Option<Vec<u8>>) = sqlx::query_as(
            "SELECT request_enc, response_enc FROM request_body_captures WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("re-read row");
        assert_eq!(
            new_codec
                .decrypt(org_id, &trace_id, BodyCaptureKind::Request, &req_now)
                .unwrap(),
            req_plain
        );
        assert!(old_codec
            .decrypt(org_id, &trace_id, BodyCaptureKind::Request, &req_now)
            .is_err());
        let resp_now = resp_now.expect("response_enc present");
        assert_eq!(
            new_codec
                .decrypt(org_id, &trace_id, BodyCaptureKind::Response, &resp_now)
                .unwrap(),
            resp_plain
        );

        // Idempotent / resumable: a second pass (still under the OLD writer key)
        // finds the row already migrated and re-keys nothing.
        let again = writer
            .reencrypt_all_batched(&new_key, 1)
            .await
            .expect("reencrypt_all (resumed)");
        assert_eq!(again.scanned, 1);
        assert_eq!(again.reencrypted, 0);
        assert_eq!(again.already_current, 1);

        sqlx::query("DELETE FROM request_body_captures WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .ok();
    }
}
