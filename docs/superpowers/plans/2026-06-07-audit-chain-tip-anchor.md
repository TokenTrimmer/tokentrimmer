# Audit-chain tip anchor (cheap-now wiring) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make audit-chain tail-truncation and whole-chain deletion detectable in production by emitting each new chain tip to the log stream and letting `tt audit verify` check the chain against an operator-supplied expected tip.

**Architecture:** The Postgres audit writer emits a structured `tt::audit::tip` tracing event (org_id, seq, hash, ts) after each committed append — an out-of-band anchor shipped off-box by the log pipeline. `tt audit verify` gains `--expected-tip <seq>:<hash>`; when set it calls the existing `verify_chain_with_anchor` (shipped in #68) instead of plain `verify_chain`. No new infrastructure; automatic WORM anchoring stays deferred.

**Tech Stack:** Rust (`crates/telemetry` with the `postgres` feature, `crates/cli` with clap), `tracing`, `ed25519-dalek`, `blake3`.

Spec: `docs/superpowers/specs/2026-06-07-audit-chain-tip-anchor-design.md`

> **REPO CAVEATS (public OSS repo):** Use **scoped** cargo commands only (ADR-012 — no workspace-wide builds). The audit writer is behind the `postgres` feature, so build/clippy/test it with `--features postgres` (or via `tt-cli`, which already enables it). `crates/cli/src/main.rs` is a large pre-existing file (~1876 lines, already over the ADR-011 800-line cap) — do NOT restructure it; add the small new code + a focused test module cohesively.

---

### Task 1: Emit the chain tip on each write

**Files:**
- Modify: `crates/telemetry/src/audit/postgres.rs` (in `PostgresAuditWriter::write`, after `tx.commit()`)

This is a single structured-log emission. The telemetry crate has no DB-backed or tracing-capture test harness (its audit tests use `InMemoryAuditWriter`), and adding a test-only `tracing` capture dependency for one log line is not worth it — so this task is verified by compile + clippy + inspection, not a unit test. (The value it carries — `seq`/`hash` of the just-written entry — is already exercised by `TipAnchor` tests in `crates/telemetry/tests/audit.rs`.)

- [ ] **Step 1: Add the tip emission after commit**

In `crates/telemetry/src/audit/postgres.rs`, in the `write` method, locate the `tx.commit()` block:
```rust
        tx.commit()
            .await
            .map_err(|e| AuditError::Storage(e.to_string()))?;

        Ok(AuditEntry {
```
Insert the emission between the `tx.commit()?` and the `Ok(AuditEntry {` return:
```rust
        tx.commit()
            .await
            .map_err(|e| AuditError::Storage(e.to_string()))?;

        // Emit the new chain tip on a dedicated target so operators can route it
        // to an append-only, off-box sink. This out-of-band anchor is what makes
        // tail-truncation / whole-chain deletion detectable later via
        // `tt audit verify --expected-tip` (the DB alone cannot reveal it).
        // Per-write is fine: audit writes happen only on privileged actions.
        tracing::info!(
            target: "tt::audit::tip",
            org_id = %org_id,
            seq = next_seq,
            tip_hash = %hash_hex,
            ts = %timestamp.to_rfc3339(),
            "audit chain tip advanced"
        );

        Ok(AuditEntry {
```
All referenced bindings (`org_id`, `next_seq`, `hash_hex`, `timestamp`) are already in scope at that point in `write`.

- [ ] **Step 2: Verify it compiles + clippy is clean (postgres feature)**

Run: `cargo clippy -p tt-telemetry --features postgres --all-targets -- -D warnings 2>&1 | tail -15`
Expected: clean — no warnings/errors on `postgres.rs`.

- [ ] **Step 3: Run the telemetry tests (no regression)**

Run: `cargo test -p tt-telemetry --features postgres 2>&1 | tail -15`
Expected: PASS — existing audit tests (incl. `test_tip_anchor_detects_truncation`) still green; nothing new to add here.

- [ ] **Step 4: Commit (stage only postgres.rs)**

```bash
git add crates/telemetry/src/audit/postgres.rs
git commit -m "feat(audit): emit chain tip on each write (tt::audit::tip)

Out-of-band anchor enabling tail-truncation detection via verify_chain_with_anchor.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `tt audit verify --expected-tip` + docs

**Files:**
- Modify: `crates/cli/src/main.rs` (`AuditAction::Verify` variant; the `Command::Audit` match arm; `run_audit_verify`; add `parse_expected_tip` + a test module)
- Modify: `docs/04-gateway-api-reference.md` (short security note in the audit section)

- [ ] **Step 1: Write the failing unit tests for `parse_expected_tip`**

In `crates/cli/src/main.rs`, add a new test module at the end of the file (after the existing `#[cfg(test)]` blocks):
```rust
#[cfg(test)]
mod expected_tip_tests {
    use super::parse_expected_tip;

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
}
```

- [ ] **Step 2: Run to confirm it fails to compile**

Run: `cargo test -p tt-cli expected_tip 2>&1 | tail -15`
Expected: FAIL — `parse_expected_tip` not found.

- [ ] **Step 3: Add the `parse_expected_tip` helper**

In `crates/cli/src/main.rs`, near `run_audit_verify` (e.g. immediately after it, before `struct ParsedChain`), add:
```rust
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
        anyhow::bail!("--expected-tip seq must be non-negative");
    }
    let hash = hash.trim().to_ascii_lowercase();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("--expected-tip hash must be 64 hex characters (BLAKE3)");
    }
    Ok(tt_telemetry::audit::TipAnchor { seq, hash })
}
```
(`TipAnchor` has public fields `pub seq: i64` and `pub hash: String`, so direct construction works.)

- [ ] **Step 4: Run the unit tests to confirm they pass**

Run: `cargo test -p tt-cli expected_tip 2>&1 | tail -15`
Expected: PASS — all 7 `expected_tip_tests` green.

- [ ] **Step 5: Add the `--expected-tip` flag to the `Verify` subcommand**

In `crates/cli/src/main.rs`, in `enum AuditAction`'s `Verify { … }` variant, after the `key_hex` field, add:
```rust
        /// Expected chain tip as `<seq>:<hash>`, captured out-of-band from the
        /// `tt::audit::tip` log stream. When set, the chain must end exactly at
        /// this tip — detects tail-truncation and whole-chain deletion. Source it
        /// from your log pipeline, NOT from the same export (an export from a
        /// truncated DB is self-consistent and cannot reveal truncation).
        #[arg(long)]
        expected_tip: Option<String>,
```

- [ ] **Step 6: Thread the flag through the dispatch match arm**

In `crates/cli/src/main.rs`, the `Command::Audit { action: AuditAction::Verify { … } }` arm destructures the fields and calls `run_audit_verify`. Update both the destructure and the call:
```rust
        Command::Audit {
            action:
                AuditAction::Verify {
                    path,
                    org,
                    key,
                    key_hex,
                    expected_tip,
                },
        } => {
            run_audit_verify(
                path.as_deref(),
                org.as_deref(),
                key.as_deref(),
                key_hex.as_deref(),
                expected_tip.as_deref(),
            )?;
        }
```

- [ ] **Step 7: Add the `expected_tip` parameter to `run_audit_verify` and branch the verification**

In `crates/cli/src/main.rs`, change the `run_audit_verify` signature to add a final parameter:
```rust
fn run_audit_verify(
    path: Option<&str>,
    org: Option<&str>,
    key_path: Option<&str>,
    key_hex_inline: Option<&str>,
    expected_tip: Option<&str>,
) -> anyhow::Result<()> {
```
Then replace the final verification block (the `match tt_telemetry::audit::verify_chain(&parsed.entries, &verifying_key) { … }` near the end of the function) with:
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
        Err(e) => {
            anyhow::bail!("chain verification FAILED: {e}");
        }
    }

    Ok(())
```
(Leave the rest of `run_audit_verify` — the no-file early return, key resolution, `parse_chain_jsonl`, the `loaded N entries` / `--org` notes — unchanged.)

- [ ] **Step 8: Verify CLI compiles, clippy clean, tests pass**

Run: `cargo clippy -p tt-cli --all-targets -- -D warnings 2>&1 | tail -15` → clean.
Run: `cargo test -p tt-cli 2>&1 | tail -15` → PASS (incl. the 7 new `expected_tip_tests`).
Run: `cargo build -p tt-cli 2>&1 | tail -5` → builds.

- [ ] **Step 9: Add the docs security note**

In `docs/04-gateway-api-reference.md`, find the audit-chain / `tt audit verify` documentation section. Add a short note (adapt wording to the surrounding doc style):
```markdown
> **Detecting truncation.** `tt audit verify` confirms each entry links and is
> signed, and that `seq` is gap-free — so reordering or a mid-chain deletion is
> caught. It cannot, on its own, detect deletion of the most recent entries or of
> the entire chain (a truncated prefix still verifies). To detect that, pass
> `tt audit verify --expected-tip <seq>:<hash>`, where the tip is captured
> **out-of-band** from the gateway's `tt::audit::tip` log stream (shipped to an
> append-only sink). Do not source the tip from the same export — an export taken
> from a truncated database is self-consistent. The anchor is only as trustworthy
> as that off-box log pipeline; automatic WORM anchoring (S3 Object Lock) is the
> deferred full solution.
```
If `docs/04-gateway-api-reference.md` has no audit section, place the note wherever `tt audit verify` is documented (search the `docs/` tree for `audit verify`); keep it next to the existing audit content.

- [ ] **Step 10: Commit (stage only the two files)**

```bash
git add crates/cli/src/main.rs docs/04-gateway-api-reference.md
git commit -m "feat(cli): tt audit verify --expected-tip detects chain truncation

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-telemetry --features postgres 2>&1 | tail -10
cargo test -p tt-cli 2>&1 | tail -10
cargo clippy -p tt-telemetry --features postgres --all-targets -- -D warnings 2>&1 | tail -10
cargo clippy -p tt-cli --all-targets -- -D warnings 2>&1 | tail -10
```
All green. **Stage only changed files.** Scoped cargo only (ADR-012).

## Notes for the implementer
- The verifier logic (`verify_chain_with_anchor`, `TipAnchor`, `VerifyError::TruncatedChain`) already exists from #68 — this slice only *wires* it: emit tips (Task 1) and expose the anchor input in the CLI (Task 2). Do not re-implement the verifier.
- The anchor must be out-of-band — that's the whole point. The CLI help text and docs must steer operators away from sourcing the tip from the same export.
- Per-write emission is intentional (audit writes are infrequent privileged actions); do not add a periodic-checkpoint scheduler (deferred / YAGNI).
- Tasks are independent (different crates) and each leaves the build green; either order works.
