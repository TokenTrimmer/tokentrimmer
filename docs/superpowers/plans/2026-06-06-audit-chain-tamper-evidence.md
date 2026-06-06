# Audit-chain tamper-evidence (seq-binding + tip anchor) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bind `seq` into the audit chain's signed hash and add gap-free + tip-anchor verification so tail-truncation, reorder/renumber, and whole-chain-deletion are detectable.

**Architecture:** In `crates/telemetry/src/audit`: add `seq` to `PayloadFields`/`canonical_payload_bytes` (hash binding) and to `AuditEntry`; wire `seq` through both writers; `verify_chain` keeps its signature but recomputes with `seq` and enforces a gap-free monotonic sequence; a new `verify_chain_with_anchor` + `TipAnchor` add the tip/length assertion. Clean-break hash format (pre-GA).

**Tech Stack:** Rust, BLAKE3 + Ed25519 (`ed25519_dalek`), `InMemoryAuditWriter` for DB-free tests.

---

### Task 1: Bind `seq` into the hash + carry it on `AuditEntry`

**Files:**
- Modify: `crates/telemetry/src/audit/mod.rs` (`PayloadFields`, `canonical_payload_bytes`, `AuditEntry`, `verify_chain`'s `PayloadFields` build)
- Modify: `crates/telemetry/src/audit/postgres.rs` (`write` returned entry, `AuditRow::into_entry`)
- Modify: `crates/telemetry/src/audit/writer.rs` (`InMemoryAuditWriter::write`)
- Test: `crates/telemetry/tests/audit.rs`

- [ ] **Step 1: Write the failing test (seq is covered by the hash)**

Add to `crates/telemetry/tests/audit.rs`:

```rust
#[test]
fn canonical_payload_includes_seq() {
    use chrono::Utc;
    use tt_telemetry::audit::{canonical_payload_bytes, Actor, PayloadFields};
    use uuid::Uuid;

    let id = Uuid::nil();
    let org = Uuid::nil();
    let ts = Utc::now();
    let actor = Actor::System;
    let payload = serde_json::json!({"k": "v"});
    let mk = |seq| {
        canonical_payload_bytes(&PayloadFields {
            id,
            org_id: org,
            timestamp: ts,
            actor: &actor,
            event: "e",
            payload: &payload,
            seq,
        })
        .unwrap()
    };
    assert_ne!(mk(0), mk(1), "seq must change the canonical bytes (so it is hashed/signed)");
}
```

Run: `cargo test -p tt-telemetry --test audit canonical_payload_includes_seq 2>&1 | tail -20`
Expected: FAIL to compile — `PayloadFields` has no `seq` field.

- [ ] **Step 2: Add `seq` to `PayloadFields` + `canonical_payload_bytes`**

In `crates/telemetry/src/audit/mod.rs`, add a field to `PayloadFields`:
```rust
    /// Arbitrary payload.
    pub payload: &'a serde_json::Value,
    /// Monotonic per-org sequence number (0-based). Bound into the hash so an
    /// entry's position cannot be changed without breaking its signature.
    pub seq: i64,
```
And include it in `canonical_payload_bytes`'s object:
```rust
    let obj = serde_json::json!({
        "id": entry_fields.id.to_string(),
        "org_id": entry_fields.org_id.to_string(),
        "timestamp": entry_fields.timestamp.to_rfc3339(),
        "actor": entry_fields.actor,
        "event": entry_fields.event,
        "payload": entry_fields.payload,
        "seq": entry_fields.seq,
    });
```

- [ ] **Step 3: Add `seq` to `AuditEntry`**

In `crates/telemetry/src/audit/mod.rs`, add to `AuditEntry` (after `org_id`, to keep the natural order):
```rust
    /// Organization this entry belongs to.
    pub org_id: Uuid,
    /// Monotonic per-org sequence number (0-based, gap-free).
    pub seq: i64,
```

- [ ] **Step 4: Build `PayloadFields` with `seq` in `verify_chain`**

In `verify_chain` (`mod.rs`), the per-entry `PayloadFields { … }` gains `seq: entry.seq,`:
```rust
        let fields = PayloadFields {
            id: entry.id,
            org_id: entry.org_id,
            timestamp: entry.timestamp,
            actor: &entry.actor,
            event: &entry.event,
            payload: &entry.payload,
            seq: entry.seq,
        };
```

- [ ] **Step 5: Wire `seq` through the Postgres writer**

In `crates/telemetry/src/audit/postgres.rs` `write`: add `seq: next_seq,` to the `PayloadFields` build, and `seq: next_seq,` to the returned `AuditEntry { … }`.
In `AuditRow::into_entry`: remove the `#[allow(dead_code)]` on the `seq` field and add `seq: self.seq,` to the `AuditEntry { … }`.

- [ ] **Step 6: Wire `seq` through the in-memory writer**

In `crates/telemetry/src/audit/writer.rs` `InMemoryAuditWriter::write`, compute the seq from the chain length before building fields:
```rust
        let seq = chain.len() as i64;
```
(place it right after `let chain = guard.entry(org_id).or_default();`), add `seq,` to the `PayloadFields` build and `seq,` to the `AuditEntry { … }`.

- [ ] **Step 7: Run — new test passes, existing audit tests still pass**

Run: `cargo test -p tt-telemetry --test audit 2>&1 | grep -E "test result:|FAILED|canonical_payload" | tail`
Expected: `canonical_payload_includes_seq` passes; all existing tests pass (writers produce gap-free seq; the tamper/broken/signature tests don't touch `seq`, so they still hit their asserted variants).

- [ ] **Step 8: Commit**

```bash
git add crates/telemetry/src/audit crates/telemetry/tests/audit.rs
git commit -m "feat(audit): bind seq into the signed hash + carry it on AuditEntry"
```

---

### Task 2: Gap-free sequence check in `verify_chain`

**Files:**
- Modify: `crates/telemetry/src/audit/mod.rs` (`VerifyError`, `verify_chain`)
- Test: `crates/telemetry/tests/audit.rs`

- [ ] **Step 1: Write the failing test (a seq gap is rejected)**

Add to `tests/audit.rs` (mirror the existing tamper tests' writer+list+vk setup; capture `vk` the same way the sibling tests do):

```rust
#[tokio::test]
async fn test_seq_gap_detected() {
    let writer = InMemoryAuditWriter::new();
    let vk = writer.verifying_key();
    let org = uuid::Uuid::new_v4();
    for i in 0..3 {
        writer.write(org, Actor::System, format!("e{i}"), serde_json::json!({}))
            .await
            .unwrap();
    }
    let mut entries = writer.list(org).await.unwrap();
    // Drop the middle entry → remaining seqs are 0,2 (a gap), prev_hash also breaks.
    entries.remove(1);
    let err = verify_chain(&entries, &vk).expect_err("gap must fail");
    assert!(
        matches!(err, VerifyError::SeqGap { .. } | VerifyError::BrokenChain { .. }),
        "got {err:?}"
    );
}
```

(If `writer.verifying_key()` is not the accessor, use the same expression the existing `tests/audit.rs` tests use to obtain `vk`.)

Run: `cargo test -p tt-telemetry --test audit test_seq_gap_detected 2>&1 | tail -20`
Expected: FAIL to compile — `VerifyError::SeqGap` does not exist.

- [ ] **Step 2: Add the `SeqGap` variant**

In `mod.rs` `VerifyError`:
```rust
    /// An entry's `seq` is not the expected gap-free successor.
    #[error("entry {index} has wrong seq (expected {expected}, got {got})")]
    SeqGap {
        /// Zero-based index of the offending entry.
        index: usize,
        /// The seq value that was expected (0 for genesis, prev+1 otherwise).
        expected: i64,
        /// The seq value actually stored.
        got: i64,
    },
```

- [ ] **Step 3: Enforce gap-free seq in `verify_chain`**

In `verify_chain`, at the top of the per-entry loop body (before the `prev_hash` linkage check), add:
```rust
        // ── 0. seq must be gap-free and monotonic ─────────────────────────────
        let expected_seq = if i == 0 { 0 } else { entries[i - 1].seq + 1 };
        if entry.seq != expected_seq {
            return Err(VerifyError::SeqGap {
                index: i,
                expected: expected_seq,
                got: entry.seq,
            });
        }
```

- [ ] **Step 4: Run — passes**

Run: `cargo test -p tt-telemetry --test audit test_seq_gap_detected 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/telemetry/src/audit/mod.rs crates/telemetry/tests/audit.rs
git commit -m "feat(audit): verify_chain enforces gap-free monotonic seq"
```

---

### Task 3: `TipAnchor` + `verify_chain_with_anchor`

**Files:**
- Modify: `crates/telemetry/src/audit/mod.rs` (`TipAnchor`, `VerifyError::TruncatedChain`, `verify_chain_with_anchor`)
- Modify: `crates/telemetry/src/audit/mod.rs` re-exports if needed (or `lib.rs`)
- Test: `crates/telemetry/tests/audit.rs`

- [ ] **Step 1: Write the failing tests (anchor catches truncation)**

Add to `tests/audit.rs`:

```rust
#[tokio::test]
async fn test_tip_anchor_detects_truncation() {
    use tt_telemetry::audit::{verify_chain_with_anchor, TipAnchor};
    let writer = InMemoryAuditWriter::new();
    let vk = writer.verifying_key();
    let org = uuid::Uuid::new_v4();
    for i in 0..3 {
        writer.write(org, Actor::System, format!("e{i}"), serde_json::json!({}))
            .await
            .unwrap();
    }
    let entries = writer.list(org).await.unwrap();
    let anchor = TipAnchor::from_entry(entries.last().unwrap());

    // Full chain verifies against its own tip.
    verify_chain_with_anchor(&entries, &vk, &anchor).expect("full chain matches anchor");

    // Drop the last entry → chain no longer reaches the recorded tip.
    let truncated = &entries[..entries.len() - 1];
    let err = verify_chain_with_anchor(truncated, &vk, &anchor)
        .expect_err("truncated chain must fail against the old tip");
    assert!(matches!(err, VerifyError::TruncatedChain { .. }), "got {err:?}");

    // Empty chain + anchor → also TruncatedChain.
    let err2 = verify_chain_with_anchor(&[], &vk, &anchor).expect_err("empty must fail");
    assert!(matches!(err2, VerifyError::TruncatedChain { .. }), "got {err2:?}");
}
```

Run: `cargo test -p tt-telemetry --test audit test_tip_anchor_detects_truncation 2>&1 | tail -20`
Expected: FAIL to compile — `TipAnchor` / `verify_chain_with_anchor` / `TruncatedChain` do not exist.

- [ ] **Step 2: Add `TruncatedChain` variant + `TipAnchor`**

In `mod.rs` `VerifyError`:
```rust
    /// The chain is shorter than the anchor says it should be (tail-truncation
    /// or whole-chain deletion), or its tip does not match the anchor.
    #[error("chain does not reach anchor tip: expected len {expected_len}, got {got_len}")]
    TruncatedChain {
        /// Entries the anchor implies (`anchor.seq + 1`).
        expected_len: i64,
        /// Entries actually present.
        got_len: i64,
    },
```

Add the `TipAnchor` type (near `AuditEntry`):
```rust
/// A captured "tip" of an audit chain — the last entry's seq + hash. Hold one
/// (anchored externally; see the deferred WORM item) and pass it to
/// [`verify_chain_with_anchor`] to detect tail-truncation / whole-chain deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipAnchor {
    /// `seq` of the expected last entry.
    pub seq: i64,
    /// Hex BLAKE3 `hash` of the expected last entry.
    pub hash: String,
}

impl TipAnchor {
    /// Capture the current tip from the chain's last entry.
    #[must_use]
    pub fn from_entry(entry: &AuditEntry) -> Self {
        Self {
            seq: entry.seq,
            hash: entry.hash.clone(),
        }
    }
}
```

- [ ] **Step 3: Add `verify_chain_with_anchor`**

In `mod.rs`, after `verify_chain`:
```rust
/// Verify a chain AND assert it still reaches `anchor`'s tip (seq + hash) with
/// no missing tail. Runs [`verify_chain`] first, then checks the tip — this is
/// what makes tail-truncation and whole-chain deletion detectable, given a
/// trustworthy `anchor` captured earlier.
pub fn verify_chain_with_anchor(
    entries: &[AuditEntry],
    verifying_key: &ed25519_dalek::VerifyingKey,
    anchor: &TipAnchor,
) -> Result<(), VerifyError> {
    let expected_len = anchor.seq + 1;
    let got_len = entries.len() as i64;
    match entries.last() {
        Some(last) if last.seq == anchor.seq && last.hash == anchor.hash && got_len == expected_len => {
            // Tip matches and length is consistent — now verify the chain itself.
            verify_chain(entries, verifying_key)
        }
        _ => Err(VerifyError::TruncatedChain {
            expected_len,
            got_len,
        }),
    }
}
```

(Confirm `TipAnchor` + `verify_chain_with_anchor` are exported wherever `verify_chain` is — they live in the same module, which is already re-exported via `pub mod`/`pub use`; if `lib.rs` enumerates re-exports, add them there.)

- [ ] **Step 4: Run — passes**

Run: `cargo test -p tt-telemetry --test audit test_tip_anchor_detects_truncation 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/telemetry/src/audit crates/telemetry/tests/audit.rs
git commit -m "feat(audit): TipAnchor + verify_chain_with_anchor (detect tail-truncation)"
```

---

### Task 4: Gates across all `verify_chain` consumers

**Files:** none (verification only); fix any caller that needs the new `seq` field.

- [ ] **Step 1: Build the whole workspace (catch any AuditEntry constructor that needs `seq`)**

Run: `cargo build --workspace 2>&1 | grep -E "error\[|missing field|Finished" | tail -20`
Expected: compiles. If any non-test `AuditEntry { … }` constructor is missing `seq`, add it (the audit write paths in Task 1 are the only known producers; the cloud repo is a separate workspace and uses the published types — out of scope here).

- [ ] **Step 2: Test all consumer crates**

Run: `cargo test -p tt-telemetry -p tt-auth -p tt-plan-core -p tt-cli 2>&1 | grep -E "test result:|error\[|FAILED" | tail -20`
Expected: all pass. (`tt-auth`/`tt-plan-core`/`tt-cli` call `verify_chain` with entries produced by the writers, which now carry gap-free `seq`, so they still verify.)

- [ ] **Step 3: Clippy + fmt**

Run: `cargo clippy -p tt-telemetry -p tt-auth -p tt-plan-core -p tt-cli --all-targets -- -D warnings 2>&1 | grep -E "^warning:|^error" | grep -v "Permission denied\|auto-clean" | tail -10`
Expected: clean.

Run: `cargo fmt && cargo fmt -- --check 2>&1 | tail -3`
Expected: clean.

- [ ] **Step 4: Commit any residual fixes**

```bash
git status --porcelain
git diff --quiet || (git add -A && git commit -m "style: cargo fmt / caller fixups for audit seq")
```

- [ ] **Step 5: Confirm scope**

Run: `git diff main --stat`
Expected: `crates/telemetry/src/audit/{mod.rs,postgres.rs,writer.rs}`, `crates/telemetry/tests/audit.rs` (+ spec/plan docs). No behavior change outside the audit module.
