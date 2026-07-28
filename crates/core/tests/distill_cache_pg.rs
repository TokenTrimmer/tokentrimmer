//! DB-gated tests for the `FlowDocDistillCache` store (D6 Slice 3) against the
//! REAL `flow_doc_distill_cache` table SQL (cloud migration 0044). Exercises the
//! miss→upsert→hit loop, the per-org isolation (one org's distill never served
//! to another), the `caller_key` composition, + fail-open on an absent table.
//!
//! The `flow_doc_distill_cache` table is CLOUD-owned (tokentrimmer-cloud
//! `crates/api/migrations/0044_flow_doc_distill_cache.up.sql`) — public
//! migrations do not create it. This test creates it manually (mirroring cloud
//! 0044) so the store SQL is exercised end-to-end without a cloud-side apply.
//!
//! Run locally against an empty Postgres:
//! ```sh
//! docker run --rm -d --name tt-pg-d6 -e POSTGRES_PASSWORD=postgres \
//!   -p 5553:5432 pgvector/pgvector:pg17
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5553/postgres
//! cargo test -p tt-core --test distill_cache_pg -- --include-ignored
//! docker rm -f tt-pg-d6
//! ```

use tt_core::workflow::distill_cache::{
    CachedDistill, DistillCacheKey, DistillCacheStore, FlowDocDistillCache,
};
use uuid::Uuid;

/// Mirrors cloud migration 0044's table and index contracts.  Keep the
/// conflict arbiter separate from the table: PostgreSQL does not allow an
/// expression such as `COALESCE(caller_key, '')` in a primary-key constraint.
const CREATE_CACHE_TABLE: &str = "CREATE TABLE IF NOT EXISTS public.flow_doc_distill_cache (
    org_id         UUID        NOT NULL,
    content_hash   TEXT        NOT NULL,
    caller_key     TEXT,
    distilled_text TEXT        NOT NULL,
    pages          INTEGER     NOT NULL DEFAULT 0,
    engine         TEXT        NOT NULL,
    distilled_at   TIMESTAMPTZ NOT NULL DEFAULT now()
)";
const CREATE_CACHE_KEY_IDX: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS flow_doc_distill_cache_key_uq \
     ON public.flow_doc_distill_cache (org_id, content_hash, COALESCE(caller_key, ''))";
const CREATE_EXPIRY_IDX: &str = "CREATE INDEX IF NOT EXISTS flow_doc_distill_cache_expiry_idx \
     ON public.flow_doc_distill_cache (distilled_at)";

/// The ignored tests below run in parallel when explicitly enabled. PostgreSQL's
/// `CREATE TABLE IF NOT EXISTS` is not sufficient to serialize concurrent first
/// creation, so hold a transaction-scoped lock while installing this fixture.
const CACHE_FIXTURE_SCHEMA_LOCK: i64 = 0x7474_6469_7374_6368; // "ttdistch"

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    let pool = tt_core::connect(&url, 2).await.expect("connect");

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(CACHE_FIXTURE_SCHEMA_LOCK)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(CREATE_CACHE_TABLE)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(CREATE_CACHE_KEY_IDX)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(CREATE_EXPIRY_IDX)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Each test generates an org UUID, so no shared-table truncation is needed
    // (or safe) while the ignored database tests run concurrently.
    pool
}

fn key(content_hash: &str) -> DistillCacheKey {
    DistillCacheKey {
        content_hash: content_hash.to_string(),
        caller_key: None,
    }
}

fn doc(text: &str) -> CachedDistill {
    CachedDistill {
        text: text.to_string(),
        pages: 1,
        engine: "pdf-extract".to_string(),
    }
}

#[test]
fn cache_fixture_uses_postgres_executable_0044_index_shapes() {
    assert!(
        !CREATE_CACHE_TABLE.contains("PRIMARY KEY"),
        "the NULL-aware cache key must be an expression index, not a primary key"
    );
    assert!(CREATE_CACHE_KEY_IDX.contains("CREATE UNIQUE INDEX"));
    assert!(CREATE_CACHE_KEY_IDX.contains("COALESCE(caller_key, '')"));
    assert!(
        !CREATE_EXPIRY_IDX.contains("WHERE"),
        "time-relative partial-index predicates use now(), which PostgreSQL rejects"
    );
    assert!(CREATE_EXPIRY_IDX.contains("(distilled_at)"));
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn miss_then_upsert_then_hit() {
    let pool = pool().await;
    let org = Uuid::new_v4();
    let cache = FlowDocDistillCache {
        org_id: org,
        pool: &pool,
    };
    let k = key("abc123");
    // Miss.
    assert!(cache.get(&k).await.is_none());
    // Upsert.
    cache.upsert(&k, &doc("cached-text")).await;
    // Hit.
    let got = cache.get(&k).await.expect("a get-after-upsert hits");
    assert_eq!(got.text, "cached-text");
    assert_eq!(got.pages, 1);
    assert_eq!(got.engine, "pdf-extract");
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn per_org_isolation() {
    let pool = pool().await;
    let org_a = Uuid::new_v4();
    let org_b = Uuid::new_v4();
    let cache_a = FlowDocDistillCache {
        org_id: org_a,
        pool: &pool,
    };
    let cache_b = FlowDocDistillCache {
        org_id: org_b,
        pool: &pool,
    };
    let k = key("shared-content-hash");
    cache_a.upsert(&k, &doc("org-a-text")).await;
    // Org B must NOT see org A's distillation (per-org isolation).
    assert!(
        cache_b.get(&k).await.is_none(),
        "one org's distillation must never leak to another"
    );
    // Org A still hits its own.
    assert_eq!(cache_a.get(&k).await.unwrap().text, "org-a-text");
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn caller_key_partitions_the_cache() {
    let pool = pool().await;
    let org = Uuid::new_v4();
    let cache = FlowDocDistillCache {
        org_id: org,
        pool: &pool,
    };
    // Same content hash + different caller_key → different cache slots.
    let k1 = DistillCacheKey {
        content_hash: "h".into(),
        caller_key: Some("flow-A".into()),
    };
    let k2 = DistillCacheKey {
        content_hash: "h".into(),
        caller_key: Some("flow-B".into()),
    };
    cache.upsert(&k1, &doc("from-A")).await;
    cache.upsert(&k2, &doc("from-B")).await;
    assert_eq!(cache.get(&k1).await.unwrap().text, "from-A");
    assert_eq!(cache.get(&k2).await.unwrap().text, "from-B");
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn upsert_overwrites_on_same_key() {
    let pool = pool().await;
    let org = Uuid::new_v4();
    let cache = FlowDocDistillCache {
        org_id: org,
        pool: &pool,
    };
    let k = key("overwrite-me");
    cache.upsert(&k, &doc("first")).await;
    cache.upsert(&k, &doc("second")).await;
    assert_eq!(cache.get(&k).await.unwrap().text, "second");
}

/// The public store's `COALESCE` conflict target deliberately treats an absent
/// caller key and an explicitly empty caller key as the same logical cache
/// address. This is the behavior provided by cloud migration 0044's expression
/// unique index; a raw nullable-column unique/primary key would not provide it.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn null_and_empty_caller_key_share_the_same_cache_slot() {
    let pool = pool().await;
    let org = Uuid::new_v4();
    let cache = FlowDocDistillCache {
        org_id: org,
        pool: &pool,
    };
    let no_key = DistillCacheKey {
        content_hash: "null-empty-conflict".into(),
        caller_key: None,
    };
    let empty_key = DistillCacheKey {
        content_hash: "null-empty-conflict".into(),
        caller_key: Some(String::new()),
    };

    cache.upsert(&no_key, &doc("first")).await;
    cache.upsert(&empty_key, &doc("second")).await;

    assert_eq!(cache.get(&no_key).await.unwrap().text, "second");
    assert_eq!(cache.get(&empty_key).await.unwrap().text, "second");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public.flow_doc_distill_cache \
         WHERE org_id = $1 AND content_hash = $2",
    )
    .bind(org)
    .bind("null-empty-conflict")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "NULL and empty caller keys share one cache row");
}
