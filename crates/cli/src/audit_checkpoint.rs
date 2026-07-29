//! Customer-controlled audit-tip checkpoints.
//!
//! A checkpoint does not replace the TokenTrimmer audit signature. It lets a
//! customer co-sign one exact organization/key/tip tuple with a separately
//! controlled Ed25519 key. Later verification still checks every audit entry
//! under the TokenTrimmer key and requires the chain to reach the co-signed tip.

use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SCHEMA: &str = "tokentrimmer.customer-audit-checkpoint.v1";
const DOMAIN: &str = "tt.customer-audit-checkpoint.v1";
const MAX_CHECKPOINT_BYTES: u64 = 16 * 1024;
const MAX_KEY_FILE_BYTES: u64 = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustomerAuditCheckpoint {
    pub schema: String,
    pub organization_id: Uuid,
    pub audit_verifying_key_hex: String,
    pub sequence: i64,
    pub tip_hash: String,
    pub checkpointed_at: DateTime<Utc>,
    pub customer_key_id: String,
    pub signature_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedCustomerCheckpoint {
    pub organization_id: Uuid,
    pub audit_verifying_key_hex: String,
    pub anchor: tt_telemetry::audit::TipAnchor,
    pub customer_key_id: String,
}

fn canonical_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn canonical_payload(checkpoint: &CustomerAuditCheckpoint) -> Vec<u8> {
    format!(
        "{DOMAIN}|{}|{}|{}|{}|{}|{}",
        checkpoint.organization_id,
        checkpoint.audit_verifying_key_hex,
        checkpoint.sequence,
        checkpoint.tip_hash,
        canonical_timestamp(checkpoint.checkpointed_at),
        checkpoint.customer_key_id,
    )
    .into_bytes()
}

fn parse_lower_hex_32(value: &str, field: &str) -> anyhow::Result<[u8; 32]> {
    if value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        anyhow::bail!("{field} must be exactly 64 lowercase hex characters");
    }
    let bytes = hex::decode(value).with_context(|| format!("decode {field}"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{field} must decode to exactly 32 bytes"))
}

fn customer_key_id(verifying_key: &VerifyingKey) -> String {
    format!(
        "ed25519-sha256:{}",
        hex::encode(Sha256::digest(verifying_key.as_bytes()))
    )
}

pub(crate) fn create_checkpoint(
    organization_id: Uuid,
    audit_verifying_key_hex: &str,
    anchor: &tt_telemetry::audit::TipAnchor,
    checkpointed_at: DateTime<Utc>,
    customer_signing_key: &SigningKey,
) -> anyhow::Result<CustomerAuditCheckpoint> {
    let audit_key = parse_lower_hex_32(audit_verifying_key_hex, "audit_verifying_key_hex")?;
    VerifyingKey::from_bytes(&audit_key)
        .context("audit_verifying_key_hex is not a valid Ed25519 point")?;
    if anchor.seq < 0 {
        anyhow::bail!("checkpoint sequence must be non-negative");
    }
    parse_lower_hex_32(&anchor.hash, "tip_hash")?;
    if checkpointed_at.nanosecond() != 0 {
        anyhow::bail!("checkpointed_at must use whole UTC seconds");
    }

    let mut checkpoint = CustomerAuditCheckpoint {
        schema: SCHEMA.to_string(),
        organization_id,
        audit_verifying_key_hex: audit_verifying_key_hex.to_string(),
        sequence: anchor.seq,
        tip_hash: anchor.hash.clone(),
        checkpointed_at,
        customer_key_id: customer_key_id(&customer_signing_key.verifying_key()),
        signature_hex: String::new(),
    };
    checkpoint.signature_hex = hex::encode(
        customer_signing_key
            .sign(&canonical_payload(&checkpoint))
            .to_bytes(),
    );
    Ok(checkpoint)
}

pub(crate) fn verify_checkpoint(
    checkpoint: &CustomerAuditCheckpoint,
    customer_verifying_key: &VerifyingKey,
) -> anyhow::Result<VerifiedCustomerCheckpoint> {
    if checkpoint.schema != SCHEMA {
        anyhow::bail!("unsupported customer audit checkpoint schema");
    }
    let audit_key = parse_lower_hex_32(
        &checkpoint.audit_verifying_key_hex,
        "audit_verifying_key_hex",
    )?;
    VerifyingKey::from_bytes(&audit_key)
        .context("audit_verifying_key_hex is not a valid Ed25519 point")?;
    if checkpoint.sequence < 0 {
        anyhow::bail!("checkpoint sequence must be non-negative");
    }
    parse_lower_hex_32(&checkpoint.tip_hash, "tip_hash")?;
    if checkpoint.checkpointed_at.nanosecond() != 0 {
        anyhow::bail!("checkpointed_at must use whole UTC seconds");
    }

    let expected_key_id = customer_key_id(customer_verifying_key);
    if checkpoint.customer_key_id != expected_key_id {
        anyhow::bail!("customer_key_id does not match the supplied out-of-band customer key");
    }
    if checkpoint.signature_hex.len() != 128
        || !checkpoint
            .signature_hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || checkpoint
            .signature_hex
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
    {
        anyhow::bail!("signature_hex must be exactly 128 lowercase hex characters");
    }
    let signature_bytes = hex::decode(&checkpoint.signature_hex).context("decode signature_hex")?;
    let signature =
        Signature::from_slice(&signature_bytes).context("parse Ed25519 checkpoint signature")?;
    customer_verifying_key
        .verify(&canonical_payload(checkpoint), &signature)
        .context("customer checkpoint signature verification failed")?;

    Ok(VerifiedCustomerCheckpoint {
        organization_id: checkpoint.organization_id,
        audit_verifying_key_hex: checkpoint.audit_verifying_key_hex.clone(),
        anchor: tt_telemetry::audit::TipAnchor {
            seq: checkpoint.sequence,
            hash: checkpoint.tip_hash.clone(),
        },
        customer_key_id: checkpoint.customer_key_id.clone(),
    })
}

fn read_hex_key_file(
    path: &Path,
    label: &str,
    require_private_permissions: bool,
) -> anyhow::Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("open {label} {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if require_private_permissions {
            let mode = file
                .metadata()
                .with_context(|| format!("read metadata for {label} {}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                anyhow::bail!(
                    "{label} {} must not be accessible by group or other users (expected mode 0600)",
                    path.display()
                );
            }
        }
    }
    let mut content = String::new();
    use std::io::Read as _;
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_string(&mut content)
        .with_context(|| format!("read {label} {}", path.display()))?;
    if content.len() as u64 > MAX_KEY_FILE_BYTES {
        anyhow::bail!("{label} exceeds {MAX_KEY_FILE_BYTES} byte limit");
    }
    Ok(content.trim().to_string())
}

fn parse_signing_key_hex(value: &str) -> anyhow::Result<SigningKey> {
    let bytes = parse_lower_hex_32(value, "customer signing key")?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub(crate) fn parse_verifying_key_hex(value: &str, label: &str) -> anyhow::Result<VerifyingKey> {
    let bytes = parse_lower_hex_32(value, label)?;
    VerifyingKey::from_bytes(&bytes)
        .with_context(|| format!("{label} is not a valid Ed25519 point"))
}

pub(crate) fn run_create_checkpoint(
    organization_id: &str,
    audit_verifying_key_hex: &str,
    expected_tip: &str,
    customer_signing_key_path: &str,
    output_path: &str,
) -> anyhow::Result<()> {
    let organization_id = organization_id
        .parse::<Uuid>()
        .context("--org must be a canonical UUID")?;
    let anchor = super::audit::parse_expected_tip(expected_tip)?;
    let signing_key_hex = read_hex_key_file(
        Path::new(customer_signing_key_path),
        "customer signing key",
        true,
    )?;
    let signing_key = parse_signing_key_hex(&signing_key_hex)?;
    let checkpointed_at = Utc::now()
        .with_nanosecond(0)
        .context("normalize checkpoint timestamp")?;
    let checkpoint = create_checkpoint(
        organization_id,
        audit_verifying_key_hex,
        &anchor,
        checkpointed_at,
        &signing_key,
    )?;
    let output = serde_json::to_vec_pretty(&checkpoint).context("serialize checkpoint")?;
    let output_path = Path::new(output_path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_path)
        .with_context(|| {
            format!(
                "create checkpoint {} (refusing to overwrite an existing file)",
                output_path.display()
            )
        })?;
    use std::io::Write as _;
    file.write_all(&output)
        .with_context(|| format!("write checkpoint {}", output_path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("finish checkpoint {}", output_path.display()))?;

    tt_cli::ui::ok(&format!(
        "customer checkpoint written to {}",
        output_path.display()
    ));
    tt_cli::ui::note(&format!(
        "customer key id: {} (the verifying key is not embedded; retain it out of band)",
        checkpoint.customer_key_id
    ));
    Ok(())
}

pub(crate) fn load_and_verify_checkpoint(
    checkpoint_path: &str,
    customer_key_path: Option<&str>,
    customer_key_hex: Option<&str>,
) -> anyhow::Result<VerifiedCustomerCheckpoint> {
    let checkpoint_path = Path::new(checkpoint_path);
    let checkpoint_file = std::fs::File::open(checkpoint_path)
        .with_context(|| format!("open checkpoint {}", checkpoint_path.display()))?;
    let mut content = String::new();
    use std::io::Read as _;
    checkpoint_file
        .take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_string(&mut content)
        .with_context(|| format!("read checkpoint {}", checkpoint_path.display()))?;
    if content.len() as u64 > MAX_CHECKPOINT_BYTES {
        anyhow::bail!("customer checkpoint exceeds {MAX_CHECKPOINT_BYTES} byte limit");
    }
    let checkpoint: CustomerAuditCheckpoint =
        serde_json::from_str(&content).context("parse strict customer checkpoint JSON")?;
    let key_hex = if let Some(value) = customer_key_hex {
        value.trim().to_string()
    } else if let Some(path) = customer_key_path {
        read_hex_key_file(Path::new(path), "customer verifying key", false)?
    } else {
        anyhow::bail!(
            "--customer-checkpoint requires --customer-key or --customer-key-hex from an out-of-band source"
        );
    };
    let customer_key = parse_verifying_key_hex(&key_hex, "customer verifying key")?;
    verify_checkpoint(&checkpoint, &customer_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn fixture() -> (CustomerAuditCheckpoint, VerifyingKey) {
        let customer = SigningKey::from_bytes(&[7; 32]);
        let audit = SigningKey::from_bytes(&[8; 32]);
        let checkpoint = create_checkpoint(
            Uuid::from_u128(1),
            &hex::encode(audit.verifying_key().to_bytes()),
            &tt_telemetry::audit::TipAnchor {
                seq: 42,
                hash: "ab".repeat(32),
            },
            Utc.with_ymd_and_hms(2026, 7, 27, 12, 0, 0)
                .single()
                .expect("fixed timestamp"),
            &customer,
        )
        .expect("fixture");
        (checkpoint, customer.verifying_key())
    }

    #[test]
    fn customer_signature_binds_the_complete_checkpoint() {
        let (checkpoint, key) = fixture();
        let verified = verify_checkpoint(&checkpoint, &key).expect("valid checkpoint");
        assert_eq!(verified.organization_id, Uuid::from_u128(1));
        assert_eq!(verified.anchor.seq, 42);
        assert_eq!(verified.anchor.hash, "ab".repeat(32));
        assert!(verified.customer_key_id.starts_with("ed25519-sha256:"));
    }

    #[test]
    fn tamper_and_wrong_customer_key_fail_closed() {
        let (checkpoint, key) = fixture();
        let wrong = SigningKey::from_bytes(&[9; 32]).verifying_key();
        assert!(verify_checkpoint(&checkpoint, &wrong).is_err());

        let mut cases = Vec::new();
        let mut changed = checkpoint.clone();
        changed.organization_id = Uuid::from_u128(2);
        cases.push(changed);
        let mut changed = checkpoint.clone();
        changed.sequence = 41;
        cases.push(changed);
        let mut changed = checkpoint.clone();
        changed.tip_hash = "cd".repeat(32);
        cases.push(changed);
        let mut changed = checkpoint.clone();
        changed.audit_verifying_key_hex =
            hex::encode(SigningKey::from_bytes(&[10; 32]).verifying_key().to_bytes());
        cases.push(changed);
        let mut changed = checkpoint.clone();
        changed.checkpointed_at += chrono::Duration::seconds(1);
        cases.push(changed);

        for changed in cases {
            assert!(verify_checkpoint(&changed, &key).is_err());
        }
    }

    #[test]
    fn strict_json_rejects_unknown_fields_and_does_not_embed_customer_key() {
        let (checkpoint, key) = fixture();
        let json = serde_json::to_string(&checkpoint).expect("serialize");
        assert!(!json.contains(&hex::encode(key.to_bytes())));

        let mut value = serde_json::to_value(checkpoint).expect("value");
        value
            .as_object_mut()
            .expect("object")
            .insert("future".into(), serde_json::json!(true));
        assert!(serde_json::from_value::<CustomerAuditCheckpoint>(value).is_err());
    }

    #[test]
    fn checkpoint_reader_enforces_its_streamed_byte_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("oversized-checkpoint.json");
        std::fs::write(&path, vec![b' '; MAX_CHECKPOINT_BYTES as usize + 1])
            .expect("write oversized checkpoint");

        let error = load_and_verify_checkpoint(
            path.to_str().expect("UTF-8 temp path"),
            None,
            Some(&hex::encode(
                SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes(),
            )),
        )
        .expect_err("oversized checkpoint must fail before parsing");
        assert!(error.to_string().contains("exceeds 16384 byte limit"));
    }
}
