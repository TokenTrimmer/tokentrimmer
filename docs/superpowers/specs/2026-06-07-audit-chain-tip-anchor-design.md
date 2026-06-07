# Audit-chain tip anchor (cheap-now wiring) — Design

**Status:** approved (design)
**Date:** 2026-06-07
**Slice:** Audit-remediation Wave 4 (public repo, `crates/telemetry` + `crates/cli`). Closes the residual of the audit-chain finding ("Audit chain has no signed tip/length — truncation of recent entries is undetectable", `pub-auth-telemetry`).

## Background (verified against current code)

#68 (Wave 3, `feat(audit): tamper-evident chain — seq-binding + tip anchor`) already shipped most of this finding:
- `seq` is bound into the signed payload (`canonical_payload_bytes` includes `seq`; `PayloadFields.seq`) — renumbering an entry after a deletion breaks its hash + Ed25519 signature.
- `verify_chain` (`crates/telemetry/src/audit/mod.rs`) enforces gap-free, monotonic `seq` (`VerifyError::SeqGap`) — **middle-deletion / reordering is detected**.
- The library primitives `TipAnchor { seq, hash }`, `TipAnchor::from_entry`, `verify_chain_with_anchor`, and `VerifyError::TruncatedChain` exist and are unit-tested in `crates/telemetry/tests/audit.rs` (`test_tip_anchor_detects_truncation`).
- `PostgresAuditWriter::write` writes `seq` gap-free under a SERIALIZABLE tx + `SELECT … FOR UPDATE` and returns the new `AuditEntry` (carrying `seq` + `hash`).

**What is still open (the production gap this slice closes):** nothing in production *uses* the anchor.
- `tt audit verify` (`crates/cli/src/main.rs::run_audit_verify`, ~line 1615) calls plain `verify_chain` — no anchor.
- No code captures or persists a tip out-of-band.

Therefore **tail-truncation (deleting the most recent N entries) and whole-chain deletion remain undetectable in production**: a truncated prefix still verifies (entry 0 → genesis, every retained entry links + signs correctly, seq is gap-free from 0), and an empty chain returns `Ok`. The full automatic answer (S3 Object Lock / WORM auto-anchoring) is explicitly deferred as `post-scale-s3-object-lock`.

**Key architectural constraint (verified):** the anchor must be captured at write-time and stored in a *different trust domain* than the entries. Reusing the existing `tt audit verify` export preamble (`{"meta":true,"verifying_key":…}`, parsed in `parse_chain_jsonl`) as the tip source does **not** work — the export is generated from the same DB, so a truncated DB yields a self-consistent truncated export (matching tip). The anchor must come from out-of-band (the shipped-off-box log stream).

## Decision (user-approved)
Wire the "cheap now" anchor: emit each new tip to the structured log stream on write, and let `tt audit verify` check the chain against an operator-supplied expected tip via the existing `verify_chain_with_anchor`. No new infrastructure; automatic WORM anchoring stays deferred.

## Architecture

### 1. Writer-side tip emission — `crates/telemetry/src/audit/postgres.rs`
In `PostgresAuditWriter::write`, after `tx.commit()` succeeds and before constructing the returned `AuditEntry`, emit:
```rust
tracing::info!(
    target: "tt::audit::tip",
    org_id = %org_id,
    seq = next_seq,
    tip_hash = %hash_hex,
    ts = %timestamp.to_rfc3339(),
    "audit chain tip advanced"
);
```
- `tt-telemetry` already depends on `tracing` (workspace dep); no new dependency.
- Dedicated target `tt::audit::tip` so operators can route these lines to a tamper-resistant, append-only sink independent of the DB.
- **Per-write**, not periodic: audit writes occur only on privileged actions (infrequent), the line is tiny, and each line supersedes the prior — the operator anchors against the highest-`seq` line they have. (Avoids a periodic-checkpoint scheduler — YAGNI here.)
- Emission lives only in the Postgres (production) writer. `InMemoryAuditWriter` (tests/CLI demos) is unchanged.
- Placed after commit so a rolled-back write never emits a tip that doesn't exist.

### 2. CLI verifier anchor — `crates/cli/src/main.rs`
**(a)** Add a flag to `AuditAction::Verify`:
```rust
        /// Expected chain tip as `<seq>:<hash>`, captured out-of-band from the
        /// `tt::audit::tip` log stream. When set, the chain must end exactly at
        /// this tip — detects tail-truncation and whole-chain deletion. Source it
        /// from your log pipeline, NOT from the same export (an export from a
        /// truncated DB is self-consistent and cannot reveal truncation).
        #[arg(long)]
        expected_tip: Option<String>,
```
Thread `expected_tip` through the `Command::Audit { action: AuditAction::Verify { … } }` match arm (~line 423) into `run_audit_verify` as a new `expected_tip: Option<&str>` parameter.

**(b)** Add a pure helper (with the `tt audit verify` impl, near `run_audit_verify`):
```rust
/// Parse an `--expected-tip` value of the form `<seq>:<hash>` into a TipAnchor.
/// `seq` must be a non-negative integer; `hash` must be 64 lowercase hex chars
/// (a BLAKE3 hash). Returns a clear error before verification so a malformed
/// anchor doesn't surface as a confusing TruncatedChain.
fn parse_expected_tip(s: &str) -> anyhow::Result<tt_telemetry::audit::TipAnchor> {
    let (seq_str, hash) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("--expected-tip must be `<seq>:<hash>` (missing ':')"))?;
    let seq: i64 = seq_str
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("--expected-tip seq must be a non-negative integer"))?;
    if seq < 0 {
        anyhow::bail!("--expected-tip seq must be non-negative");
    }
    let hash = hash.trim().to_ascii_lowercase();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("--expected-tip hash must be 64 hex characters (BLAKE3)");
    }
    Ok(tt_telemetry::audit::TipAnchor { seq, hash })
}
```
(`TipAnchor`'s fields are `pub seq: i64` + `pub hash: String` — verified in `mod.rs` — so direct struct construction is fine.)

**(c)** In `run_audit_verify`, replace the single `verify_chain` call with a branch:
```rust
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
        Err(e) => anyhow::bail!("chain verification FAILED: {e}"),
    }
    Ok(())
```
When no `--expected-tip` is given, behavior is unchanged (back-compat).

### 3. Docs
Add a short security note where audit verification is documented (the audit/observability section of `docs/04-gateway-api-reference.md`, or wherever `tt audit verify` is described if that's a more natural home — the implementer places it next to the existing audit docs). A few sentences: tail-truncation/whole-chain deletion are only detectable when `tt audit verify` is given an `--expected-tip` sourced out-of-band from the `tt::audit::tip` log stream (an export from the same DB cannot reveal truncation); the anchor is only as trustworthy as that off-box pipeline; automatic WORM anchoring (S3 Object Lock) is the deferred full solution.

## Data flow
Privileged action → `PostgresAuditWriter::write` appends the entry and emits `tt::audit::tip {org_id, seq, tip_hash, ts}` to the log stream → operator's log pipeline ships it off-box (append-only) → operator captures the latest `seq:tip_hash` → `tt audit verify --expected-tip <seq>:<hash>` runs `verify_chain_with_anchor`, which (i) confirms the chain's last entry is exactly that `(seq, hash)` and that exactly `seq + 1` entries are present (else `TruncatedChain`), then (ii) runs the full `verify_chain` integrity pass.

## Error handling
- `parse_expected_tip` rejects: missing `:`, non-numeric/negative `seq`, hash not 64 hex chars — with specific messages, before verification.
- A short/empty chain or a non-matching tip → `VerifyError::TruncatedChain` → `tt audit verify` exits non-zero with `chain verification FAILED: …`.
- Without `--expected-tip`, the command behaves exactly as today.

## Testing
- **CLI unit tests** for `parse_expected_tip` (in `crates/cli/src/main.rs` `#[cfg(test)]`): valid `"42:" + 64-hex`; uppercase hash normalizes to lowercase and parses; missing colon → err; negative seq (`"-1:…"`) → err; non-numeric seq → err; hash wrong length → err; non-hex hash → err.
- **Anchor verification** logic is already covered by `crates/telemetry/tests/audit.rs::test_tip_anchor_detects_truncation` (full-match passes; truncated tail and empty chain return `TruncatedChain`). No new telemetry test needed for the verifier.
- **Tip emission** is thin `tracing::info!` glue in the Postgres writer. The telemetry crate has **no** DB-backed test harness (audit tests use `InMemoryAuditWriter`) and **no** tracing-capture tooling; rather than add a test-only dependency (`tracing-test`) for a single log line, emission is verified by inspection. The pure value it carries (`TipAnchor::from_entry` equivalent: `seq`/`hash` from the just-written entry) is already covered. This choice is explicit, not a silent skip.

Gates (public repo, scoped per ADR-012 — no workspace-wide builds): `cargo test -p tt-telemetry`; `cargo test -p tt-cli` (incl. the new `parse_expected_tip` tests); `cargo clippy -p tt-telemetry -p tt-cli --all-targets -- -D warnings` clean on touched code; `cargo build -p tt-cli`.

## Out of scope
- Automatic / periodic external anchoring to WORM storage (S3 Object Lock) — deferred `post-scale-s3-object-lock`.
- Signing the checkpoint object — unnecessary: the tip `hash` is only valid against a properly-signed entry, which `verify_chain_with_anchor` re-checks; truncation detection only requires remembering the highest emitted tip.
- A daemon/cron that auto-captures and stores tips — operators wire the `tt::audit::tip` target to their sink; the gateway only emits.
- Per-org `--org` filtering in `tt audit verify` (already a separate, pre-existing "deferred" note in that command).
