# Audit-chain tamper-evidence: seq-binding + tip anchor (W3) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** Audit-remediation Wave 3 (public repo, `crates/telemetry`). Close the audit finding that the tamper-evident chain can be silently truncated: `seq` is stored but not hashed/signed, and `verify_chain` checks neither chain length nor the tip.

## Goal

Make the audit chain detect **tail-truncation, reorder/renumber, and whole-chain-deletion** — not only the middle-deletion that `prev_hash` linkage already catches. Two cheap-now mechanisms: bind `seq` into the signed hash, and let verification assert a gap-free sequence plus an expected tip. External/WORM anchoring stays deferred.

## Background (verified)

- `crates/telemetry/src/audit/mod.rs`:
  - `canonical_payload_bytes` (`:143-155`) hashes `{id, org_id, timestamp, actor, event, payload}` — **no `seq`**.
  - `compute_hash` = BLAKE3(`prev_hash_bytes || canonical_payload_bytes`); the Ed25519 signature is over that 32-byte hash.
  - `verify_chain(entries, vk)` (`:199-266`) checks, per entry: `prev_hash` linkage, hash recomputation, signature. Nothing about length/tip/seq. Empty slice → `Ok`.
  - `AuditEntry` (`:35`) has **no `seq`** field.
  - `VerifyError` variants: `BrokenChain`, `HashMismatch`, `BadSignature`, `Hex`.
- `crates/telemetry/src/audit/postgres.rs`: `write` computes a monotonic per-org `next_seq` under `FOR UPDATE` and persists it. The read path's `AuditRow` **already `SELECT`s `seq`** but `into_entry` (`:194`) drops it (`#[allow(dead_code)]`).
- `InMemoryAuditWriter` (`audit/writer.rs`) — used by tests; lets us round-trip write→list→verify without Postgres.
- `verify_chain` callers (must keep working): `crates/cli/src/main.rs:1615` (`tt audit verify`), `crates/auth/tests/keys.rs:350`, `crates/plan-core/src/apply.rs:380`, and `crates/telemetry/tests/audit.rs` (many).
- Decision (user): **clean-break** hash format — new entries include `seq`; no version field. Platform is pre-GA/deploy-blocked, so there is no meaningful production audit data; existing dev/staging chains must be re-seeded (documented).

## Architecture (`crates/telemetry/src/audit`)

### 1. Bind `seq` into the hash
- Add `seq: i64` to `PayloadFields`.
- Add `"seq": entry_fields.seq` to the object in `canonical_payload_bytes`.
Because the signature is over the resulting hash, each entry's position is now cryptographically committed — a renumber/reorder breaks both hash and signature.

### 2. Carry `seq` on `AuditEntry`
- Add `pub seq: i64` to `AuditEntry`.
- `postgres::write`: pass `next_seq` into `PayloadFields` and set it on the returned `AuditEntry`.
- `postgres::AuditRow::into_entry`: populate `seq: self.seq` (drop the `#[allow(dead_code)]`).
- `InMemoryAuditWriter`: assign an incrementing per-org `seq` (0-based) so its listed entries form a gap-free chain.
- Every other `AuditEntry { … }` constructor (test tamper builders) sets `seq`.

### 3. `verify_chain` — seq-binding + gap-free (signature unchanged)
`verify_chain(entries, vk)` keeps its signature (so all ~10 callers are untouched) and additionally:
- recomputes each hash with the entry's `seq` (via the extended `PayloadFields`);
- enforces a **gap-free monotonic sequence**: the first entry's `seq` must be `0`, each subsequent `= prev.seq + 1`; otherwise `VerifyError::SeqGap { index, expected, got }` (new variant).

### 4. Tip anchor — new `verify_chain_with_anchor`
- New `pub struct TipAnchor { pub seq: i64, pub hash: String }` + `TipAnchor::from_entry(&AuditEntry)`.
- New `pub fn verify_chain_with_anchor(entries, vk, anchor: &TipAnchor) -> Result<(), VerifyError>`: runs `verify_chain`, then asserts the **last** entry's `(seq, hash) == (anchor.seq, anchor.hash)` **and** `entries.len() as i64 == anchor.seq + 1` (no missing tail). A short/empty chain vs the anchor → `VerifyError::TruncatedChain { expected_len, got_len }` (new variant). Empty `entries` with an anchor → `TruncatedChain`.
- This is what catches tail-truncation + whole-chain-deletion: the caller holds a previously-captured `TipAnchor` and checks the chain still reaches it.

### 5. Producing the anchor
`TipAnchor::from_entry(last_entry)` lets a caller capture the current tip after a write/list. (A `PostgresAuditStore::latest_tip(org_id)` convenience may be added if a call site needs it; not required for the core.)

## Explicitly deferred (matches the finding)
**External/WORM anchoring** — persisting the `TipAnchor` to a tamper-proof, append-only sink (object-lock / co-sign) so an attacker with DB write can't also roll back the anchor. That is the existing `post-scale-s3-object-lock` backlog item. Storing the anchor in the same DB adds no tamper-resistance, so this slice does **not** do that; it makes `verify_chain_with_anchor` *able* to check an anchor and provides `TipAnchor`.

## Testing (pure — no DB; build entries with a generated test key, or via `InMemoryAuditWriter`)
- `canonical_payload_bytes` includes `seq` (the bytes change when `seq` changes).
- Happy path: a written/gap-free chain passes `verify_chain` and `verify_chain_with_anchor` with the derived tip.
- Tampered `seq` on an entry → `HashMismatch` (hash now covers seq).
- Deleted middle entry → fails (`BrokenChain` or `SeqGap`).
- Gap in seq (e.g. 0,1,3) → `SeqGap`.
- Truncated tail + the original anchor → `TruncatedChain`.
- Empty chain + anchor → `TruncatedChain`; empty chain + no anchor (`verify_chain`) → `Ok` (documented).
- Round-trip via `InMemoryAuditWriter`: write 3, derive tip, `verify_chain_with_anchor` passes; drop the last entry, re-verify with the old tip → `TruncatedChain`.
- Existing `tests/audit.rs` tamper/broken/bad-signature tests updated for the new `seq` field; assertions that pin a specific `VerifyError` variant adjusted only where seq-binding changes which check fires first.

Gates: `cargo test -p tt-telemetry -p tt-auth -p tt-plan-core -p tt-cli` (all `verify_chain` consumers); `cargo clippy --all-targets -- -D warnings`; `cargo fmt --check`.

## Out of scope
- External/WORM anchor sink (deferred item).
- Versioned/back-compat hashing (clean break chosen).
- Audit signing-key management/rotation.
- Auto-emitting periodic checkpoints (no value without the external sink).
