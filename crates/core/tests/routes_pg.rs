//! Clean-PostgreSQL proof for the canonical route write/read/runtime boundary.
//!
//! The `routes` table is Cloud-owned, so Public migrations deliberately do not
//! create it. This test creates the minimal Cloud-shaped table, writes only a
//! route accepted by the canonical Public parser, verifies the exact canonical
//! JSON reached PostgreSQL, re-reads the same active hash through the
//! management view, and selects it through the production PostgreSQL runtime
//! store plus the real routing matcher.
//!
//! Run locally against an empty PostgreSQL with pgvector:
//! ```sh
//! docker run --rm -d --name tt-pg-canonical-routes \
//!   -e POSTGRES_PASSWORD=postgres -p 5554:5432 pgvector/pgvector:pg17
//! TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5554/postgres \
//!   cargo test -p tt-core --test routes_pg -- --include-ignored
//! docker rm -f tt-pg-canonical-routes
//! ```

use serde_json::{json, Value};
use tt_routing::{
    canonicalize_route_value, PostgresRoutingStore, RoutingEngine, RoutingStore,
    ROUTE_SCHEMA_VERSION,
};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    ChatCompletionRequest, RequestContext,
};
use uuid::Uuid;

/// Minimal Cloud-shaped `routes` table (mirrors Cloud migration 0002).
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
    tt_core::migrate_only(&url)
        .await
        .expect("Public migrations apply cleanly");
    let pool = tt_core::connect(&url, 2).await.expect("connect");

    // Other DB-gated route tests can share this database in CI. Serialize the
    // one-time Cloud-table bootstrap so concurrent CREATE IF NOT EXISTS calls
    // cannot race on PostgreSQL's implicit composite type.
    let mut tx = pool.begin().await.expect("begin bootstrap transaction");
    sqlx::query("SELECT pg_advisory_xact_lock(0x74740017)")
        .execute(&mut *tx)
        .await
        .expect("acquire route-table bootstrap lock");
    sqlx::query(CREATE_ROUTES_TABLE)
        .execute(&mut *tx)
        .await
        .expect("create minimal Cloud-shaped routes table");
    tx.commit().await.expect("commit bootstrap transaction");

    (PostgresRoutingStore::new(pool.clone()), pool)
}

fn request_context(org_id: Uuid) -> RequestContext {
    RequestContext {
        trace_id: Uuid::now_v7(),
        org_id,
        api_key_id: Uuid::now_v7(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("test"),
            base_url: None,
            extra_headers: Vec::new(),
        },
        tag: None,
        deadline: None,
        run_id: None,
        node_id: None,
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty PostgreSQL with pgvector) — run with --include-ignored"]
async fn pg_canonical_route_round_trip_is_active_and_runtime_selectable() {
    let (store, pool) = store().await;
    let org_id = Uuid::now_v7();

    // Defaults deliberately remain implicit at the input boundary. The parser
    // owns their canonical expansion and the stable identity of that exact
    // definition.
    let canonical = canonicalize_route_value(json!({
        "schema_version": ROUTE_SCHEMA_VERSION,
        "name": "canonical-pg-round-trip",
        "priority": 37,
        "when": {
            "model_in": ["gpt-4o"]
        },
        "then": {
            "target_model": "gpt-4o-mini"
        }
    }))
    .expect("route is canonical");
    let expected_hash = canonical.canonical_hash.clone();
    let expected_conditions = canonical.conditions.clone();
    let expected_target = canonical.target.clone();

    let created = store
        .create_route(org_id, canonical.route)
        .await
        .expect("persist canonical route");
    assert_eq!(created.name, "canonical-pg-round-trip");
    assert_eq!(created.priority, 37);
    assert!(created.enabled, "the canonical default is enabled");

    let (stored_name, stored_priority, stored_enabled, stored_conditions, stored_target): (
        String,
        i32,
        bool,
        Value,
        Value,
    ) = sqlx::query_as(
        "SELECT name, priority, enabled, conditions, target \
           FROM routes WHERE org_id = $1 AND id = $2",
    )
    .bind(org_id)
    .bind(created.id)
    .fetch_one(&pool)
    .await
    .expect("read exact persisted columns");
    assert_eq!(stored_name, "canonical-pg-round-trip");
    assert_eq!(stored_priority, 37);
    assert!(stored_enabled);
    assert_eq!(stored_conditions, expected_conditions);
    assert_eq!(stored_target, expected_target);

    let management = store
        .get_management_route(org_id, created.id)
        .await
        .expect("management read")
        .expect("stored route exists");
    assert_eq!(management.schema_version, ROUTE_SCHEMA_VERSION);
    assert_eq!(
        management.canonical_hash.as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(management.activation.state, "active");
    assert!(management.activation.issues.is_empty());

    // Loading through the production runtime store re-validates the raw
    // columns. Building the real matcher from that result proves the stored
    // definition is executable configuration rather than management-only
    // display data.
    let runtime = store
        .list_runtime_for_org(org_id)
        .await
        .expect("runtime route read");
    assert_eq!(runtime.len(), 1);
    assert_eq!(runtime[0].route.id, created.id);
    let engine = RoutingEngine::with_runtime_routes(runtime);
    let request: ChatCompletionRequest = serde_json::from_value(json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hello"}]
    }))
    .expect("valid chat request");
    let matched = engine
        .evaluate(&request, &request_context(org_id), 5)
        .expect("persisted route matches");
    assert_eq!(matched.id, created.id);
    assert_eq!(matched.then.target_model.as_deref(), Some("gpt-4o-mini"));

    // This org/id belongs solely to this test; leave a shared CI database
    // without retained route rows.
    sqlx::query("DELETE FROM routes WHERE org_id = $1 AND id = $2")
        .bind(org_id)
        .bind(created.id)
        .execute(&pool)
        .await
        .expect("remove test-owned route");
}
