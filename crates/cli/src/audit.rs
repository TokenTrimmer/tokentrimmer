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
        /// After chain verification, compute the Merkle root over the entry
        /// hashes and print a machine-readable inclusion proof as JSON.
        /// LOCAL-ONLY: this is membership inside the verified export's own
        /// root — NOT a transparency-log publication and NOT external
        /// timestamping.
        #[arg(long)]
        merkle: bool,
        /// 0-based index of the entry to prove inclusion for (default: the
        /// tip / last entry). Implies `--merkle`.
        #[arg(long)]
        merkle_index: Option<u64>,
    },
    /// Co-sign an out-of-band CUSTOMER checkpoint that binds the exact org,
    /// TokenTrimmer audit verifying key, monotonic sequence, lowercase BLAKE3
    /// tip hash, whole-second UTC time, and a SHA-256 identity of the customer
    /// key. The customer signs with their OWN Ed25519 key; output is
    /// create-only (never overwrites).
    CreateCheckpoint {
        /// Path to the JSONL chain to checkpoint. Defaults to
        /// `.claude/AUDIT-CHAIN.jsonl`.
        #[arg(long)]
        chain: Option<String>,
        /// Canonical organization UUID the chain must belong to (required).
        #[arg(long)]
        org: String,
        /// Path to a file containing the hex-encoded TokenTrimmer Ed25519
        /// verifying key. Overrides the preamble key.
        #[arg(long)]
        key: Option<String>,
        /// Hex-encoded TokenTrimmer Ed25519 verifying key inline.
        #[arg(long)]
        key_hex: Option<String>,
        /// Path to the customer's Ed25519 SIGNING seed (64 hex chars). Must be
        /// mode 0600 on Unix.
        #[arg(long)]
        customer_key: String,
        /// Output path (create-only). Defaults to
        /// `customer-audit-checkpoint.v1.json`.
        #[arg(long)]
        output: Option<String>,
    },
    /// Verify a customer co-signed checkpoint against a locally exported chain.
    /// Requires the customer PUBLIC key out of band, then checks the customer
    /// signature, the chain under the checkpointed TokenTrimmer key, the org of
    /// every row, and the exact co-signed tip.
    VerifyCheckpoint {
        /// Path to the checkpoint artifact.
        #[arg(long)]
        checkpoint: String,
        /// Path to the JSONL chain to verify under the checkpointed key.
        #[arg(long)]
        chain: Option<String>,
        /// Path to a file containing the customer's hex-encoded Ed25519 public
        /// key (64 hex chars).
        #[arg(long)]
        customer_key: Option<String>,
        /// Customer's hex-encoded Ed25519 public key inline.
        #[arg(long)]
        customer_key_hex: Option<String>,
        /// Optional org that must agree with every chain row (and the checkpoint).
        #[arg(long)]
        org: Option<String>,
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
    merkle: bool,
    merkle_index: Option<u64>,
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
            if merkle || merkle_index.is_some() {
                print_merkle_proof(&parsed.entries, org, merkle_index)?;
            }
        }
        Err(e) => {
            anyhow::bail!("chain verification FAILED: {e}");
        }
    }

    Ok(())
}

/// Build the machine-readable Merkle proof JSON for one entry, after chain
/// verification. Pure and side-effect free so tests can assert on it directly.
fn build_merkle_proof_json(
    entries: &[tt_telemetry::audit::AuditEntry],
    org: Option<&str>,
    merkle_index: Option<u64>,
) -> anyhow::Result<serde_json::Value> {
    if entries.is_empty() {
        anyhow::bail!(
            "--merkle requires at least one verified entry; the chain file has none"
        );
    }
    let chain_id = match org {
        Some(o) => o.to_string(),
        None => entries[0].org_id.to_string(),
    };
    let index = merkle_index.unwrap_or((entries.len() - 1) as u64);
    if index >= entries.len() as u64 {
        anyhow::bail!(
            "--merkle-index {index} is out of range: chain has {} entries (valid 0..={})",
            entries.len(),
            entries.len() - 1
        );
    }

    let mut tree =
        tt_telemetry::audit::merkle::IncrementalMerkleTree::with_chain_id(chain_id.clone());
    for entry in entries {
        let leaf = tt_telemetry::audit::merkle::leaf_from_hex(&entry.hash).map_err(|e| {
            anyhow::anyhow!("entry seq {} has an invalid hash field: {e}", entry.seq)
        })?;
        tree.push(leaf);
    }

    let proof = tree
        .prove_inclusion(index)
        .expect("index bounds-checked against entries above");
    let selected = &entries[index as usize];
    let selected_seq = selected.seq;

    // Guards against any inconsistency in the builder itself before we print.
    let selected_leaf =
        tt_telemetry::audit::merkle::leaf_from_hex(&selected.hash)
            .expect("hash decoded above");
    tt_telemetry::audit::merkle::verify_inclusion(&proof, &selected_leaf, &proof.root)
        .map_err(|e| anyhow::anyhow!("internal merkle self-check failed: {e}"))?;

    Ok(serde_json::json!({
        "proof_version": proof.version,
        "chain_id": proof.chain_id,
        "leaf_index": proof.leaf_index,
        "chain_seq": selected_seq,
        "leaf_count": proof.leaf_count,
        "root": proof.root_hex(),
        "sibling_path": proof.sibling_path.iter().map(|s| serde_json::json!({
            "hash": s.hex(),
            "left": s.left,
        })).collect::<Vec<_>>(),
        "local_only": true,
        "note": "local-only inclusion proof over the verified export root; NOT a transparency-log publication",
    }))
}

/// Print a machine-readable Merkle inclusion proof for one entry.
///
/// All data is derived from the already-verified export — no network, no
/// external timestamping, no transparency receipt. The proof asserts membership
/// of one entry hash inside the chain root computed from this file, and is
/// explicitly labeled **local-only**.
fn print_merkle_proof(
    entries: &[tt_telemetry::audit::AuditEntry],
    org: Option<&str>,
    merkle_index: Option<u64>,
) -> anyhow::Result<()> {
    let json = build_merkle_proof_json(entries, org, merkle_index)?;
    println!("{}", serde_json::to_string_pretty(&json)?);
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
            false,
            None,
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
            false,
            None,
        );
        assert!(
            res.is_ok(),
            "missing file without expected_tip must stay Ok"
        );
    }
}

#[cfg(test)]
mod merkle_proof_tests {
    use super::*;
    use tt_telemetry::audit::{build_entry, generate_signing_key, Actor, AuditEntry};

    /// Build a real signed chain of `n` entries for `org`.
    fn build_chain(n: u64, org: uuid::Uuid) -> (ed25519_dalek::SigningKey, Vec<AuditEntry>) {
        let key = generate_signing_key();
        let mut entries: Vec<AuditEntry> = Vec::new();
        for i in 0..n {
            let entry = build_entry(
                &key,
                entries.last(),
                org,
                Actor::System,
                format!("event.{i}"),
                serde_json::json!({"n": i}),
            )
            .expect("entry builds");
            entries.push(entry);
        }
        (key, entries)
    }

    fn expect_error<R>(res: anyhow::Result<R>, needle: &str) {
        match res {
            Ok(_) => panic!("expected error containing {needle:?}"),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains(needle),
                    "expected error containing {needle:?}, got {msg:?}"
                );
            }
        }
    }

    #[test]
    fn merkle_proof_json_matches_tree_and_verifies() {
        let org = uuid::Uuid::new_v4();
        let (_key, entries) = build_chain(9, org);

        let mut tree =
            tt_telemetry::audit::merkle::IncrementalMerkleTree::with_chain_id(org.to_string());
        for e in &entries {
            tree.push(tt_telemetry::audit::merkle::leaf_from_hex(&e.hash).unwrap());
        }

        let json = build_merkle_proof_json(&entries, None, None).expect("tip proof");
        assert_eq!(json["proof_version"], 1);
        assert_eq!(json["chain_id"], org.to_string());
        assert_eq!(json["leaf_index"], 8);
        assert_eq!(json["chain_seq"], entries[8].seq);
        assert_eq!(json["leaf_count"], 9);
        assert_eq!(json["root"], tree.root_hex().unwrap());
        assert_eq!(json["local_only"], true);
        let note = json["note"].as_str().unwrap();
        assert!(note.contains("NOT a transparency-log publication"));
        let path = json["sibling_path"].as_array().unwrap();
        assert!(!path.is_empty());

        // Reconstruct the proof from the JSON and verify it end to end.
        let proof = proof_from_json(&json);
        let selected_leaf = tt_telemetry::audit::merkle::leaf_from_hex(&entries[8].hash).unwrap();
        let root_bytes = hex::decode(json["root"].as_str().unwrap()).unwrap();
        let root: [u8; 32] = root_bytes.try_into().unwrap();
        assert!(
            tt_telemetry::audit::merkle::verify_inclusion(&proof, &selected_leaf, &root).is_ok()
        );
    }

    #[test]
    fn merkle_proof_selected_index_and_org_override() {
        let org = uuid::Uuid::new_v4();
        let (_key, entries) = build_chain(6, org);

        // --merkle-index picks a non-tip leaf; --org overrides chain_id.
        let json =
            build_merkle_proof_json(&entries, Some("org-override"), Some(2)).expect("selected");
        assert_eq!(json["leaf_index"], 2);
        assert_eq!(json["chain_seq"], entries[2].seq);
        assert_eq!(json["chain_id"], "org-override");
        assert_eq!(json["leaf_count"], 6);

        let proof = proof_from_json(&json);
        let leaf = tt_telemetry::audit::merkle::leaf_from_hex(&entries[2].hash).unwrap();
        let root: [u8; 32] =
            hex::decode(json["root"].as_str().unwrap()).unwrap().try_into().unwrap();
        assert!(tt_telemetry::audit::merkle::verify_inclusion(&proof, &leaf, &root).is_ok());

        // A different leaf must NOT verify against this proof.
        let other = tt_telemetry::audit::merkle::leaf_from_hex(&entries[3].hash).unwrap();
        assert!(tt_telemetry::audit::merkle::verify_inclusion(&proof, &other, &root).is_err());
    }

    #[test]
    fn merkle_proof_out_of_range_is_error() {
        let org = uuid::Uuid::new_v4();
        let (_key, entries) = build_chain(3, org);
        expect_error(
            build_merkle_proof_json(&entries, None, Some(99)),
            "out of range",
        );
    }

    #[test]
    fn merkle_proof_empty_chain_is_error() {
        expect_error(build_merkle_proof_json(&[], None, None), "at least one verified entry");
    }

    #[test]
    fn run_audit_verify_accepts_merkle_flag() {
        let org = uuid::Uuid::new_v4();
        let (key, entries) = build_chain(4, org);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_chain_file(dir.path(), &key, &entries);
        let p = path.to_str().unwrap().to_string();

        // --merkle (tip proof) and --merkle-index both succeed.
        run_audit_verify(Some(&p), None, None, None, None, true, None)
            .expect("--merkle verify ok");
        run_audit_verify(Some(&p), None, None, None, None, false, Some(1))
            .expect("--merkle-index implies merkle");
    }

    #[test]
    fn run_audit_verify_rejects_out_of_range_merkle_index() {
        let org = uuid::Uuid::new_v4();
        let (key, entries) = build_chain(2, org);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_chain_file(dir.path(), &key, &entries);
        let p = path.to_str().unwrap().to_string();
        expect_error(
            run_audit_verify(Some(&p), None, None, None, None, false, Some(5)),
            "out of range",
        );
    }

    /// Write a JSONL chain with a verifying-key preamble to `dir`; returns its
    /// path.
    fn write_chain_file(
        dir: &std::path::Path,
        key: &ed25519_dalek::SigningKey,
        entries: &[AuditEntry],
    ) -> std::path::PathBuf {
        let verifying_hex = hex::encode(key.verifying_key().to_bytes());
        let mut content = format!(r#"{{"meta":true,"verifying_key":"{verifying_hex}"}}"#);
        for e in entries {
            content.push('\n');
            content.push_str(&serde_json::to_string(e).expect("serialize entry"));
        }
        let path = dir.join("AUDIT-CHAIN.jsonl");
        std::fs::write(&path, content).expect("write chain file");
        path
    }

    /// Reconstruct an `InclusionProof` from the CLI's machine-readable JSON.
    fn proof_from_json(json: &serde_json::Value) -> tt_telemetry::audit::merkle::InclusionProof {
        use tt_telemetry::audit::merkle::Sibling;
        let root: [u8; 32] = hex::decode(json["root"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let path = json["sibling_path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let hash: [u8; 32] = hex::decode(s["hash"].as_str().unwrap())
                    .unwrap()
                    .try_into()
                    .unwrap();
                Sibling {
                    hash,
                    left: s["left"].as_bool().unwrap(),
                }
            })
            .collect();
        tt_telemetry::audit::merkle::InclusionProof {
            version: json["proof_version"].as_u64().unwrap(),
            chain_id: json["chain_id"].as_str().unwrap().to_string(),
            leaf_index: json["leaf_index"].as_u64().unwrap(),
            leaf_count: json["leaf_count"].as_u64().unwrap(),
            root,
            sibling_path: path,
        }
    }
}

// ─── Customer co-signed audit checkpoint ───────────────────────────────────

fn canonical_hex(value: &str) -> anyhow::Result<String> {
    let trimmed = value.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("expected exactly 64 hex characters, got {trimmed:?}");
    }
    Ok(trimmed.to_lowercase())
}

fn load_customer_signing_key(path: &str) -> anyhow::Result<ed25519_dalek::SigningKey> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .map_err(|e| anyhow::anyhow!("customer seed {path}: {e}"))?;
        let mode = meta.permissions().mode();
        if mode & 0o777 != 0o600 {
            anyhow::bail!(
                "customer seed {} must be mode 0600 (got {:03o}); refusing to use a group/other-readable seed",
                path,
                mode & 0o777
            );
        }
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read customer seed {path}: {e}"))?;
    let hex = content.trim();
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("customer seed must be exactly 64 hex characters");
    }
    let bytes: [u8; 32] = hex::decode(hex)
        .map_err(|e| anyhow::anyhow!("customer seed hex decode: {e}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("customer seed must be exactly 32 bytes (64 hex chars)"))?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

fn write_create_only(path: &str, contents: &str) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!("refusing to overwrite existing checkpoint {path} (create-only)")
            } else {
                anyhow::anyhow!("failed to create {path}: {e}")
            }
        })?;
    use std::io::Write;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// Co-sign a customer audit checkpoint for one completed chain export.
pub(crate) fn run_audit_create_checkpoint(
    chain: Option<&str>,
    org: &str,
    key_path: Option<&str>,
    key_hex_inline: Option<&str>,
    customer_key: &str,
    output: Option<&str>,
) -> anyhow::Result<()> {
    let chain_path_str = chain.unwrap_or(".claude/AUDIT-CHAIN.jsonl");
    let chain_path = Path::new(chain_path_str);
    if !chain_path.exists() {
        anyhow::bail!("checkpoint FAILED: chain file {} does not exist", chain_path.display());
    }
    let content = std::fs::read_to_string(chain_path)
        .map_err(|e| anyhow::anyhow!("failed to read reach chain {}: {e}", chain_path.display()))?;
    let parsed = parse_chain_jsonl(&content)?;
    if parsed.entries.is_empty() {
        anyhow::bail!("checkpoint FAILED: chain {} has no entries to co-sign", chain_path.display());
    }
    let tip = parsed.entries.last().expect("non-empty");
    let tip_hash = tip.hash.trim().to_lowercase();
    if tip_hash.len() != 64 || !tip_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("checkpoint FAILED: chain tip hash is not canonical 64-hex BLAKE3");
    }

    let tt_key_hex = if let Some(hex) = key_hex_inline {
        canonical_hex(hex)?
    } else if let Some(path) = key_path {
        canonical_hex(
            &std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("failed to read key file {path}: {e}"))?,
        )?
    } else if let Some(preamble) = parsed.preamble_verifying_key.as_deref() {
        canonical_hex(preamble)?
    } else {
        anyhow::bail!(
            "checkpoint FAILED: no TokenTrimmer verifying key found — pass --key <path>, \
             --key-hex <hex>, or use an export with a preamble line"
        );
    };

    let customer = load_customer_signing_key(customer_key)?;
    let identity = tt_telemetry::audit::checkpoint::customer_key_identity(&customer.verifying_key());
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let payload = tt_telemetry::audit::checkpoint::CheckpointPayload {
        org: org.trim().to_lowercase(),
        verifying_key_hex: tt_key_hex,
        sequence: tip.seq,
        tip_hash,
        timestamp,
        customer_key_identity: identity,
    };
    let artifact = tt_telemetry::audit::checkpoint::build_checkpoint(&payload, &customer)
        .map_err(|e| anyhow::anyhow!("checkpoint creation failed: {e}"))?;

    let output_path = output
        .map(str::to_string)
        .unwrap_or_else(|| "customer-audit-checkpoint.v1.json".to_string());
    write_create_only(
        &output_path,
        &serde_json::to_string_pretty(&artifact)?,
    )?;
    tt_cli::ui::note(&format!(
        "wrote customer checkpoint {} (org={}, seq={})",
        output_path, payload.org, payload.sequence
    ));
    Ok(())
}

/// Verify a customer checkpoint against a locally exported chain.
pub(crate) fn run_audit_verify_checkpoint(
    checkpoint_path: &str,
    chain: Option<&str>,
    customer_key_path: Option<&str>,
    customer_key_hex_inline: Option<&str>,
    org: Option<&str>,
) -> anyhow::Result<()> {
    let checkpoint_value: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(checkpoint_path)
            .map_err(|e| anyhow::anyhow!("failed to read checkpoint {}: {e}", checkpoint_path))?,
    )
    .map_err(|e| anyhow::anyhow!("checkpoint {} is not valid JSON: {e}", checkpoint_path))?;

    let customer_hex = if let Some(hex) = customer_key_hex_inline {
        hex.trim().to_string()
    } else if let Some(path) = customer_key_path {
        std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read customer key {path}: {e}"))?
            .trim()
            .to_string()
    } else {
        anyhow::bail!(
            "checkpoint verification requires the customer PUBLIC key out of band: \
             pass --customer-key <path> or --customer-key-hex <hex>"
        );
    };
    let key_bytes = hex::decode(canonical_hex(&customer_hex)?)
        .map_err(|e| anyhow::anyhow!("customer key hex decode: {e}"))?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("customer public key must be exactly 32 bytes (64 hex chars)"))?;
    let customer_public = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
        .map_err(|e| anyhow::anyhow!("invalid customer Ed25519 public key: {e}"))?;

    let bound = tt_telemetry::audit::checkpoint::verify_checkpoint(&checkpoint_value, &customer_public)
        .map_err(|e| anyhow::anyhow!("checkpoint signature/identity verification FAILED: {e}"))?;
    if let Some(want) = org {
        if want.trim().to_lowercase() != bound.org {
            anyhow::bail!("checkpoint FAILED: --org={want} disagrees with checkpointed org {}", bound.org);
        }
    }

    let chain_path_str = chain.unwrap_or(".claude/AUDIT-CHAIN.jsonl");
    let chain_path = Path::new(chain_path_str);
    if !chain_path.exists() {
        anyhow::bail!(
            "checkpoint FAILED: chain file {} does not exist (possible whole-chain deletion)",
            chain_path.display()
        );
    }
    let chain_content = std::fs::read_to_string(chain_path)
        .map_err(|e| anyhow::anyhow!("failed to read chain {}: {e}", chain_path.display()))?;
    let parsed = parse_chain_jsonl(&chain_content)?;
    if parsed.entries.is_empty() {
        anyhow::bail!("checkpoint FAILED: chain {} has no entries", chain_path.display());
    }
    // Every row must belong to the checkpointed org.
    let org_checked: Vec<uuid::Uuid> = parsed
        .entries
        .iter()
        .map(|entry| entry.org_id)
        .collect();
    if org_checked.iter().any(|row_org| row_org.to_string() != bound.org) {
        anyhow::bail!(
            "checkpoint FAILED: chain contains rows outside the checkpointed org {}",
            bound.org
        );
    }
    // The chain preamble key (when present) must agree with the checkpointed key.
    if let Some(preamble) = parsed.preamble_verifying_key.as_deref() {
        if canonical_hex(preamble)?.to_lowercase() != bound.verifying_key_hex {
            anyhow::bail!("checkpoint FAILED: chain preamble key disagrees with the checkpointed key");
        }
    }
    let tt_key_bytes = hex::decode(&bound.verifying_key_hex)
        .map_err(|e| anyhow::anyhow!("checkpointed key hex decode: {e}"))?;
    let tt_key_array: [u8; 32] = tt_key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("checkpointed key must be 32 bytes"))?;
    let tt_verifying = ed25519_dalek::VerifyingKey::from_bytes(&tt_key_array)
        .map_err(|e| anyhow::anyhow!("invalid checkpointed TokenTrimmer key: {e}"))?;

    let anchor = tt_telemetry::audit::TipAnchor {
        seq: bound.sequence,
        hash: bound.tip_hash.clone(),
    };
    tt_telemetry::audit::verify_chain_with_anchor(&parsed.entries, &tt_verifying, &anchor)
        .map_err(|e| anyhow::anyhow!("chain under checkpointed key FAILED: {e}"))?;

    tt_cli::ui::note(&format!(
        "checkpoint verified: org={} seq={} tip={}" ,
        bound.org, bound.sequence, &bound.tip_hash[..16]
    ));
    Ok(())
}

#[cfg(test)]
mod checkpoint_cli_tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use std::os::unix::fs::PermissionsExt;

    fn seed() -> (SigningKey, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        // Use `tt_cli::local_audit::append_entry` to build a REAL signed chain.
        let chain = dir.path().join("chain.jsonl");
        (SigningKey::from_bytes(&[3u8; 32]), chain, dir.into_path())
    }

    #[test]
    fn create_then_verify_roundtrip() {
        let (tt_seed, chain, dir) = seed();
        let org = uuid::Uuid::new_v4();
        let tt_key_hex = tt_cli::local_audit::append_entry(
            &chain,
            &tt_seed,
            org,
            "api_key.created",
            serde_json::json!({}),
        )
        .expect("append first entry");

        let customer = SigningKey::from_bytes(&[7u8; 32]);
        let seed_path = dir.join("customer.seed");
        std::fs::write(&seed_path, hex::encode(customer.to_bytes())).unwrap();
        std::fs::set_permissions(&seed_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let checkpoint = dir.join("cp.json");
        run_audit_create_checkpoint(
            Some(chain.to_str().unwrap()),
            &org.to_string(),
            None,
            Some(&tt_key_hex),
            seed_path.to_str().unwrap(),
            Some(checkpoint.to_str().unwrap()),
        )
        .expect("create checkpoint");

        // create-only: a second write into the same path refuses overwrite.
        assert!(
            run_audit_create_checkpoint(
                Some(chain.to_str().unwrap()),
                &org.to_string(),
                None,
                Some(&tt_key_hex),
                seed_path.to_str().unwrap(),
                Some(checkpoint.to_str().unwrap()),
            )
            .is_err(),
            "checkpoint output must be create-only"
        );

        // Round-trip verify with the customer PUBLIC key + the same org.
        let pub_hex = hex::encode(customer.verifying_key().to_bytes());
        run_audit_verify_checkpoint(
            checkpoint.to_str().unwrap(),
            Some(chain.to_str().unwrap()),
            None,
            Some(&pub_hex),
            Some(&org.to_string()),
        )
        .expect("verify roundtrip");

        // Wrong customer public key must fail closed.
        let wrong = hex::encode(SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes());
        assert!(
            run_audit_verify_checkpoint(
                checkpoint.to_str().unwrap(),
                Some(chain.to_str().unwrap()),
                None,
                Some(&wrong),
                Some(&org.to_string()),
            )
            .is_err(),
            "wrong customer key must fail"
        );

        // A different org must not agree with the co-signed org.
        let other = uuid::Uuid::new_v4().to_string();
        assert!(
            run_audit_verify_checkpoint(
                checkpoint.to_str().unwrap(),
                Some(chain.to_str().unwrap()),
                None,
                Some(&pub_hex),
                Some(&other),
            )
            .is_err(),
            "disagreeing --org must fail"
        );

        // A missing chain is the whole-chain-deletion fail-closed case.
        assert!(
            run_audit_verify_checkpoint(
                checkpoint.to_str().unwrap(),
                Some(dir.join("missing.jsonl").to_str().unwrap()),
                None,
                Some(&pub_hex),
                Some(&org.to_string()),
            )
            .is_err(),
            "missing chain must fail closed"
        );
    }

    #[test]
    fn refuses_a_group_or_other_readable_customer_seed() {
        let (tt_seed, chain, dir) = seed();
        let org = uuid::Uuid::new_v4();
        let tt_key_hex = tt_cli::local_audit::append_entry(
            &chain,
            &tt_seed,
            org,
            "api_key.created",
            serde_json::json!({}),
        )
        .unwrap();

        let seed_path = dir.join("loose.seed");
        std::fs::write(&seed_path, hex::encode([7u8; 32])).unwrap();
        std::fs::set_permissions(&seed_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let result = run_audit_create_checkpoint(
            Some(chain.to_str().unwrap()),
            &org.to_string(),
            None,
            Some(&tt_key_hex),
            seed_path.to_str().unwrap(),
            Some(dir.join("cp.json").to_str().unwrap()),
        );
        assert!(result.is_err(), "mode-0644 customer seed must be refused on Unix");
    }
}
