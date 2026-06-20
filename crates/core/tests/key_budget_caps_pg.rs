//! DB-gated test for the per-API-key spend/request cap read (P2) through the
//! REAL [`tt_core::tier_resolver::PostgresTierResolver::resolve_key_cap`] SQL.
//!
//! `api_key_budget_caps` is CLOUD-owned (tokentrimmer-cloud
//! `crates/api/migrations/0024_api_key_budget_caps.up.sql`) — public migrations
//! deliberately do not create it. This test therefore creates a minimal
//! cloud-shaped `api_key_budget_caps` table itself, mirroring cloud 0024
//! (api_key_id / org_id / monthly_cap_usd / monthly_request_cap). The resolver's
//! SELECT touches only this table, so no `api_keys` / `orgs` parent is needed for
//! the read path.
//!
//! Run locally against an empty Postgres:
//! ```sh
//! docker run --rm -d --name tt-pg-keycap -e POSTGRES_PASSWORD=postgres \
//!   -p 5557:5432 pgvector/pgvector:pg17
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5557/postgres
//! cargo test -p tt-core --test key_budget_caps_pg -- --include-ignored
//! docker rm -f tt-pg-keycap
//! ```

use tt_core::tier_resolver::{PostgresTierResolver, TierResolver};
use uuid::Uuid;

/// Minimal cloud-shaped `api_key_budget_caps` table (mirrors cloud 0024). The
/// FKs to `api_keys`/`orgs` are dropped here — the resolver SELECT does not need
/// the parents, and this keeps the standalone OSS test self-contained.
const CREATE_KEY_CAPS_TABLE: &str = "CREATE TABLE IF NOT EXISTS api_key_budget_caps (
  api_key_id          UUID PRIMARY KEY,
  org_id              UUID NOT NULL,
  monthly_cap_usd     DOUBLE PRECISION NULL,
  monthly_request_cap INTEGER NULL,
  updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
)";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("public migrations apply cleanly");
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    let mut tx = pool.begin().await.expect("begin bootstrap tx");
    sqlx::query("SELECT pg_advisory_xact_lock(0x74740024)")
        .execute(&mut *tx)
        .await
        .expect("acquire bootstrap advisory lock");
    sqlx::query(CREATE_KEY_CAPS_TABLE)
        .execute(&mut *tx)
        .await
        .expect("create minimal cloud-shaped api_key_budget_caps table");
    tx.commit().await.expect("commit bootstrap tx");
    pool
}

/// A seeded per-key cap is read back (scoped to (api_key_id, org_id)); an
/// unseeded key returns the empty cap; a cross-org key id is not visible.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_resolve_key_cap_reads_seeded_row_scoped_to_org() {
    let pool = pool().await;
    let org = Uuid::now_v7();
    let key = Uuid::now_v7();
    let other_org = Uuid::now_v7();

    // Seed a $25 / 2000-request cap for (key, org).
    sqlx::query(
        "INSERT INTO api_key_budget_caps (api_key_id, org_id, monthly_cap_usd, monthly_request_cap)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(key)
    .bind(org)
    .bind(25.0_f64)
    .bind(2000_i32)
    .execute(&pool)
    .await
    .expect("seed key cap");

    let resolver = PostgresTierResolver::new(pool.clone());

    // Read scoped to the owning org → the seeded cap.
    let cap = resolver.resolve_key_cap(org, key).await;
    assert_eq!(cap.monthly_cap_usd, Some(25.0));
    assert_eq!(cap.monthly_request_cap, Some(2000));

    // Same key id but a DIFFERENT org → not visible (scope guard).
    let cross = resolver.resolve_key_cap(other_org, key).await;
    assert!(
        cross.is_empty(),
        "a key cap must not be visible from another org"
    );

    // An unseeded key under the owning org → empty cap.
    let none = resolver.resolve_key_cap(org, Uuid::now_v7()).await;
    assert!(none.is_empty(), "unseeded key must have no cap");
}
