//! `tt audit` command implementation.

use std::path::Path;

use anyhow::Context as _;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum AuditAction {
    /// Verify the integrity of an audit log hash chain.
    ///
    /// Reads JSONL entries from `[PATH]` (default `.claude/AUDIT-CHAIN.jsonl`).
    /// When the first line is the tt-api export preamble
    /// `{"meta":true,"verifying_key":"<hex>",...}` the verifying key is
    /// extracted automatically. Otherwise pass `--key <hex-file>` with the
    /// hex-encoded Ed25519 verifying key (or `--key-hex <hex>` inline).
    Verify {
        /// Path to the JSONL chain. Defaults to `.claude/AUDIT-CHAIN.jsonl`.
        path: Option<String>,
        /// Assert the checkpoint org when using `--customer-checkpoint`.
        /// Otherwise recorded only; every entry in the file is still verified.
        #[arg(long)]
        org: Option<String>,
        /// Path to a file containing the hex-encoded Ed25519 verifying key.
        /// Overrides the preamble key when both are present.
        #[arg(long)]
        key: Option<String>,
        /// Hex-encoded Ed25519 verifying key inline. Overrides `--key` and the
        /// preamble when present.
        #[arg(long)]
        key_hex: Option<String>,
        /// Expected chain tip as `<seq>:<hash>`, captured out-of-band from the
        /// `tt::audit::tip` log stream. When set, the chain must end exactly at
        /// this tip — detects tail-truncation and whole-chain deletion. Source it
        /// from your log pipeline, NOT from the same export (an export from a
        /// truncated DB is self-consistent and cannot reveal truncation).
        /// Only valid for a single-org chain — the seq/length check spans the
        /// whole file.
        #[arg(long)]
        expected_tip: Option<String>,
        /// Customer-co-signed checkpoint JSON. The checkpoint's expected tip,
        /// organization, and TokenTrimmer audit key must all match this chain.
        /// Requires an independently obtained customer verifying key.
        #[arg(long, conflicts_with = "expected_tip")]
        customer_checkpoint: Option<String>,
        /// File containing the 64-lowercase-hex customer verifying key.
        #[arg(
            long,
            requires = "customer_checkpoint",
            conflicts_with = "customer_key_hex"
        )]
        customer_key: Option<String>,
        /// 64-lowercase-hex customer verifying key supplied out of band.
        #[arg(
            long,
            requires = "customer_checkpoint",
            conflicts_with = "customer_key"
        )]
        customer_key_hex: Option<String>,
    },
    /// Co-sign an out-of-band audit tip with a customer-controlled Ed25519 key.
    CreateCheckpoint {
        /// Organization UUID whose audit tip is being checkpointed.
        #[arg(long)]
        org: String,
        /// TokenTrimmer audit verifying key bound to the checkpoint.
        #[arg(long)]
        audit_key_hex: String,
        /// Exact out-of-band chain tip as `<seq>:<64-lowercase-hex-hash>`.
        #[arg(long)]
        expected_tip: String,
        /// Mode-0600 file containing a 32-byte Ed25519 seed as lowercase hex.
        #[arg(long)]
        customer_signing_key: String,
        /// New checkpoint JSON path. Existing files are never overwritten.
        #[arg(long)]
        output: String,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CustomerCheckpointInputs<'a> {
    pub path: Option<&'a str>,
    pub key_path: Option<&'a str>,
    pub key_hex: Option<&'a str>,
}

/// Implement `tt audit verify`.
///
/// Loads JSONL entries from `path` (default `.claude/AUDIT-CHAIN.jsonl`).
/// When the first line is the tt-api export preamble
/// (`{"meta":true,"verifying_key":"<hex>",...}`), the verifying key is
/// extracted automatically. Override sources, in priority order:
///
/// 1. `--key-hex <hex>` (inline)
/// 2. `--key <path>` (file containing hex)
/// 3. preamble line
pub(crate) fn run_audit_verify(
    path: Option<&str>,
    org: Option<&str>,
    key_path: Option<&str>,
    key_hex_inline: Option<&str>,
    expected_tip: Option<&str>,
    customer_checkpoint_inputs: CustomerCheckpointInputs<'_>,
) -> anyhow::Result<()> {
    let customer_checkpoint = match customer_checkpoint_inputs.path {
        Some(path) => Some(super::audit_checkpoint::load_and_verify_checkpoint(
            path,
            customer_checkpoint_inputs.key_path,
            customer_checkpoint_inputs.key_hex,
        )?),
        None => {
            if customer_checkpoint_inputs.key_path.is_some()
                || customer_checkpoint_inputs.key_hex.is_some()
            {
                anyhow::bail!(
                    "--customer-key and --customer-key-hex require --customer-checkpoint"
                );
            }
            None
        }
    };
    let chain_path_str = path.unwrap_or(".claude/AUDIT-CHAIN.jsonl");
    let chain_path = Path::new(chain_path_str);
    if !chain_path.exists() {
        // A missing chain cannot satisfy an anchor: if the operator supplied an
        // expected tip, treat the absent file as a verification FAILURE (this is
        // the whole-chain-deletion case the anchor exists to catch). Without an
        // anchor, an absent file is still just an informational no-op.
        if expected_tip.is_some() || customer_checkpoint.is_some() {
            anyhow::bail!(
                "chain verification FAILED: an expected tip was supplied but chain file {} \
                 does not exist (possible whole-chain deletion)",
                chain_path.display()
            );
        }
        tt_cli::ui::note(&format!("no chain to verify ({chain_path_str} not found)"));
        if let Some(o) = org {
            tt_cli::ui::note(&format!(
                "(org filter --org={o} noted; no entries to filter)"
            ));
        }
        return Ok(());
    }

    let content = std::fs::read_to_string(chain_path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", chain_path.display()))?;

    let parsed = parse_chain_jsonl(&content)?;

    let key_hex = if let Some(h) = key_hex_inline {
        h.trim().to_string()
    } else if let Some(p) = key_path {
        std::fs::read_to_string(p)
            .map_err(|e| anyhow::anyhow!("failed to read key file {p}: {e}"))?
            .trim()
            .to_string()
    } else if let Some(h) = parsed.preamble_verifying_key {
        tt_cli::ui::note("verifying-key sourced from export preamble");
        h
    } else {
        anyhow::bail!(
            "no verifying key found: pass --key <path>, --key-hex <hex>, or use an \
             export with a preamble line"
        );
    };

    let key_bytes =
        hex::decode(key_hex.trim()).map_err(|e| anyhow::anyhow!("key hex decode failed: {e}"))?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("verifying key must be exactly 32 bytes (64 hex chars)"))?;
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
        .map_err(|e| anyhow::anyhow!("invalid Ed25519 verifying key: {e}"))?;
    let normalized_key_hex = hex::encode(key_array);

    if let Some(checkpoint) = customer_checkpoint.as_ref() {
        if normalized_key_hex != checkpoint.audit_verifying_key_hex {
            anyhow::bail!(
                "chain verification FAILED: customer checkpoint audit key does not match the selected chain key"
            );
        }
        if parsed.entries.is_empty()
            || parsed
                .entries
                .iter()
                .any(|entry| entry.org_id != checkpoint.organization_id)
        {
            anyhow::bail!(
                "chain verification FAILED: customer checkpoint organization does not match every chain entry"
            );
        }
        if let Some(requested_org) = org {
            let requested_org = requested_org
                .parse::<uuid::Uuid>()
                .context("--org must be a canonical UUID when using a customer checkpoint")?;
            if requested_org != checkpoint.organization_id {
                anyhow::bail!(
                    "chain verification FAILED: --org does not match customer checkpoint organization"
                );
            }
        }
    }

    tt_cli::ui::note(&format!("loaded {} entries", parsed.entries.len()));

    if let Some(o) = org {
        tt_cli::ui::note(&format!(
            "(--org={o} noted; filtering is deferred — verifies full chain)"
        ));
    }

    let supplied_anchor = if let Some(checkpoint) = customer_checkpoint.as_ref() {
        Some(checkpoint.anchor.clone())
    } else {
        expected_tip.map(parse_expected_tip).transpose()?
    };
    let result = match supplied_anchor.as_ref() {
        Some(anchor) => {
            tt_telemetry::audit::verify_chain_with_anchor(&parsed.entries, &verifying_key, anchor)
        }
        None => tt_telemetry::audit::verify_chain(&parsed.entries, &verifying_key),
    };
    match result {
        Ok(()) => {
            let tip_note = if let Some(checkpoint) = customer_checkpoint.as_ref() {
                tt_cli::ui::note(&format!(
                    "customer checkpoint signature matched out-of-band key {}",
                    checkpoint.customer_key_id
                ));
                " (customer-co-signed tip matched)"
            } else if expected_tip.is_some() {
                " (tip anchor matched)"
            } else {
                ""
            };
            tt_cli::ui::ok(&format!(
                "chain OK — all {} entries verified{tip_note}",
                parsed.entries.len()
            ));
        }
        Err(e) => {
            anyhow::bail!("chain verification FAILED: {e}");
        }
    }

    Ok(())
}

/// Parse an `--expected-tip` value of the form `<seq>:<hash>` into a TipAnchor.
/// `seq` must be a non-negative integer; `hash` must be 64 hex chars (a BLAKE3
/// hash), normalized to lowercase. Returns a clear error before verification so
/// a malformed anchor doesn't surface as a confusing TruncatedChain.
pub(crate) fn parse_expected_tip(s: &str) -> anyhow::Result<tt_telemetry::audit::TipAnchor> {
    let (seq_str, hash) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("--expected-tip must be `<seq>:<hash>` (missing ':')"))?;
    let seq: i64 = seq_str
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("--expected-tip seq must be a non-negative integer"))?;
    if seq < 0 {
        anyhow::bail!("--expected-tip seq must be a non-negative integer");
    }
    let hash = hash.trim().to_ascii_lowercase();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("--expected-tip hash must be 64 hex characters (BLAKE3)");
    }
    Ok(tt_telemetry::audit::TipAnchor { seq, hash })
}

/// Result of parsing a JSONL chain file. The preamble line (if present) is
/// stripped out — only real audit entries land in `entries`.
struct ParsedChain {
    entries: Vec<tt_telemetry::audit::AuditEntry>,
    preamble_verifying_key: Option<String>,
}

fn parse_chain_jsonl(content: &str) -> anyhow::Result<ParsedChain> {
    let mut preamble_verifying_key: Option<String> = None;
    let mut entries: Vec<tt_telemetry::audit::AuditEntry> = Vec::new();

    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Try the preamble shape first when we're on the first non-empty
        // line. The preamble carries `"meta": true` so it never collides
        // with a real `AuditEntry`.
        if entries.is_empty() && preamble_verifying_key.is_none() {
            let v: serde_json::Value = serde_json::from_str(trimmed)
                .map_err(|e| anyhow::anyhow!("failed to parse line {} as JSON: {e}", i + 1))?;
            if v.get("meta").and_then(|m| m.as_bool()) == Some(true) {
                preamble_verifying_key = v
                    .get("verifying_key")
                    .and_then(|k| k.as_str())
                    .map(String::from);
                continue;
            }
            // Fall through — not a preamble, parse as entry.
            let entry: tt_telemetry::audit::AuditEntry = serde_json::from_value(v)
                .map_err(|e| anyhow::anyhow!("failed to parse line {} as entry: {e}", i + 1))?;
            entries.push(entry);
            continue;
        }
        let entry: tt_telemetry::audit::AuditEntry = serde_json::from_str(trimmed)
            .map_err(|e| anyhow::anyhow!("failed to parse line {} as entry: {e}", i + 1))?;
        entries.push(entry);
    }

    Ok(ParsedChain {
        entries,
        preamble_verifying_key,
    })
}

#[cfg(test)]
mod audit_verify_tests {
    use super::*;

    #[test]
    fn parses_preamble_line() {
        let content = r#"{"meta":true,"verifying_key":"aa","entry_count":0}"#;
        let parsed = parse_chain_jsonl(content).unwrap();
        assert_eq!(parsed.preamble_verifying_key.as_deref(), Some("aa"));
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn handles_chain_without_preamble() {
        let entry = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "org_id": "00000000-0000-0000-0000-000000000002",
            "timestamp": "2026-05-27T00:00:00Z",
            "actor": {"type": "system"},
            "event": "x",
            "payload": {},
            "seq": 0,
            "prev_hash": "0".repeat(64),
            "hash": "f".repeat(64),
            "signature": "a".repeat(128),
        });
        let content = serde_json::to_string(&entry).unwrap();
        let parsed = parse_chain_jsonl(&content).unwrap();
        assert!(parsed.preamble_verifying_key.is_none());
        assert_eq!(parsed.entries.len(), 1);
    }

    #[test]
    fn ignores_blank_lines() {
        let content = "\n\n";
        let parsed = parse_chain_jsonl(content).unwrap();
        assert!(parsed.preamble_verifying_key.is_none());
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn preamble_then_entries() {
        let entry = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "org_id": "00000000-0000-0000-0000-000000000002",
            "timestamp": "2026-05-27T00:00:00Z",
            "actor": {"type": "system"},
            "event": "x",
            "payload": {},
            "seq": 0,
            "prev_hash": "0".repeat(64),
            "hash": "f".repeat(64),
            "signature": "a".repeat(128),
        });
        let content = format!(
            r#"{{"meta":true,"verifying_key":"deadbeef"}}{}{}"#,
            "\n",
            serde_json::to_string(&entry).unwrap()
        );
        let parsed = parse_chain_jsonl(&content).unwrap();
        assert_eq!(parsed.preamble_verifying_key.as_deref(), Some("deadbeef"));
        assert_eq!(parsed.entries.len(), 1);
    }
}

#[cfg(test)]
mod expected_tip_tests {
    use super::{parse_expected_tip, run_audit_verify, CustomerCheckpointInputs};

    const HASH64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parses_valid_seq_and_hash() {
        let anchor = parse_expected_tip(&format!("42:{HASH64}")).expect("valid");
        assert_eq!(anchor.seq, 42);
        assert_eq!(anchor.hash, HASH64);
    }

    #[test]
    fn uppercase_hash_is_normalized_to_lowercase() {
        let anchor = parse_expected_tip(&format!("0:{}", HASH64.to_uppercase())).expect("valid");
        assert_eq!(anchor.hash, HASH64);
    }

    #[test]
    fn missing_colon_is_rejected() {
        assert!(parse_expected_tip(HASH64).is_err());
    }

    #[test]
    fn non_numeric_seq_is_rejected() {
        assert!(parse_expected_tip(&format!("notanum:{HASH64}")).is_err());
    }

    #[test]
    fn negative_seq_is_rejected() {
        assert!(parse_expected_tip(&format!("-1:{HASH64}")).is_err());
    }

    #[test]
    fn wrong_length_hash_is_rejected() {
        assert!(parse_expected_tip("3:abcd").is_err());
    }

    #[test]
    fn non_hex_hash_is_rejected() {
        let bad = "z".repeat(64);
        assert!(parse_expected_tip(&format!("3:{bad}")).is_err());
    }

    #[test]
    fn missing_chain_file_with_expected_tip_is_error() {
        // A missing file + an anchor must FAIL (whole-chain deletion case),
        // not silently succeed.
        let res = run_audit_verify(
            Some("/nonexistent/tt-audit-chain-test/AUDIT-CHAIN.jsonl"),
            None,
            None,
            None,
            Some("5:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
            CustomerCheckpointInputs::default(),
        );
        assert!(res.is_err(), "missing file + expected_tip must error");
    }

    #[test]
    fn missing_chain_file_without_expected_tip_is_ok() {
        // Back-compat: a missing file with no anchor remains an informational no-op.
        let res = run_audit_verify(
            Some("/nonexistent/tt-audit-chain-test/AUDIT-CHAIN.jsonl"),
            None,
            None,
            None,
            None,
            CustomerCheckpointInputs::default(),
        );
        assert!(
            res.is_ok(),
            "missing file without expected_tip must stay Ok"
        );
    }
}

#[cfg(test)]
mod customer_checkpoint_tests {
    use super::{run_audit_verify, CustomerCheckpointInputs};
    use chrono::{TimeZone as _, Utc};
    use ed25519_dalek::SigningKey;
    use tt_telemetry::audit::{build_entry, Actor, TipAnchor};
    use uuid::Uuid;

    #[test]
    fn customer_checkpoint_verifies_a_real_chain_and_binds_its_org() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chain_path = dir.path().join("audit.jsonl");
        let checkpoint_path = dir.path().join("checkpoint.json");
        let wrong_org_checkpoint_path = dir.path().join("wrong-org-checkpoint.json");

        let audit_signing_key = SigningKey::from_bytes(&[21; 32]);
        let customer_signing_key = SigningKey::from_bytes(&[22; 32]);
        let organization_id = Uuid::from_u128(23);
        let entry = build_entry(
            &audit_signing_key,
            None,
            organization_id,
            Actor::System,
            "checkpoint.test".to_string(),
            serde_json::json!({"bounded": true}),
        )
        .expect("build signed audit entry");
        let audit_key_hex = hex::encode(audit_signing_key.verifying_key().to_bytes());
        let chain = format!(
            "{}\n{}\n",
            serde_json::json!({
                "meta": true,
                // Exercise canonical key comparison: legacy audit verification
                // accepts uppercase key hex in an export preamble.
                "verifying_key": audit_key_hex.to_uppercase(),
                "entry_count": 1
            }),
            serde_json::to_string(&entry).expect("serialize audit entry")
        );
        std::fs::write(&chain_path, chain).expect("write chain");

        let anchor = TipAnchor::from_entry(&entry);
        let checkpointed_at = Utc
            .with_ymd_and_hms(2026, 7, 27, 12, 0, 0)
            .single()
            .expect("fixed timestamp");
        let checkpoint = crate::audit_checkpoint::create_checkpoint(
            organization_id,
            &audit_key_hex,
            &anchor,
            checkpointed_at,
            &customer_signing_key,
        )
        .expect("create checkpoint");
        std::fs::write(
            &checkpoint_path,
            serde_json::to_vec_pretty(&checkpoint).expect("serialize checkpoint"),
        )
        .expect("write checkpoint");

        let customer_key_hex = hex::encode(customer_signing_key.verifying_key().to_bytes());
        run_audit_verify(
            chain_path.to_str(),
            Some(&organization_id.to_string()),
            None,
            None,
            None,
            CustomerCheckpointInputs {
                path: checkpoint_path.to_str(),
                key_path: None,
                key_hex: Some(&customer_key_hex),
            },
        )
        .expect("chain reaches the customer-co-signed checkpoint");

        let wrong_org_checkpoint = crate::audit_checkpoint::create_checkpoint(
            Uuid::from_u128(24),
            &audit_key_hex,
            &anchor,
            checkpointed_at,
            &customer_signing_key,
        )
        .expect("create wrong-org checkpoint");
        std::fs::write(
            &wrong_org_checkpoint_path,
            serde_json::to_vec_pretty(&wrong_org_checkpoint)
                .expect("serialize wrong-org checkpoint"),
        )
        .expect("write wrong-org checkpoint");
        let error = run_audit_verify(
            chain_path.to_str(),
            None,
            None,
            None,
            None,
            CustomerCheckpointInputs {
                path: wrong_org_checkpoint_path.to_str(),
                key_path: None,
                key_hex: Some(&customer_key_hex),
            },
        )
        .expect_err("checkpoint must bind every chain entry to the same organization");
        assert!(error
            .to_string()
            .contains("checkpoint organization does not match every chain entry"));
    }
}
