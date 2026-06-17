//! Local signed audit-chain append for `tt plan --apply`.
//!
//! The public CLI otherwise only *verifies* chains; this is the minimal local
//! APPEND path so a `plan.applied` entry lands in `.claude/AUDIT-CHAIN.jsonl`
//! and is provable with `tt audit verify`. The Ed25519 signing key is persisted
//! per-machine at `~/.tokentrimmer/audit-signing-key` (mode 0600), generated on
//! first use — a local-operator trust model.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use ed25519_dalek::SigningKey;
use uuid::Uuid;

use tt_telemetry::audit::{build_entry, generate_signing_key, Actor, AuditEntry};

/// Default local audit chain path (same file `tt audit verify` reads).
pub const DEFAULT_CHAIN_PATH: &str = ".claude/AUDIT-CHAIN.jsonl";

/// Load the per-machine signing key, generating + persisting one at
/// `~/.tokentrimmer/audit-signing-key` (mode 0600) on first use.
pub fn load_or_create_signing_key() -> anyhow::Result<SigningKey> {
    let path = signing_key_path()?;
    if path.exists() {
        let hex_str = std::fs::read_to_string(&path)
            .with_context(|| format!("read signing key {}", path.display()))?;
        let bytes = hex::decode(hex_str.trim()).context("signing key hex decode")?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes (64 hex chars)"))?;
        Ok(SigningKey::from_bytes(&arr))
    } else {
        let key = generate_signing_key();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        write_private(&path, &hex::encode(key.to_bytes()))?;
        Ok(key)
    }
}

fn signing_key_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .context("HOME not set — cannot locate ~/.tokentrimmer/audit-signing-key")?;
    Ok(PathBuf::from(home)
        .join(".tokentrimmer")
        .join("audit-signing-key"))
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create signing key {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("create signing key {}", path.display()))
}

/// Append a signed entry to the JSONL chain at `chain_path`, returning the
/// verifying-key hex. Creates the file with a
/// `{"meta":true,"verifying_key":"<hex>"}` preamble when absent; otherwise
/// chains onto the last entry.
pub fn append_entry(
    chain_path: &Path,
    signing_key: &SigningKey,
    org_id: Uuid,
    event: &str,
    payload: serde_json::Value,
) -> anyhow::Result<String> {
    let verifying_hex = hex::encode(signing_key.verifying_key().to_bytes());

    let existing = read_entries(chain_path)?;
    let entry = build_entry(
        signing_key,
        existing.last(),
        org_id,
        Actor::System,
        event.to_string(),
        payload,
    )
    .context("build audit entry")?;

    let new_file = !chain_path.exists();
    if let Some(parent) = chain_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(chain_path)
        .with_context(|| format!("open chain {}", chain_path.display()))?;
    if new_file {
        let preamble = serde_json::json!({"meta": true, "verifying_key": verifying_hex});
        writeln!(f, "{}", serde_json::to_string(&preamble)?)?;
    }
    writeln!(f, "{}", serde_json::to_string(&entry)?)?;
    Ok(verifying_hex)
}

/// Read audit entries from a JSONL chain file (skipping a `meta` preamble line).
/// Returns an empty vec when the file does not exist.
fn read_entries(chain_path: &Path) -> anyhow::Result<Vec<AuditEntry>> {
    if !chain_path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(chain_path)
        .with_context(|| format!("read chain {}", chain_path.display()))?;
    let mut entries = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(t).context("parse chain line")?;
        if v.get("meta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        entries.push(serde_json::from_value(v).context("parse audit entry")?);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_preamble_then_chains_and_verifies() {
        let dir = std::env::temp_dir().join(format!("tt-local-audit-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let chain = dir.join("AUDIT-CHAIN.jsonl");
        let key = generate_signing_key();
        let org = Uuid::new_v4();

        // First append → file created with preamble + 1 entry.
        let vk1 = append_entry(
            &chain,
            &key,
            org,
            "plan.applied",
            serde_json::json!({"n":1}),
        )
        .expect("first append");
        // Second append → chains onto it (seq 1).
        let vk2 = append_entry(
            &chain,
            &key,
            org,
            "plan.applied",
            serde_json::json!({"n":2}),
        )
        .expect("second append");
        assert_eq!(vk1, vk2, "stable verifying key");

        let entries = read_entries(&chain).expect("read");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[1].prev_hash, entries[0].hash);

        // The chain verifies under the verifying key parsed from the hex.
        let vk_bytes: [u8; 32] = hex::decode(&vk1).unwrap().try_into().unwrap();
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&vk_bytes).unwrap();
        tt_telemetry::audit::verify_chain(&entries, &vk).expect("verifies");

        std::fs::remove_dir_all(&dir).ok();
    }
}
