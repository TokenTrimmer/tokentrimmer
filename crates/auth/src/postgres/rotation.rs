//! Maintenance primitives, not an online rotation coordinator. Runtime reads
//! still use a single root; all writers/readers must be stopped during rotation.

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{decrypt_blob, encrypt_blob, CredentialStoreError, PostgresProviderCredentialStore};

type Row = (Uuid, Uuid, String, Vec<u8>);

/// Aggregate committed progress. No tenant identifiers, key material or secrets.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CredentialRotationStats {
    /// Rows inspected, including those already sealed with the destination key.
    pub scanned: usize,
    /// Rows actually re-sealed in this pass.
    pub reencrypted: usize,
    /// Rows authenticated under the destination key and left byte-for-byte intact.
    pub already_current: usize,
    /// Nonempty committed transactions.
    pub batches: usize,
}

impl PostgresProviderCredentialStore {
    /// Legacy full-table, single-transaction re-key. Returns rows re-encrypted.
    ///
    /// The scan holds row locks through every ciphertext-CAS update. A racing
    /// put cannot be overwritten with a stale secret; it may still write under
    /// its OLD root after commit. Thus concurrency safety is not key convergence:
    /// freeze all writers and cover all encrypted families before promotion.
    /// This API retains its all-or-nothing behavior; for bounded, mixed-old/new
    /// maintenance use [`Self::reencrypt_all_batched`].
    pub async fn reencrypt_all(
        &self,
        new_master_key: &[u8; 32],
    ) -> Result<usize, CredentialStoreError> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, org_id, provider, secret_enc FROM provider_credentials ORDER BY id FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await?;
        let count = rows.len();
        for row in rows {
            reencrypt(&mut tx, &self.master_key, new_master_key, &row).await?;
        }
        tx.commit().await?;
        Ok(count)
    }

    /// Bounded, idempotent maintenance over `provider_credentials` ONLY.
    ///
    /// Each keyset page (1–1000 rows) is locked, re-keyed and committed in one
    /// transaction. Authenticate with the destination key first, leaving already
    /// current ciphertext and timestamps unchanged. A row authenticating under
    /// neither key rolls back its entire page; earlier pages remain committed.
    /// Re-run from the beginning with the SAME two roots to resume safely after
    /// failure or cancellation. No progress cursor is trusted across restarts.
    ///
    /// Reads remain single-key and this function does NOT fence serving, prove
    /// writer quiescence, rotate other families, or authorize root promotion.
    /// In particular, a concurrent old-root write behind the cursor requires
    /// another pass after writers stop. Never advertise this as online rotation.
    pub async fn reencrypt_all_batched(
        &self,
        new_master_key: &[u8; 32],
        batch_size: usize,
    ) -> Result<CredentialRotationStats, CredentialStoreError> {
        if !(1..=1000).contains(&batch_size) {
            return Err(CredentialStoreError::InvalidRotationBatchSize);
        }
        let mut stats = CredentialRotationStats::default();
        let mut cursor: Option<Uuid> = None;
        loop {
            let mut tx = self.pool.begin().await?;
            let rows: Vec<Row> = sqlx::query_as(
                "SELECT id, org_id, provider, secret_enc FROM provider_credentials
                 WHERE ($1::uuid IS NULL OR id > $1)
                 ORDER BY id LIMIT $2 FOR UPDATE",
            )
            .bind(cursor)
            .bind(batch_size as i64)
            .fetch_all(&mut *tx)
            .await?;
            if rows.is_empty() {
                tx.commit().await?;
                return Ok(stats);
            }
            let mut reencrypted = 0;
            let mut already_current = 0;
            for row @ (id, org, provider, blob) in &rows {
                if decrypt_blob(new_master_key, *org, provider, blob).is_ok() {
                    already_current += 1;
                } else {
                    reencrypt(&mut tx, &self.master_key, new_master_key, row).await?;
                    reencrypted += 1;
                }
                cursor = Some(*id);
            }
            tx.commit().await?;
            stats.scanned += rows.len();
            stats.reencrypted += reencrypted;
            stats.already_current += already_current;
            stats.batches += 1;
        }
    }
}

async fn reencrypt(
    tx: &mut Transaction<'_, Postgres>,
    old: &[u8; 32],
    new: &[u8; 32],
    (id, org, provider, blob): &Row,
) -> Result<(), CredentialStoreError> {
    let plain = decrypt_blob(old, *org, provider, blob)?;
    let sealed = encrypt_blob(new, *org, provider, &plain)?;
    let updated = sqlx::query(
        "UPDATE provider_credentials SET secret_enc = $1, rotated_at = now()
         WHERE id = $2 AND secret_enc = $3",
    )
    .bind(sealed)
    .bind(id)
    .bind(blob)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(CredentialStoreError::ConcurrentModification);
    }
    Ok(())
}
