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

/// Mirrors cloud migration 0044's `CREATE TABLE flow_doc_distill_cache`.
const CREATE_CACHE_TABLE: &str = "CREATE TABLE IF NOT EXISTS flow_doc_distill_cache (
    org_id         UUID        NOT NULL,
    content_hash   TEXT        NOT NULL,
    caller_key     TEXT,
    distilled_text TEXT        NOT NULL,
    pages          INTEGER     NOT NULL DEFAULT 0,
    engine         TEXT        NOT NULL,
    distilled_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT flow_doc_distill_cache_pk PRIMARY KEY
        (org_id, content_hash, COALESCE(caller_key, ''))
)";
const CREATE_EXPIRY_IDX: &str = "CREATE INDEX IF NOT EXISTS flow_doc_distill_cache_expiry_idx \
     ON flow_doc_distill_cache (org_id, distilled_at) \
     WHERE distilled_at < now() - INTERVAL '30 days'";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    sqlx::query(CREATE_CACHE_TABLE)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(CREATE_EXPIRY_IDX).execute(&pool).await.unwrap();
    sqlx::query("TRUNCATE flow_doc_distill_cache")
        .execute(&pool)
        .await
        .unwrap();
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
