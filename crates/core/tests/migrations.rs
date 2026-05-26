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
