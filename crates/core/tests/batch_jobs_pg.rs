//! DB-gated round-trip test for the durable Batch Lane store
//! ([`tt_core::PostgresBatchStore`] → `batch_jobs`, migration 0024).
//!
//! Run locally against an empty Postgres:
//! ```sh
//! docker run --rm -d --name tt-pg-batch -e POSTGRES_PASSWORD=postgres \
//!   -p 5546:5432 pgvector/pgvector:pg17
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5546/postgres
//! cargo test -p tt-core --test batch_jobs_pg -- --include-ignored
//! docker rm -f tt-pg-batch
//! ```

use tt_core::{BatchJob, BatchStore, PostgresBatchStore};
use tt_provider_openai::Batch;
use uuid::Uuid;

fn sample_batch(id: &str, status: &str) -> Batch {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "object": "batch",
        "endpoint": "/v1/chat/completions",
        "input_file_id": "file-in",
        "completion_window": "24h",
        "status": status,
        "request_counts": { "total": 7, "completed": 0, "failed": 0 }
    }))
    .unwrap()
}

/// Insert one batch row, read it back org-scoped, then prove (a) cross-org reads
/// miss, (b) an org-scoped update mutates status + file ids + counts, and (c) a
/// cross-org update is a no-op against the migrated `batch_jobs` schema.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_batch_store_round_trips_and_is_org_scoped() {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("migrations (incl. 0024) apply cleanly");
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    let store = PostgresBatchStore::new(pool);

    let org_a = Uuid::now_v7();
    let org_b = Uuid::now_v7();
    let api_key_id = Uuid::now_v7();
    // Unique id per run so reruns against the same DB don't PK-collide.
    let batch_id = format!("batch_{}", Uuid::now_v7().simple());

    let job = BatchJob::from_batch(
        org_a,
        Some(api_key_id),
        "openai",
        &sample_batch(&batch_id, "validating"),
    );
    store.insert(job.clone()).await.expect("insert");

    // org_a reads it; every persisted field round-trips.
    let got = store
        .get(org_a, &batch_id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(got.id, batch_id);
    assert_eq!(got.org_id, org_a);
    assert_eq!(got.api_key_id, Some(api_key_id));
    assert_eq!(got.provider, "openai");
    assert_eq!(got.provider_batch_id.as_deref(), Some(batch_id.as_str()));
    assert_eq!(got.input_file_id.as_deref(), Some("file-in"));
    assert_eq!(got.status, "validating");
    assert_eq!(got.endpoint.as_deref(), Some("/v1/chat/completions"));
    assert_eq!(got.completion_window.as_deref(), Some("24h"));
    assert_eq!(got.request_total, 7);

    // org_b does NOT see org_a's batch (org-scoping).
    assert!(store.get(org_b, &batch_id).await.expect("get").is_none());

    // org-scoped update: adopt a completed provider view.
    let mut updated = got.clone();
    updated.apply_provider_update(&{
        let mut b = sample_batch(&batch_id, "completed");
        b.output_file_id = Some("file-out".into());
        b.error_file_id = Some("file-err".into());
        b.request_counts.completed = 6;
        b.request_counts.failed = 1;
        b
    });
    store.update(updated).await.expect("update");
    let after = store
        .get(org_a, &batch_id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(after.status, "completed");
    assert_eq!(after.output_file_id.as_deref(), Some("file-out"));
    assert_eq!(after.error_file_id.as_deref(), Some("file-err"));
    assert_eq!(after.request_completed, 6);
    assert_eq!(after.request_failed, 1);
    assert!(store
        .owns_file(org_a, "file-in")
        .await
        .expect("input ownership"));
    assert!(store
        .owns_file(org_a, "file-out")
        .await
        .expect("output ownership"));
    assert!(store
        .owns_file(org_a, "file-err")
        .await
        .expect("error ownership"));
    assert!(!store
        .owns_file(org_b, "file-out")
        .await
        .expect("cross-org ownership"));
    assert!(!store
        .owns_file(org_a, "file-unknown")
        .await
        .expect("unknown ownership"));

    // A cross-org update (org_b claiming org_a's id) must NOT mutate org_a's row.
    let mut hijack = after.clone();
    hijack.org_id = org_b;
    hijack.status = "cancelled".into();
    store.update(hijack).await.expect("update");
    let still = store
        .get(org_a, &batch_id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(
        still.status, "completed",
        "an org_b update must not mutate org_a's row"
    );
}
