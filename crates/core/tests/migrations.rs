//! Migration sanity checks. Real round-trip lives in compose-backed integration
//! tests added later; here we just verify the migrator compiles and references
//! the expected version.

#[test]
fn migrator_includes_first_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    assert!(
        !migrations.is_empty(),
        "no migrations registered — sqlx::migrate! macro pointed at empty dir"
    );
    let first = migrations
        .iter()
        .find(|m| m.version == 1)
        .expect("migration version 1 not found");
    assert!(
        first.description.to_lowercase().contains("request")
            || first.description.to_lowercase().contains("logs"),
        "migration 0001 description is '{}', expected to mention request/logs",
        first.description,
    );
}

#[test]
fn migrator_includes_cache_entries_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let second = migrations
        .iter()
        .find(|m| m.version == 2)
        .expect("migration version 2 not found");
    let desc = second.description.to_lowercase();
    assert!(
        desc.contains("cache") || desc.contains("entries"),
        "migration 0002 description is '{}', expected to mention cache/entries",
        second.description,
    );
}

#[test]
fn migrator_includes_inspect_runs_findings_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let third = migrations
        .iter()
        .find(|m| m.version == 3)
        .expect("migration version 3 not found");
    let desc = third.description.to_lowercase();
    assert!(
        desc.contains("inspect") || desc.contains("findings"),
        "migration 0003 description is '{}', expected to mention inspect/findings",
        third.description,
    );
}

#[test]
fn migrator_includes_plan_runs_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let fourth = migrations
        .iter()
        .find(|m| m.version == 4)
        .expect("migration version 4 not found");
    let desc = fourth.description.to_lowercase();
    assert!(
        desc.contains("plan") || desc.contains("runs"),
        "migration 0004 description is '{}', expected to mention plan/runs",
        fourth.description,
    );
}

#[test]
fn migrator_includes_cache_baseline_cost_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let tenth = migrations
        .iter()
        .find(|m| m.version == 10)
        .expect("migration version 10 not found");
    let desc = tenth.description.to_lowercase();
    assert!(
        desc.contains("baseline") || desc.contains("cost"),
        "migration 0010 description is '{}', expected to mention baseline/cost",
        tenth.description,
    );
}

#[test]
fn migrator_includes_provider_cache_saved_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let eleventh = migrations
        .iter()
        .find(|m| m.version == 11)
        .expect("migration version 11 not found");
    let desc = eleventh.description.to_lowercase();
    assert!(
        desc.contains("provider") || desc.contains("cache"),
        "migration 0011 description is '{}', expected to mention provider/cache",
        eleventh.description,
    );
}

/// Strict migrate-only path: connects to a real DB, applies all migrations,
/// returns Ok, and the schema is queryable.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn migrate_only_applies_schema() {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("migrate_only should apply cleanly to an empty DB");
    // Idempotent: a second run is a no-op, not an error.
    tt_core::migrate_only(&url)
        .await
        .expect("migrate_only should be idempotent");
    // Schema is present: the v1 migration creates request_logs.
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema='public' AND table_name='request_logs')",
    )
    .fetch_one(&pool)
    .await
    .expect("query");
    assert!(exists, "request_logs table should exist after migrate_only");
}
