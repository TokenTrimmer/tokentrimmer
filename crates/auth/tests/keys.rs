//! Integration tests for `tt_auth::keys`.
//!
//! All tests use `InMemoryKeyStore` and `InMemoryAuditWriter` — no external
//! services required.

use tt_auth::{issue, revoke_key, verify, Environment, InMemoryKeyStore, KeyError, KeyStore};
use tt_telemetry::audit::{verify_chain, Actor, AuditWriter, InMemoryAuditWriter};
use uuid::Uuid;

fn org() -> Uuid {
    Uuid::now_v7()
}

fn actor() -> Actor {
    Actor::System
}

// ── 1. issue + verify round-trip (live env) ──────────────────────────────────

#[tokio::test]
async fn test_issue_verify_roundtrip_live() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let issued = issue(&store, &audit, org_id, "test key", Environment::Live, actor())
        .await
        .expect("issue should succeed");

    let ctx = verify(&store, &issued.plaintext)
        .await
        .expect("verify should succeed");

    assert_eq!(ctx.key_id, issued.record.id);
    assert_eq!(ctx.org_id, org_id);
}

// ── 2. issue + verify round-trip (test env) ──────────────────────────────────

#[tokio::test]
async fn test_issue_verify_roundtrip_test_env() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let issued = issue(&store, &audit, org_id, "sandbox key", Environment::Test, actor())
        .await
        .expect("issue should succeed");

    assert!(
        issued.plaintext.starts_with("tt_test_"),
        "test env key should start with tt_test_"
    );

    let ctx = verify(&store, &issued.plaintext)
        .await
        .expect("verify should succeed");

    assert_eq!(ctx.key_id, issued.record.id);
    assert_eq!(ctx.org_id, org_id);
}

// ── 3. issue emits apikey.issued audit row with prefix + label, NO plaintext ─

#[tokio::test]
async fn test_issue_emits_audit_row_without_plaintext() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let issued = issue(
        &store,
        &audit,
        org_id,
        "labelled key",
        Environment::Live,
        actor(),
    )
    .await
    .expect("issue should succeed");

    let entries = audit.list(org_id).await.expect("list should succeed");
    assert_eq!(entries.len(), 1, "should have exactly one audit entry");

    let entry = &entries[0];
    assert_eq!(entry.event, "apikey.issued");

    let payload = &entry.payload;
    assert_eq!(
        payload["label"].as_str().unwrap(),
        "labelled key",
        "payload should contain label"
    );
    assert_eq!(
        payload["prefix"].as_str().unwrap(),
        &issued.record.prefix,
        "payload should contain prefix"
    );

    // The plaintext MUST NOT appear anywhere in the payload.
    let payload_str = serde_json::to_string(payload).unwrap();
    assert!(
        !payload_str.contains(&issued.plaintext),
        "plaintext must not appear in audit payload"
    );
}

// ── 4. verify with wrong key returns NotFound ─────────────────────────────────

#[tokio::test]
async fn test_verify_wrong_key_returns_not_found() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let issued = issue(&store, &audit, org_id, "key", Environment::Live, actor())
        .await
        .expect("issue should succeed");

    // Tamper with the last character of the hex portion.
    let mut bad = issued.plaintext.clone();
    let last = bad.pop().unwrap();
    let replacement = if last == 'a' { 'b' } else { 'a' };
    bad.push(replacement);

    let result = verify(&store, &bad).await;
    assert!(
        matches!(result, Err(KeyError::NotFound)),
        "wrong key should return NotFound, got: {result:?}"
    );
}

// ── 5. verify with invalid format returns InvalidFormat ───────────────────────

#[tokio::test]
async fn test_verify_invalid_format_returns_invalid_format() {
    let store = InMemoryKeyStore::new();

    // Doesn't start with tt_live_ or tt_test_. Avoid using realistic provider
    // prefixes like "sk_live_*" / "sk-*" / "AIza*" / "AKIA*" here so GitHub
    // push-protection secret scanners don't false-positive on a test fixture.
    let result = verify(&store, "PLACEHOLDER_NOT_A_KEY").await;
    assert!(
        matches!(result, Err(KeyError::InvalidFormat)),
        "non-tt prefix should return InvalidFormat"
    );

    let result2 = verify(&store, "tt_prod_abcdefghijklmnopqrstuvwxyz12").await;
    assert!(
        matches!(result2, Err(KeyError::InvalidFormat)),
        "unknown tt_ prefix should return InvalidFormat"
    );
}

// ── 6. verify with too-short string returns InvalidFormat ─────────────────────

#[tokio::test]
async fn test_verify_too_short_returns_invalid_format() {
    let store = InMemoryKeyStore::new();

    // 11 chars — one shy of PREFIX_DISPLAY_LEN (12).
    let result = verify(&store, "tt_live_abc").await;
    assert!(
        matches!(result, Err(KeyError::InvalidFormat)),
        "too-short string should return InvalidFormat"
    );

    let result2 = verify(&store, "tt_live_").await;
    assert!(
        matches!(result2, Err(KeyError::InvalidFormat)),
        "prefix-only string should return InvalidFormat"
    );

    let result3 = verify(&store, "").await;
    assert!(
        matches!(result3, Err(KeyError::InvalidFormat)),
        "empty string should return InvalidFormat"
    );
}

// ── 7. revoke + verify returns Revoked ───────────────────────────────────────

#[tokio::test]
async fn test_revoke_then_verify_returns_revoked() {
    use chrono::Utc;

    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let issued = issue(&store, &audit, org_id, "key", Environment::Live, actor())
        .await
        .expect("issue should succeed");

    let found = store
        .revoke(issued.record.id, Utc::now())
        .await
        .expect("revoke should succeed");
    assert!(found, "revoke should return true for existing key");

    let result = verify(&store, &issued.plaintext).await;
    assert!(
        matches!(result, Err(KeyError::Revoked)),
        "revoked key should return Revoked, got: {result:?}"
    );
}

// ── 8. issued key prefix is exactly 12 chars and matches stored prefix ────────

#[tokio::test]
async fn test_prefix_length_and_matches_stored() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let issued = issue(&store, &audit, org_id, "key", Environment::Live, actor())
        .await
        .expect("issue should succeed");

    assert_eq!(
        issued.record.prefix.len(),
        12,
        "stored prefix must be exactly 12 chars"
    );
    assert_eq!(
        &issued.plaintext[..12],
        &issued.record.prefix,
        "prefix must be the first 12 chars of the plaintext"
    );
}

// ── 9. issued plaintext is tt_live_ + 32 hex chars = 40 chars ────────────────

#[tokio::test]
async fn test_plaintext_format_live() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let issued = issue(&store, &audit, org_id, "key", Environment::Live, actor())
        .await
        .expect("issue should succeed");

    assert_eq!(
        issued.plaintext.len(),
        40,
        "live key plaintext must be exactly 40 chars"
    );
    assert!(
        issued.plaintext.starts_with("tt_live_"),
        "live key must start with tt_live_"
    );
    // The hex portion (after "tt_live_") must be lowercase hex.
    let hex_part = &issued.plaintext[8..];
    assert_eq!(hex_part.len(), 32, "hex portion must be 32 chars");
    assert!(
        hex_part.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "hex portion must be lowercase hex digits"
    );
}

// ── 10. two keys issued for same org have distinct ids and distinct plaintexts ─

#[tokio::test]
async fn test_two_keys_distinct() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let key_a = issue(&store, &audit, org_id, "key A", Environment::Live, actor())
        .await
        .expect("issue A should succeed");
    let key_b = issue(&store, &audit, org_id, "key B", Environment::Live, actor())
        .await
        .expect("issue B should succeed");

    assert_ne!(key_a.record.id, key_b.record.id, "ids must be distinct");
    assert_ne!(
        key_a.plaintext, key_b.plaintext,
        "plaintexts must be distinct"
    );
    assert_ne!(
        key_a.record.prefix, key_b.record.prefix,
        "prefixes must be distinct (with overwhelming probability)"
    );

    // Both audit entries exist.
    let entries = audit.list(org_id).await.expect("list should succeed");
    assert_eq!(entries.len(), 2, "should have two audit entries");
}

// ── 11. revoke_key emits audit row + chain verifies end-to-end (w8-audit-row-gate) ──

#[tokio::test]
async fn test_revoke_emits_audit_row_and_chain_verifies() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    // Issue then revoke.
    let issued = issue(&store, &audit, org_id, "to-revoke", Environment::Live, actor())
        .await
        .expect("issue ok");
    revoke_key(&store, &audit, org_id, issued.record.id, actor())
        .await
        .expect("revoke ok");

    // Two audit entries in order: issued, then revoked.
    let entries = audit.list(org_id).await.expect("list ok");
    assert_eq!(entries.len(), 2, "issued + revoked = 2 entries");
    assert_eq!(entries[0].event, "apikey.issued");
    assert_eq!(entries[1].event, "apikey.revoked");

    // Revoked payload references the same key_id and never leaks plaintext.
    let revoked_payload = entries[1].payload.to_string();
    assert!(
        revoked_payload.contains(&issued.record.id.to_string()),
        "revoke audit payload should reference the key_id"
    );
    assert!(
        !revoked_payload.contains(&issued.plaintext),
        "revoke audit payload must NEVER include plaintext"
    );

    // Hash chain integrity holds across both events under the org's signing key.
    let vk = audit.verifying_key();
    verify_chain(&entries, &vk).expect("hash chain + signatures must verify end-to-end");
}

#[tokio::test]
async fn test_revoke_unknown_key_returns_not_found_no_audit_row() {
    let store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = org();

    let err = revoke_key(&store, &audit, org_id, Uuid::now_v7(), actor())
        .await
        .expect_err("revoking nonexistent key must fail");
    assert!(matches!(err, KeyError::NotFound));

    let entries = audit.list(org_id).await.expect("list ok");
    assert!(
        entries.is_empty(),
        "no audit row should be written for a failed revoke"
    );
}
