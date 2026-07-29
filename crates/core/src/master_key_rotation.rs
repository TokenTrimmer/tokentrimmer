//! Shared database boot/readiness fence for `TT_MASTER_KEY` rotation.
//!
//! The hosted rotator writes only domain-separated key fingerprints to
//! `public.master_key_rotation`. The gateway never performs a rotation; it
//! consumes that journal so an interrupted pass or stale root cannot serve.

use anyhow::{bail, Context};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const FINGERPRINT_DOMAIN: &[u8] = b"tt-api:master-key-rotation:fingerprint:v1\0";

#[derive(Debug, sqlx::FromRow)]
struct RotationFence {
    state: String,
    new_key_fingerprint: String,
    phase: String,
}

/// Enforce the current shared rotation state using `TT_MASTER_KEY` only when a
/// journal row exists. Pre-first-rotation self-hosted databases retain their
/// existing behavior when the variable is intentionally absent.
pub async fn ensure_normal_boot_allowed_from_env(pool: &PgPool) -> anyhow::Result<()> {
    let row: Option<RotationFence> = sqlx::query_as(
        "SELECT state, new_key_fingerprint, phase \
         FROM public.master_key_rotation WHERE singleton",
    )
    .fetch_optional(pool)
    .await
    .context("read master-key rotation journal")?;
    let Some(row) = row else {
        return Ok(());
    };
    if row.state == "in_progress" {
        bail!(
            "TT_MASTER_KEY rotation is in progress at phase {}; normal serving is fenced",
            row.phase
        );
    }
    if row.state != "awaiting_promotion" && row.state != "complete" {
        bail!("unknown TT_MASTER_KEY rotation state {:?}", row.state);
    }
    let key = master_key_from_env()?;
    if key_fingerprint(&key) != row.new_key_fingerprint {
        bail!(
            "configured TT_MASTER_KEY does not match the promoted rotation fingerprint; refusing normal serving"
        );
    }
    Ok(())
}

fn master_key_from_env() -> anyhow::Result<[u8; 32]> {
    let encoded = std::env::var("TT_MASTER_KEY")
        .context("TT_MASTER_KEY is required while a rotation journal exists")?;
    let bytes =
        hex::decode(encoded.trim()).context("TT_MASTER_KEY must contain 32 hex-encoded bytes")?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        anyhow::anyhow!(
            "TT_MASTER_KEY must contain 32 hex-encoded bytes; decoded {}",
            bytes.len()
        )
    })
}

fn key_fingerprint(key: &[u8; 32]) -> String {
    let mut digest = Sha256::new();
    digest.update(FINGERPRINT_DOMAIN);
    digest.update(key);
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_contract_matches_hosted_rotator_vector() {
        assert_eq!(
            key_fingerprint(&[7_u8; 32]),
            "183171e8b84e7335d15205d61e2e1a391e718e740e86d67d17c004eb7ae1a980"
        );
        assert_ne!(key_fingerprint(&[7_u8; 32]), hex::encode([7_u8; 32]));
    }
}
