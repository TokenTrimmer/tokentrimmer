//! Strict offline receipt-issuer registry and customer-manifest-pin checking.

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REGISTRY_SCHEMA: &str = "tokentrimmer.receipt-issuer-registry.v1";
const KEYSET_SCHEMA: &str = "tokentrimmer.receipt-issuer-keyset.v1";
const REGISTRY_SCOPE: &str = "durable_latest_complete_revision";
const MAX_REGISTRY_BYTES: usize = 64 * 1024;
const MAX_KEYS: usize = 32;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum KeyState {
    Active,
    Retired,
    Revoked,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IssuerKey {
    kid: String,
    algorithm: String,
    public_key_hex: String,
    state: KeyState,
    valid_from: DateTime<Utc>,
    valid_until: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    revocation_reason_code: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IssuerKeyset {
    schema: String,
    revision: u64,
    issuer_id: String,
    keys: Vec<IssuerKey>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryTrustLimits {
    issuer_identity: String,
    customer_pinning: String,
    external_checkpoint: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuerRegistryDocument {
    schema: String,
    availability: String,
    registry_scope: String,
    issuer_id: String,
    revision: String,
    manifest_sha256: String,
    responding_runtime_signer_kid: String,
    keys: Vec<IssuerKey>,
    trust: RegistryTrustLimits,
}

fn is_lower_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_issuer_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b':' | b'/' | b'-'))
        })
}

fn valid_reason_code(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'_')
        })
}

fn canonical_kid(public_key: &[u8; 32]) -> String {
    format!("ed25519-sha256:{}", hex::encode(Sha256::digest(public_key)))
}

fn validate_key(key: &IssuerKey, now: DateTime<Utc>) -> anyhow::Result<()> {
    if key.algorithm != "Ed25519" || !is_lower_hex(&key.public_key_hex, 32) {
        bail!("every registry key must be canonical 32-byte Ed25519 public-key hex");
    }
    let bytes: [u8; 32] = hex::decode(&key.public_key_hex)
        .context("decode registry public key")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("registry public key must decode to 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&bytes).context("invalid registry Ed25519 key")?;
    if verifying_key.is_weak() || key.kid != canonical_kid(&bytes) {
        bail!("registry key is weak or its key ID is not its SHA-256 fingerprint");
    }
    if key.valid_until.is_some_and(|until| until <= key.valid_from) {
        bail!("registry key valid_until must be later than valid_from");
    }
    match key.state {
        KeyState::Active => {
            if key.revoked_at.is_some() || key.revocation_reason_code.is_some() {
                bail!("an active registry key cannot carry revocation metadata");
            }
        }
        KeyState::Retired => {
            if key.valid_until.is_none_or(|until| until > now)
                || key.revoked_at.is_some()
                || key.revocation_reason_code.is_some()
            {
                bail!(
                    "a retired registry key needs a non-future validity end and no revocation metadata"
                );
            }
        }
        KeyState::Revoked => {
            if key
                .revoked_at
                .is_none_or(|revoked_at| revoked_at < key.valid_from || revoked_at > now)
                || key
                    .revocation_reason_code
                    .as_deref()
                    .is_none_or(|reason| !valid_reason_code(reason))
            {
                bail!("a revoked registry key needs canonical non-future revocation metadata");
            }
        }
    }
    Ok(())
}

fn validate_keyset(keyset: &IssuerKeyset, now: DateTime<Utc>) -> anyhow::Result<()> {
    if keyset.schema != KEYSET_SCHEMA
        || keyset.revision == 0
        || keyset.revision > i64::MAX as u64
        || !valid_issuer_id(&keyset.issuer_id)
        || keyset.keys.is_empty()
        || keyset.keys.len() > MAX_KEYS
    {
        bail!("invalid issuer keyset schema, revision, issuer, or key count");
    }
    let mut previous: Option<&str> = None;
    for key in &keyset.keys {
        validate_key(key, now)?;
        if previous.is_some_and(|previous| previous >= key.kid.as_str()) {
            bail!("registry keys must be strictly sorted by unique key ID");
        }
        previous = Some(&key.kid);
    }
    if !keyset.keys.iter().any(|key| key.state == KeyState::Active) {
        bail!("issuer keyset must retain at least one active key");
    }
    Ok(())
}

fn parse_registry(raw: &str, now: DateTime<Utc>) -> anyhow::Result<(IssuerKeyset, Option<String>)> {
    if raw.len() > MAX_REGISTRY_BYTES {
        bail!("issuer registry exceeds the 64 KiB limit");
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).context("parse issuer registry JSON")?;
    match value.get("schema").and_then(serde_json::Value::as_str) {
        Some(KEYSET_SCHEMA) => {
            let keyset: IssuerKeyset =
                serde_json::from_value(value).context("parse strict issuer keyset")?;
            validate_keyset(&keyset, now)?;
            Ok((keyset, None))
        }
        Some(REGISTRY_SCHEMA) => {
            let registry: IssuerRegistryDocument =
                serde_json::from_value(value).context("parse strict issuer registry")?;
            if registry.schema != REGISTRY_SCHEMA
                || registry.availability != "available"
                || registry.registry_scope != REGISTRY_SCOPE
                || !is_lower_hex(&registry.manifest_sha256, 32)
                || registry.trust.issuer_identity
                    != "requires_independently_authenticated_registry_pin"
                || registry.trust.customer_pinning != "not_managed_by_this_registry"
                || registry.trust.external_checkpoint != "not_published"
            {
                bail!("issuer registry is unavailable or has unsupported trust semantics");
            }
            let revision = registry
                .revision
                .parse::<u64>()
                .context("issuer registry revision must be canonical decimal")?;
            if revision.to_string() != registry.revision {
                bail!("issuer registry revision must be canonical decimal");
            }
            let keyset = IssuerKeyset {
                schema: KEYSET_SCHEMA.to_owned(),
                revision,
                issuer_id: registry.issuer_id,
                keys: registry.keys,
            };
            validate_keyset(&keyset, now)?;
            if !keyset.keys.iter().any(|key| {
                key.kid == registry.responding_runtime_signer_kid && key.state == KeyState::Active
            }) {
                bail!("issuer registry responding signer is not an active key");
            }
            Ok((keyset, Some(registry.manifest_sha256)))
        }
        _ => bail!("unsupported issuer registry/keyset schema"),
    }
}

pub(super) fn verify_registry_pin(
    registry_path: &str,
    pinned_manifest_sha256: &str,
    supplied_key_hex: &str,
) -> anyhow::Result<()> {
    if !is_lower_hex(pinned_manifest_sha256, 32) {
        bail!("registry manifest pin must be exactly 64 lowercase SHA-256 hex characters");
    }
    if !is_lower_hex(supplied_key_hex, 32) {
        bail!("supplied verifying key must be exactly 64 lowercase hex characters");
    }
    let raw = std::fs::read_to_string(registry_path)
        .with_context(|| format!("read issuer registry {registry_path}"))?;
    let now = Utc::now();
    let (keyset, declared_sha) = parse_registry(&raw, now)?;
    let canonical =
        serde_json::to_vec(&keyset).context("serialize canonical issuer keyset manifest")?;
    let computed_sha = hex::encode(Sha256::digest(&canonical));
    if computed_sha != pinned_manifest_sha256
        || declared_sha
            .as_deref()
            .is_some_and(|declared| declared != computed_sha)
    {
        bail!("issuer keyset does not match the independently supplied manifest SHA-256 pin");
    }
    let supplied_bytes: [u8; 32] = hex::decode(supplied_key_hex)
        .context("decode supplied verifying key")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("supplied verifying key must decode to 32 bytes"))?;
    let supplied_kid = canonical_kid(&supplied_bytes);
    let key = keyset
        .keys
        .iter()
        .find(|key| key.kid == supplied_kid && key.public_key_hex == supplied_key_hex)
        .context("supplied receipt key is absent from the pinned issuer keyset")?;
    match key.state {
        KeyState::Revoked => bail!(
            "supplied receipt key is revoked ({})",
            key.revocation_reason_code.as_deref().unwrap_or("unspecified")
        ),
        KeyState::Retired => bail!(
            "supplied receipt key is a retired historical key; the receipt has no uniformly trusted signed issuance time proving it predates retirement"
        ),
        KeyState::Active
            if key.valid_from > now || key.valid_until.is_some_and(|until| now >= until) =>
        {
            bail!("supplied receipt key is outside its declared active validity window");
        }
        KeyState::Active => {}
    }
    crate::ui::ok(&format!(
        "PASS: supplied key is active in pinned issuer {} revision {} (manifest SHA-256 {})",
        keyset.issuer_id, keyset.revision, computed_sha
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ed25519_dalek::SigningKey;

    fn key(state: KeyState) -> IssuerKey {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let public = signing_key.verifying_key().to_bytes();
        IssuerKey {
            kid: canonical_kid(&public),
            algorithm: "Ed25519".into(),
            public_key_hex: hex::encode(public),
            state,
            valid_from: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            valid_until: (state == KeyState::Retired)
                .then(|| Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap()),
            revoked_at: (state == KeyState::Revoked)
                .then(|| Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap()),
            revocation_reason_code: (state == KeyState::Revoked).then(|| "key_compromise".into()),
        }
    }

    fn keyset(state: KeyState) -> IssuerKeyset {
        IssuerKeyset {
            schema: KEYSET_SCHEMA.into(),
            revision: 3,
            issuer_id: "tokentrimmer.hosted".into(),
            keys: vec![key(state)],
        }
    }

    fn write_keyset(
        dir: &std::path::Path,
        state: KeyState,
    ) -> (std::path::PathBuf, String, String) {
        let manifest = keyset(state);
        let raw = serde_json::to_vec(&manifest).unwrap();
        let sha = hex::encode(Sha256::digest(&raw));
        let path = dir.join(format!("issuer-keyset-{state:?}.json"));
        std::fs::write(&path, raw).unwrap();
        (path, sha, manifest.keys[0].public_key_hex.clone())
    }

    #[test]
    fn active_key_matches_independently_pinned_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (path, sha, public_key) = write_keyset(dir.path(), KeyState::Active);
        verify_registry_pin(path.to_str().unwrap(), &sha, &public_key)
            .expect("active key in exact pinned manifest must pass");
    }

    #[test]
    fn wrong_pin_absent_key_retired_and_revoked_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (active, sha, public_key) = write_keyset(dir.path(), KeyState::Active);
        assert!(
            verify_registry_pin(active.to_str().unwrap(), &"0".repeat(64), &public_key).is_err()
        );
        assert!(verify_registry_pin(active.to_str().unwrap(), &sha, &"01".repeat(32)).is_err());

        for state in [KeyState::Retired, KeyState::Revoked] {
            let (path, sha, public_key) = write_keyset(dir.path(), state);
            assert!(verify_registry_pin(path.to_str().unwrap(), &sha, &public_key).is_err());
        }
    }
}
