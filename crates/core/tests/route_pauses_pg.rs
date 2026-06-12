//! DB-gated tests for the sticky `route_pauses` table (migration 0017) driven
//! through the REAL [`tt_routing::PostgresRoutingStore`] SQL: the
//! ownership-guarded `INSERT..SELECT` pause, the conditional `ON CONFLICT`
//! stickiness (an ACTIVE pause keeps its evidence; a resumed watermark row is
//! overwritten by a re-pause), the active-only `LEFT JOIN` that surfaces
//! `Route.paused` on every accessor, the watermark-stamping `UPDATE` resume
//! (the row is RETAINED with `resumed_at` set), and the pause-record GC on
//! route delete.
//!
//! The `routes` table is CLOUD-owned (tokentrimmer-cloud
//! `crates/api/migrations/0002_routes.up.sql`) — public migrations
//! deliberately do not create it (and `route_pauses` has no FK to it so 0017
//! applies standalone on OSS deploys). This test therefore creates a minimal
//! cloud-shaped `routes` table itself, mirroring cloud 0002
//! (id/org_id/name/priority/conditions/target/enabled/created_at).
//!
//! Run locally against an empty Postgres:
//! ```sh
//! docker run --rm -d --name tt-pg-honesty -e POSTGRES_PASSWORD=postgres \
//!   -p 5553:5432 pgvector/pgvector:pg17
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5553/postgres
//! cargo test -p tt-core --test route_pauses_pg -- --include-ignored
//! docker rm -f tt-pg-honesty
//! ```

use tt_routing::{
    NewRoute, NewRoutePause, PausedBy, PostgresRoutingStore, RouteAction, RouteConditions,
    RoutingStore,
};
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
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
)";

async fn store() -> (PostgresRoutingStore, sqlx::PgPool) {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("migrations (incl. 0017) apply cleanly");
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    // `CREATE TABLE IF NOT EXISTS` is NOT race-safe in Postgres: on a fresh
    // database, two concurrent sessions can both pass the existence check and
    // one then fails with a duplicate `pg_type` key (SQLSTATE 23505). The
    // tests in this binary run concurrently and each calls this helper, so
    // serialize the bootstrap with a transaction-scoped advisory lock (the
    // sqlx migrator above is already advisory-locked internally).
    let mut tx = pool.begin().await.expect("begin bootstrap tx");
    sqlx::query("SELECT pg_advisory_xact_lock(0x74740017)")
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

fn new_route(name: &str) -> NewRoute {
    NewRoute {
        name: name.into(),
        priority: 10,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: "gpt-4o-mini".into(),
            ..Default::default()
        },
    }
}

async fn pause_row_count(pool: &sqlx::PgPool, route_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM route_pauses WHERE route_id = $1")
        .bind(route_id)
        .fetch_one(pool)
        .await
        .expect("count route_pauses")
}

/// Ownership + stickiness + LEFT-JOIN surfacing + resume, end to end against
/// the real SQL.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_pause_resume_round_trip_and_ownership() {
    let (s, pool) = store().await;
    let org_a = Uuid::now_v7();
    let org_b = Uuid::now_v7();
    let created = s.create_route(org_a, new_route("down-a")).await.unwrap();
    assert!(!created.paused, "fresh route is unpaused");

    // Foreign-org pause: INSERT..SELECT matches no row → Ok(false), NO row.
    let pause = NewRoutePause {
        paused_by: PausedBy::Manual,
        reason: "foreign".into(),
        pass_rate: None,
        verdicts_in_window: None,
    };
    assert!(
        !s.pause_route(org_b, created.id, pause).await.unwrap(),
        "foreign org must not pause"
    );
    assert_eq!(pause_row_count(&pool, created.id).await, 0);
    assert!(
        !s.get_route(org_a, created.id)
            .await
            .unwrap()
            .unwrap()
            .paused
    );

    // Owner pause: Ok(true), evidence persisted.
    let auto_pause = NewRoutePause {
        paused_by: PausedBy::Auto,
        reason: "auto: paired pass-rate 75.0% < floor 90.0% (n=20)".into(),
        pass_rate: Some(0.75),
        verdicts_in_window: Some(20),
    };
    assert!(s.pause_route(org_a, created.id, auto_pause).await.unwrap());
    let (by, reason, rate, n): (String, String, Option<f64>, Option<i32>) = sqlx::query_as(
        "SELECT paused_by, reason, pass_rate, verdicts_in_window \
         FROM route_pauses WHERE route_id = $1",
    )
    .bind(created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(by, "auto");
    assert!(reason.starts_with("auto:"), "reason: {reason}");
    assert!((rate.unwrap() - 0.75).abs() < 1e-12);
    assert_eq!(n, Some(20));

    // Every accessor surfaces paused via the LEFT JOIN.
    assert!(
        s.get_route(org_a, created.id)
            .await
            .unwrap()
            .unwrap()
            .paused
    );
    assert!(s.list_for_org(org_a).await.unwrap()[0].paused);
    assert!(s.list_all_for_org(org_a).await.unwrap()[0].paused);

    // Sticky: a second pause on an ACTIVE pause is Ok(false) (the conditional
    // ON CONFLICT update matches no row), and the FIRST pause's evidence is
    // kept.
    let second = NewRoutePause {
        paused_by: PausedBy::Manual,
        reason: "manual overwrite attempt".into(),
        pass_rate: None,
        verdicts_in_window: None,
    };
    assert!(!s.pause_route(org_a, created.id, second).await.unwrap());
    let kept: String = sqlx::query_scalar("SELECT reason FROM route_pauses WHERE route_id = $1")
        .bind(created.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        kept.starts_with("auto:"),
        "first pause's evidence must be kept, got {kept:?}"
    );

    // Foreign org cannot resume.
    assert!(!s.resume_route(org_b, created.id).await.unwrap());
    assert!(
        s.get_route(org_a, created.id)
            .await
            .unwrap()
            .unwrap()
            .paused
    );

    // Owner resume: the record is RETAINED with resumed_at stamped (the
    // auto-pause verdict-window watermark) — NOT deleted — and the route
    // reads unpaused everywhere. A second resume is Ok(false).
    assert!(s.resume_route(org_a, created.id).await.unwrap());
    assert_eq!(
        pause_row_count(&pool, created.id).await,
        1,
        "resume must RETAIN the row as a watermark"
    );
    let resumed_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT resumed_at FROM route_pauses WHERE route_id = $1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(resumed_at.is_some(), "resume must stamp resumed_at");
    assert!(
        !s.get_route(org_a, created.id)
            .await
            .unwrap()
            .unwrap()
            .paused
    );
    assert!(!s.list_for_org(org_a).await.unwrap()[0].paused);
    assert!(!s.resume_route(org_a, created.id).await.unwrap());

    // Re-pause AFTER the resume: the resumed watermark row is overwritten by
    // the NEW pause's evidence (resumed_at back to NULL, paused again).
    let repause = NewRoutePause {
        paused_by: PausedBy::Manual,
        reason: "manual re-pause".into(),
        pass_rate: None,
        verdicts_in_window: None,
    };
    assert!(
        s.pause_route(org_a, created.id, repause).await.unwrap(),
        "re-pause after resume must succeed"
    );
    let (reason2, resumed2): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT reason, resumed_at FROM route_pauses WHERE route_id = $1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason2, "manual re-pause", "NEW evidence must be recorded");
    assert!(resumed2.is_none(), "re-pause must clear resumed_at");
    assert!(
        s.get_route(org_a, created.id)
            .await
            .unwrap()
            .unwrap()
            .paused
    );

    // Deleting the route GCs its pause record (no orphaned rows; a recreated
    // route gets a fresh id, so the documented delete-and-recreate edit flow
    // starts clean).
    assert!(s.delete_route(org_a, created.id).await.unwrap());
    assert_eq!(
        pause_row_count(&pool, created.id).await,
        0,
        "delete_route must GC the pause record"
    );
}

/// A pause survives unrelated dashboard writes (route create/delete) — the
/// pause rides its own gateway-owned table, keyed by route id, so editing
/// OTHER routes can never clear it.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_pause_survives_unrelated_route_writes() {
    let (s, _pool) = store().await;
    let org = Uuid::now_v7();
    let r1 = s.create_route(org, new_route("keep-paused")).await.unwrap();
    let r2 = s.create_route(org, new_route("to-delete")).await.unwrap();

    assert!(s
        .pause_route(
            org,
            r1.id,
            NewRoutePause {
                paused_by: PausedBy::Auto,
                reason: "auto: regressed".into(),
                pass_rate: Some(0.5),
                verdicts_in_window: Some(30),
            },
        )
        .await
        .unwrap());

    // Unrelated dashboard writes.
    let _r3 = s.create_route(org, new_route("new-route")).await.unwrap();
    assert!(s.delete_route(org, r2.id).await.unwrap());

    let listed = s.list_all_for_org(org).await.unwrap();
    let kept = listed.iter().find(|r| r.id == r1.id).expect("r1 listed");
    assert!(
        kept.paused,
        "pause must survive unrelated route create/delete"
    );
}
