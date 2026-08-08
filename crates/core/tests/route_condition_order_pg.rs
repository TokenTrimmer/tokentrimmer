//! DB-gated order-execution gate for the canonical route store.
//!
//! Proves on REAL PostgreSQL that overlapping-condition routes are loaded in
//! the pinned `priority DESC, created_at ASC, id ASC` order and that the
//! gateway [`RoutingEngine`] evaluates them first-match-wins in exactly that
//! order: strictly-higher priority wins, equal-priority overlapping routes
//! resolve by earlier creation time (created_at ASC), and byte-identical
//! creation times resolve by ascending UUID (id ASC).
//!
//! The `routes` table is CLOUD-owned (tokentrimmer-cloud 0002) — public
//! migrations deliberately do not create it (and `route_pauses` has no FK to
//! it so 0019 applies standalone on OSS deploys). This test therefore creates
//! a minimal cloud-shaped `routes` table itself and leaves the cloud-owned
//! `route_versions` ledger absent, so the store's documented rolling-fallback
//! SQL path (NULL immutable-version provenance) is the one exercised.
//!
//! Run locally against the public DB-gated test database:
//! ```sh
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/tt_public_test
//! cargo test -p tt-core --test route_condition_order_pg -- --include-ignored
//! ```

use serde_json::{json, Value};
use tt_core::{connect, migrate_only};
use tt_routing::{PostgresRoutingStore, RouteFeatureSnapshot, RoutingEngine, RoutingStore};
use uuid::Uuid;

/// Minimal cloud-shaped `routes` table (mirrors tokentrimmer-cloud 0002).
const CREATE_ROUTES_TABLE: &str = "CREATE TABLE IF NOT EXISTS routes (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id      UUID NOT NULL,
  name        TEXT NOT NULL,
  priority    INT  NOT NULL,
  conditions  JSONB NOT NULL,
  target      JSONB NOT NULL,
  enabled     BOOLEAN NOT NULL DEFAULT TRUE,
  revision    BIGINT NOT NULL DEFAULT 1,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
)";

async fn store() -> (PostgresRoutingStore, sqlx::PgPool) {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    migrate_only(&url)
        .await
        .expect("migrations (incl. 0019 route_pauses) apply cleanly");
    let pool = connect(&url, 2).await.expect("connect");
    // The tests in this binary run concurrently; serialize the CREATE TABLE
    // bootstrap with a transaction-scoped advisory lock so `IF NOT EXISTS`
    // cannot race two sessions into a duplicate pg_type key (SQLSTATE 23505).
    let mut tx = pool.begin().await.expect("begin bootstrap tx");
    sqlx::query("SELECT pg_advisory_xact_lock(0x7474002f)")
        .execute(&mut *tx)
        .await
        .expect("acquire bootstrap advisory lock");
    sqlx::query(CREATE_ROUTES_TABLE)
        .execute(&mut *tx)
        .await
        .expect("create minimal cloud-shaped routes table");
    tx.commit().await.expect("commit bootstrap tx");
    (PostgresRoutingStore::new(pool.clone()), pool)
}

struct RouteSeed {
    id: Uuid,
    name: &'static str,
    priority: u32,
    created_at: &'static str,
    conditions: Value,
    target: Value,
}

impl RouteSeed {
    /// Build a route id deterministically from the org UUID plus a constant
    /// 4-byte `tag`. Each run uses a fresh org (v7), so the inserted ids are
    /// unique across re-runs on a shared database while staying reproducible
    /// inside one run — the persisted `id ASC` tie-break is driven purely by
    /// the tag (highest-order differing byte first).
    fn id_for(org: Uuid, tag: [u8; 4]) -> Uuid {
        let mut bytes = *org.as_bytes();
        bytes[12..16].copy_from_slice(&tag);
        Uuid::from_bytes(bytes)
    }

    fn new(
        org: Uuid,
        tag: [u8; 4],
        name: &'static str,
        priority: u32,
        created_at: &'static str,
        conditions: Value,
        target: Value,
    ) -> Self {
        Self {
            id: Self::id_for(org, tag),
            name,
            priority,
            created_at,
            conditions,
            target,
        }
    }
}

async fn insert_route(pool: &sqlx::PgPool, org_id: Uuid, seed: RouteSeed) {
    sqlx::query(
        "INSERT INTO routes (id, org_id, name, priority, conditions, target, enabled, revision, created_at) \
         VALUES ($1, $2, $3, $4, $5::jsonb, $6::jsonb, TRUE, 1, $7::timestamptz)",
    )
    .bind(seed.id)
    .bind(org_id)
    .bind(seed.name)
    .bind(seed.priority as i32)
    .bind(seed.conditions)
    .bind(seed.target)
    .bind(seed.created_at)
    .execute(pool)
    .await
    .unwrap_or_else(|e| panic!("insert route {}: {e}", seed.name));
}

async fn runtime_names_and_versions(
    store: &PostgresRoutingStore,
    org_id: Uuid,
) -> (Vec<String>, Vec<Option<i64>>) {
    let runtime = store
        .list_runtime_for_org(org_id)
        .await
        .expect("runtime list for org");
    (
        runtime.iter().map(|r| r.route.name.clone()).collect(),
        runtime.iter().map(|r| r.route_version_id).collect(),
    )
}

/// Evaluate a request via the pinned canonical snapshot builders.
fn snapshot(model: &str, tag: Option<&str>, prompt: &str) -> RouteFeatureSnapshot {
    RouteFeatureSnapshot::from_retained_features(model.to_owned(), 100, tag.map(str::to_owned))
        .with_input_text(prompt)
}

/// The gateway must select routes in `priority DESC, created_at ASC, id ASC`
/// first-match-wins order when overlapping conditions match one request:
///   * org_priority — strictly higher priority wins over a lower-priority
///     route with the same overlapping conditions;
///   * org_created — two equal-priority overlapping routes resolve by earlier
///     created_at even when the earlier route has the HIGHER UUID;
///   * org_uuidtie — two equal-priority, byte-identical-created_at,
///     overlapping routes resolve by ascending UUID.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_overlapping_conditions_execute_in_persisted_store_order() {
    let (store, pool) = store().await;
    let org_priority = Uuid::now_v7();
    let org_created = Uuid::now_v7();
    let org_uuidtie = Uuid::now_v7();

    // --- priority DESC -----------------------------------------------------
    insert_route(
        &pool,
        org_priority,
        RouteSeed::new(
            org_priority,
            [0, 0, 0, 1],
            "high",
            300,
            "2026-01-01T00:00:00Z",
            json!({"model_in": ["gpt-4o"], "tag_equals": "alpha"}),
            json!({"target_model": "high"}),
        ),
    )
    .await;
    insert_route(
        &pool,
        org_priority,
        RouteSeed::new(
            org_priority,
            [0, 0, 0, 2],
            "low",
            100,
            "2026-01-02T00:00:00Z",
            json!({"model_in": ["gpt-4o"], "tag_equals": "alpha"}),
            json!({"target_model": "low"}),
        ),
    )
    .await;

    let (names, versions) = runtime_names_and_versions(&store, org_priority).await;
    assert_eq!(
        names,
        ["high", "low"],
        "runtime load must order strictly higher priority first (priority DESC)"
    );
    assert!(
        versions.iter().all(Option::is_none),
        "with the cloud route_versions ledger absent, provenance must stay NULL through the rolling fallback"
    );

    let engine = RoutingEngine::with_runtime_routes(
        store
            .list_runtime_for_org(org_priority)
            .await
            .expect("reload priority org"),
    );
    let selected = engine
        .evaluate_snapshot(&snapshot("gpt-4o", Some("alpha"), "x"))
        .expect("a route must match");
    assert_eq!(
        selected.name, "high",
        "strictly higher priority must win the overlapping-condition tie"
    );

    // --- created_at ASC overrides UUID -------------------------------------
    insert_route(
        &pool,
        org_created,
        RouteSeed::new(
            org_created,
            [0, 0, 0, 9],
            "early",
            200,
            "2026-01-01T00:00:00Z",
            json!({"model_in": ["gpt-4o"]}),
            json!({"target_model": "early"}),
        ),
    )
    .await;
    insert_route(
        &pool,
        org_created,
        RouteSeed::new(
            org_created,
            [0, 0, 0, 8],
            "late",
            200,
            "2026-02-01T00:00:00Z",
            json!({"model_in": ["gpt-4o"], "tag_equals": "prod"}),
            json!({"target_model": "late"}),
        ),
    )
    .await;

    let (names, _) = runtime_names_and_versions(&store, org_created).await;
    assert_eq!(
        names,
        ["early", "late"],
        "equal priority must load by created_at ASC even though 'early' has the higher UUID"
    );
    let engine = RoutingEngine::with_runtime_routes(
        store
            .list_runtime_for_org(org_created)
            .await
            .expect("reload created org"),
    );
    let selected = engine
        .evaluate_snapshot(&snapshot("gpt-4o", Some("prod"), "x"))
        .expect("a route must match");
    assert_eq!(
        selected.name, "early",
        "equal-priority overlapping routes resolve by earlier created_at, not by UUID"
    );

    // --- id ASC tie-break --------------------------------------------------
    insert_route(
        &pool,
        org_uuidtie,
        RouteSeed::new(
            org_uuidtie,
            [0, 0, 0, 2],
            "tie1",
            200,
            "2026-05-05T00:00:00Z",
            json!({"model_in": ["gpt-4o"], "tag_equals": "prod"}),
            json!({"target_model": "tie1"}),
        ),
    )
    .await;
    insert_route(
        &pool,
        org_uuidtie,
        RouteSeed::new(
            org_uuidtie,
            [0, 0, 0, 1],
            "tie2",
            200,
            "2026-05-05T00:00:00Z",
            json!({"model_in": ["gpt-4o"], "tag_equals": "prod"}),
            json!({"target_model": "tie2"}),
        ),
    )
    .await;

    let (names, _) = runtime_names_and_versions(&store, org_uuidtie).await;
    assert_eq!(
        names,
        ["tie2", "tie1"],
        "equal priority and byte-identical created_at must order by ascending UUID (id ASC)"
    );
    let engine = RoutingEngine::with_runtime_routes(
        store
            .list_runtime_for_org(org_uuidtie)
            .await
            .expect("reload uuid-tie org"),
    );
    let selected = engine
        .evaluate_snapshot(&snapshot("gpt-4o", Some("prod"), "x"))
        .expect("a route must match");
    assert_eq!(
        selected.name, "tie2",
        "byte-identical created_at equal-priority routes resolve by ascending UUID"
    );

    // --- management read keeps the same order (incl. disabled rows) --------
    insert_route(
        &pool,
        org_uuidtie,
        RouteSeed::new(
            org_uuidtie,
            [0, 0, 0, 9],
            "disabled-catchall",
            500,
            "2026-01-01T00:00:00Z",
            json!({}),
            json!({"target_model": "x"}),
        ),
    )
    .await;
    sqlx::query(
        "UPDATE routes SET enabled = FALSE WHERE org_id = $1 AND name = 'disabled-catchall'",
    )
    .bind(org_uuidtie)
    .execute(&pool)
    .await
    .expect("disable catchall row");
    let management_names: Vec<String> = store
        .list_all_for_org(org_uuidtie)
        .await
        .expect("management list")
        .into_iter()
        .map(|route| route.name)
        .collect();
    assert_eq!(
        management_names,
        [
            "disabled-catchall".to_owned(),
            "tie2".to_owned(),
            "tie1".to_owned(),
        ],
        "management read must keep priority DESC, created_at ASC, id ASC and include disabled rows"
    );
}
