//! Integration tests for the audit log hash chain.
//!
//! Tests exercise the full write + verify round-trip, chain linkage invariants,
//! tamper detection, and multi-org isolation.

use tt_telemetry::audit::{verify_chain, Actor, AuditWriter, InMemoryAuditWriter, VerifyError};
use uuid::Uuid;

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Returns a System actor (simplest variant for tests that don't care about
/// actor identity).
fn system() -> Actor {
    Actor::System
}

/// Minimal payload.
fn payload() -> serde_json::Value {
    serde_json::json!({ "note": "test" })
}

// ─── 1. Round-trip single entry ───────────────────────────────────────────────

#[tokio::test]
async fn test_single_entry_round_trip() {
    let writer = InMemoryAuditWriter::new();
    let org = Uuid::new_v4();

    let entry = writer
        .write(org, system(), "test.event".to_string(), payload())
        .await
        .expect("write should succeed");

    let listed = writer.list(org).await.expect("list should succeed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, entry.id);

    let vk = writer.verifying_key();
    verify_chain(&listed, &vk).expect("chain should verify");
}

// ─── 2. 3-entry chain verifies ────────────────────────────────────────────────

#[tokio::test]
async fn test_three_entry_chain_verifies() {
    let writer = InMemoryAuditWriter::new();
    let org = Uuid::new_v4();

    for i in 0..3u32 {
        writer
            .write(
                org,
                system(),
                format!("event.{i}"),
                serde_json::json!({ "i": i }),
            )
            .await
            .expect("write should succeed");
    }

    let entries = writer.list(org).await.expect("list should succeed");
    assert_eq!(entries.len(), 3);

    let vk = writer.verifying_key();
    verify_chain(&entries, &vk).expect("3-entry chain should verify");
}

// ─── 3. prev_hash linkage ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_prev_hash_linkage() {
    let writer = InMemoryAuditWriter::new();
    let org = Uuid::new_v4();

    for i in 0..4u32 {
        writer
            .write(
                org,
                system(),
                format!("e{i}"),
                serde_json::json!({ "i": i }),
            )
            .await
            .expect("write should succeed");
    }

    let entries = writer.list(org).await.expect("list should succeed");
    for i in 1..entries.len() {
        assert_eq!(
            entries[i].prev_hash,
            entries[i - 1].hash,
            "entry {i} prev_hash must equal entry {} hash",
            i - 1
        );
    }
}

// ─── 4. Genesis prev_hash is all zeros ────────────────────────────────────────

#[tokio::test]
async fn test_genesis_prev_hash_is_zeros() {
    let writer = InMemoryAuditWriter::new();
    let org = Uuid::new_v4();

    writer
        .write(org, system(), "genesis".to_string(), payload())
        .await
        .expect("write should succeed");

    let entries = writer.list(org).await.expect("list should succeed");
    let expected = "0".repeat(64);
    assert_eq!(
        entries[0].prev_hash, expected,
        "genesis entry must have all-zero prev_hash"
    );
}

// ─── 5. Tamper: payload mutation → HashMismatch ───────────────────────────────

#[tokio::test]
async fn test_tamper_payload_mutation_detected() {
    let writer = InMemoryAuditWriter::new();
    let org = Uuid::new_v4();

    for i in 0..3u32 {
        writer
            .write(
                org,
                system(),
                format!("e{i}"),
                serde_json::json!({ "i": i }),
            )
            .await
            .expect("write should succeed");
    }

    let mut entries = writer.list(org).await.expect("list should succeed");
    // Mutate entry[1]'s payload.
    entries[1].payload = serde_json::json!({ "tampered": true });

    let vk = writer.verifying_key();
    let err = verify_chain(&entries, &vk).expect_err("tampered chain should fail");
    assert!(
        matches!(err, VerifyError::HashMismatch { index: 1 }),
        "expected HashMismatch at index 1, got {err:?}"
    );
}

// ─── 6. Tamper: prev_hash mutation → BrokenChain ─────────────────────────────

#[tokio::test]
async fn test_tamper_prev_hash_mutation_detected() {
    let writer = InMemoryAuditWriter::new();
    let org = Uuid::new_v4();

    for i in 0..3u32 {
        writer
            .write(
                org,
                system(),
                format!("e{i}"),
                serde_json::json!({ "i": i }),
            )
            .await
            .expect("write should succeed");
    }

    let mut entries = writer.list(org).await.expect("list should succeed");
    // Mutate entry[2]'s prev_hash to something wrong.
    entries[2].prev_hash = "a".repeat(64);

    let vk = writer.verifying_key();
    let err = verify_chain(&entries, &vk).expect_err("broken chain should fail");
    assert!(
        matches!(err, VerifyError::BrokenChain { index: 2, .. }),
        "expected BrokenChain at index 2, got {err:?}"
    );
}

// ─── 7. Tamper: signature mutation → BadSignature ────────────────────────────

#[tokio::test]
async fn test_tamper_signature_mutation_detected() {
    let writer = InMemoryAuditWriter::new();
    let org = Uuid::new_v4();

    for i in 0..3u32 {
        writer
            .write(
                org,
                system(),
                format!("e{i}"),
                serde_json::json!({ "i": i }),
            )
            .await
            .expect("write should succeed");
    }

    let mut entries = writer.list(org).await.expect("list should succeed");
    // Corrupt entry[1]'s signature — flip all bytes.
    let bad_sig = "f".repeat(128); // 64 bytes of 0xff as hex
    entries[1].signature = bad_sig;

    let vk = writer.verifying_key();
    let err = verify_chain(&entries, &vk).expect_err("bad signature should fail");
    assert!(
        matches!(err, VerifyError::BadSignature(1, _)),
        "expected BadSignature at index 1, got {err:?}"
    );
}

// ─── 8. Multiple orgs are independent ────────────────────────────────────────

#[tokio::test]
async fn test_multiple_orgs_are_independent() {
    let writer = InMemoryAuditWriter::new();
    let org_a = Uuid::new_v4();
    let org_b = Uuid::new_v4();

    // Write 2 entries to org A and 1 to org B.
    writer
        .write(org_a, system(), "a.1".to_string(), payload())
        .await
        .unwrap();
    writer
        .write(org_a, system(), "a.2".to_string(), payload())
        .await
        .unwrap();
    writer
        .write(org_b, system(), "b.1".to_string(), payload())
        .await
        .unwrap();

    let a_entries = writer.list(org_a).await.unwrap();
    let b_entries = writer.list(org_b).await.unwrap();

    assert_eq!(a_entries.len(), 2, "org A should have 2 entries");
    assert_eq!(b_entries.len(), 1, "org B should have 1 entry");

    // B's genesis must also be all-zero prev_hash.
    let expected_genesis_prev = "0".repeat(64);
    assert_eq!(b_entries[0].prev_hash, expected_genesis_prev);

    // Verify each org's chain independently.
    let vk = writer.verifying_key();
    verify_chain(&a_entries, &vk).expect("org A chain should verify");
    verify_chain(&b_entries, &vk).expect("org B chain should verify");
}

// ─── 9. 100-entry chain perf smoke test ──────────────────────────────────────

#[tokio::test]
async fn test_100_entry_chain_perf() {
    let writer = InMemoryAuditWriter::new();
    let org = Uuid::new_v4();

    for i in 0..100u32 {
        writer
            .write(
                org,
                system(),
                format!("event.{i}"),
                serde_json::json!({ "seq": i, "data": "some payload content here" }),
            )
            .await
            .expect("write should succeed");
    }

    let entries = writer.list(org).await.expect("list should succeed");
    assert_eq!(entries.len(), 100);

    let vk = writer.verifying_key();
    let start = std::time::Instant::now();
    verify_chain(&entries, &vk).expect("100-entry chain should verify");
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 500,
        "verify_chain for 100 entries took {}ms, expected <500ms",
        elapsed.as_millis()
    );
}
