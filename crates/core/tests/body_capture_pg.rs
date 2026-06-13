//! DB-gated round-trip test for the encrypted body-capture writer
//! ([`tt_telemetry::body_capture::postgres::PostgresBodyCaptureWriter`]),
//! specifically the per-body size cap + truncation marker (#451 gap-fill):
//! an over-cap body is stored truncated-with-marker, the ORIGINAL length is
//! recorded honestly in `request_bytes` / `response_bytes`, and a decrypt of
//! the stored ciphertext yields the truncated prefix plus the explicit marker.
//!
//! Run locally against an empty Postgres:
//! ```sh
//! docker run --rm -d --name tt-pg-capture -e POSTGRES_PASSWORD=postgres \
//!   -p 5556:5432 pgvector/pgvector:pg17
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5556/postgres
//! cargo test -p tt-core --test body_capture_pg -- --include-ignored
//! docker rm -f tt-pg-capture
//! ```

use chrono::Utc;
use tt_telemetry::body_capture::{
    postgres::PostgresBodyCaptureWriter, BodyCaptureCodec, BodyCaptureKind, BodyCaptureRecord,
    BodyCaptureWriter, TRUNCATION_MARKER_PREFIX,
};
use uuid::Uuid;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("migrations (incl. 0022 body capture) apply cleanly");
    tt_core::connect(&url, 2).await.expect("connect")
}

/// Opt an org into capture with the given retention.
async fn enable_capture(pool: &sqlx::PgPool, org: Uuid, retention_days: i32) {
    sqlx::query(
        "INSERT INTO request_body_capture_settings (org_id, enabled, retention_days) \
         VALUES ($1, TRUE, $2) \
         ON CONFLICT (org_id) DO UPDATE SET enabled = TRUE, retention_days = $2",
    )
    .bind(org)
    .bind(retention_days)
    .execute(pool)
    .await
    .expect("enable capture setting");
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn over_cap_body_is_stored_truncated_with_marker_and_original_len_recorded() {
    let pool = pool().await;
    let org = Uuid::now_v7();
    let trace_id = format!("trace-{}", Uuid::now_v7());
    enable_capture(&pool, org, 7).await;

    // Small cap so a modest body overflows; the codec carries the cap.
    let cap = 64usize;
    let master_key = [11u8; 32];
    let codec = BodyCaptureCodec::new(master_key).with_max_bytes(cap);
    let writer = PostgresBodyCaptureWriter::new(pool.clone(), codec.clone());

    let request_json = vec![b'q'; 500];
    let response_json = vec![b's'; 300];

    writer
        .record(BodyCaptureRecord {
            org_id: org,
            api_key_id: Uuid::now_v7(),
            trace_id: trace_id.clone(),
            endpoint: "/v1/chat/completions".into(),
            provider: "test".into(),
            model: "gpt-4o-mini".into(),
            request_json: request_json.clone(),
            response_json: Some(response_json.clone()),
            ts: Utc::now(),
        })
        .await
        .expect("record persists");

    // Original lengths recorded honestly, not the truncated stored size.
    let (req_bytes, resp_bytes): (i32, Option<i32>) = sqlx::query_as(
        "SELECT request_bytes, response_bytes FROM request_body_captures \
         WHERE org_id = $1 AND trace_id = $2",
    )
    .bind(org)
    .bind(&trace_id)
    .fetch_one(&pool)
    .await
    .expect("row present");
    assert_eq!(req_bytes, 500, "request_bytes is the ORIGINAL length");
    assert_eq!(
        resp_bytes,
        Some(300),
        "response_bytes is the ORIGINAL length"
    );

    // Decrypt the stored ciphertext: truncated prefix + explicit marker.
    let (req_enc, resp_enc): (Vec<u8>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT request_enc, response_enc FROM request_body_captures \
         WHERE org_id = $1 AND trace_id = $2",
    )
    .bind(org)
    .bind(&trace_id)
    .fetch_one(&pool)
    .await
    .expect("ciphertext present");

    let req_plain = codec
        .decrypt(org, &trace_id, BodyCaptureKind::Request, &req_enc)
        .expect("request decrypts");
    assert_eq!(
        &req_plain[..cap],
        &request_json[..cap],
        "request prefix kept"
    );
    assert!(
        req_plain[cap..].starts_with(TRUNCATION_MARKER_PREFIX),
        "request truncation marker appended"
    );
    assert!(
        req_plain[cap..].ends_with(b"500]"),
        "request marker discloses original length"
    );
    assert!(req_plain.len() < request_json.len(), "request truncated");

    let resp_plain = codec
        .decrypt(
            org,
            &trace_id,
            BodyCaptureKind::Response,
            resp_enc.as_ref().expect("response_enc present"),
        )
        .expect("response decrypts");
    assert_eq!(
        &resp_plain[..cap],
        &response_json[..cap],
        "response prefix kept"
    );
    assert!(
        resp_plain[cap..].starts_with(TRUNCATION_MARKER_PREFIX),
        "response truncation marker appended"
    );
    assert!(
        resp_plain[cap..].ends_with(b"300]"),
        "response marker discloses original length"
    );
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn under_cap_body_round_trips_verbatim_no_marker() {
    let pool = pool().await;
    let org = Uuid::now_v7();
    let trace_id = format!("trace-{}", Uuid::now_v7());
    enable_capture(&pool, org, 7).await;

    let codec = BodyCaptureCodec::new([22u8; 32]).with_max_bytes(64 * 1024);
    let writer = PostgresBodyCaptureWriter::new(pool.clone(), codec.clone());

    let request_json = br#"{"model":"gpt-4o-mini","messages":[]}"#.to_vec();

    writer
        .record(BodyCaptureRecord {
            org_id: org,
            api_key_id: Uuid::now_v7(),
            trace_id: trace_id.clone(),
            endpoint: "/v1/chat/completions".into(),
            provider: "test".into(),
            model: "gpt-4o-mini".into(),
            request_json: request_json.clone(),
            response_json: None,
            ts: Utc::now(),
        })
        .await
        .expect("record persists");

    let req_bytes: (i32,) = sqlx::query_as(
        "SELECT request_bytes FROM request_body_captures WHERE org_id = $1 AND trace_id = $2",
    )
    .bind(org)
    .bind(&trace_id)
    .fetch_one(&pool)
    .await
    .expect("row present");
    assert_eq!(req_bytes.0 as usize, request_json.len());

    let req_enc: (Vec<u8>,) = sqlx::query_as(
        "SELECT request_enc FROM request_body_captures WHERE org_id = $1 AND trace_id = $2",
    )
    .bind(org)
    .bind(&trace_id)
    .fetch_one(&pool)
    .await
    .expect("ciphertext present");

    let req_plain = codec
        .decrypt(org, &trace_id, BodyCaptureKind::Request, &req_enc.0)
        .expect("request decrypts");
    assert_eq!(req_plain, request_json, "under-cap body stored verbatim");
    assert!(
        !req_plain
            .windows(TRUNCATION_MARKER_PREFIX.len())
            .any(|w| w == TRUNCATION_MARKER_PREFIX),
        "no truncation marker for under-cap body"
    );
}
