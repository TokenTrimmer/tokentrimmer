//! `tt audit` command implementation.

use std::path::Path;

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
        /// Filter to a specific org UUID (recorded but not yet enforced — all
        /// entries in the file are verified regardless).
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
    },
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
) -> anyhow::Result<()> {
    let chain_path_str = path.unwrap_or(".claude/AUDIT-CHAIN.jsonl");
    let chain_path = Path::new(chain_path_str);
    if !chain_path.exists() {
        // A missing chain cannot satisfy an anchor: if the operator supplied an
        // expected tip, treat the absent file as a verification FAILURE (this is
        // the whole-chain-deletion case the anchor exists to catch). Without an
        // anchor, an absent file is still just an informational no-op.
        if expected_tip.is_some() {
            anyhow::bail!(
                "chain verification FAILED: --expected-tip was supplied but chain file {} \
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

    tt_cli::ui::note(&format!("loaded {} entries", parsed.entries.len()));

    if let Some(o) = org {
        tt_cli::ui::note(&format!(
            "(--org={o} noted; filtering is deferred — verifies full chain)"
        ));
    }

    let result = match expected_tip {
        Some(tip_str) => {
            let anchor = parse_expected_tip(tip_str)?;
            tt_telemetry::audit::verify_chain_with_anchor(&parsed.entries, &verifying_key, &anchor)
        }
        None => tt_telemetry::audit::verify_chain(&parsed.entries, &verifying_key),
    };
    match result {
        Ok(()) => {
            let tip_note = if expected_tip.is_some() {
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
fn parse_expected_tip(s: &str) -> anyhow::Result<tt_telemetry::audit::TipAnchor> {
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
    use super::{parse_expected_tip, run_audit_verify};

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
        );
        assert!(
            res.is_ok(),
            "missing file without expected_tip must stay Ok"
        );
    }
}
