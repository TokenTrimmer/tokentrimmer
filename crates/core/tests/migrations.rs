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

#[test]
fn migrator_includes_provider_cache_tokens_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let fifteenth = migrations
        .iter()
        .find(|m| m.version == 15)
        .expect("migration version 15 not found");
    let desc = fifteenth.description.to_lowercase();
    assert!(
        desc.contains("cache") || desc.contains("tokens"),
        "migration 0015 description is '{}', expected to mention cache/tokens",
        fifteenth.description,
    );
}

#[test]
fn migrator_includes_cache_bust_penalty_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let sixteenth = migrations
        .iter()
        .find(|m| m.version == 16)
        .expect("migration version 16 not found");
    let desc = sixteenth.description.to_lowercase();
    assert!(
        desc.contains("cache") || desc.contains("bust"),
        "migration 0016 description is '{}', expected to mention cache/bust",
        sixteenth.description,
    );
}

#[test]
fn migrator_includes_request_logs_batch_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let seventeenth = migrations
        .iter()
        .find(|m| m.version == 17)
        .expect("migration version 17 not found");
    let desc = seventeenth.description.to_lowercase();
    assert!(
        desc.contains("batch"),
        "migration 0017 description is '{}', expected to mention batch",
        seventeenth.description,
    );
}

#[test]
fn migrator_includes_l2_verify_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let eighteenth = migrations
        .iter()
        .find(|m| m.version == 18)
        .expect("migration version 18 not found");
    let desc = eighteenth.description.to_lowercase();
    assert!(
        desc.contains("l2") || desc.contains("verify") || desc.contains("lexical"),
        "migration 0018 description is '{}', expected to mention l2/verify/lexical",
        eighteenth.description,
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
    // Migration 0014: the persisted negative-savings column is present.
    let bust_col: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema='public' AND table_name='request_logs' \
         AND column_name='cache_bust_penalty_usd')",
    )
    .fetch_one(&pool)
    .await
    .expect("query");
    assert!(
        bust_col,
        "request_logs.cache_bust_penalty_usd should exist after migration 0016"
    );
}

#[test]
fn migrator_includes_routing_honesty_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let seventeenth = migrations
        .iter()
        .find(|m| m.version == 19)
        .expect("migration version 19 not found");
    let desc = seventeenth.description.to_lowercase();
    assert!(
        desc.contains("routing") || desc.contains("honesty") || desc.contains("pause"),
        "migration 0019 description is '{}', expected to mention routing/honesty/pause",
        seventeenth.description,
    );
}

#[test]
fn migrator_includes_quality_verdicts_migration() {
    let migrations = tt_core::db::MIGRATOR.iter().collect::<Vec<_>>();
    let fourteenth = migrations
        .iter()
        .find(|m| m.version == 14)
        .expect("migration version 14 not found");
    let desc = fourteenth.description.to_lowercase();
    assert!(
        desc.contains("quality") || desc.contains("verdicts"),
        "migration 0014 description is '{}', expected to mention quality/verdicts",
        fourteenth.description,
    );
}

/// DB-gated: a real `PostgresRequestLogWriter` INSERT against the migrated
/// schema round-trips the provider prompt-cache token columns (migration
/// 0015), keeping NULL ("provider didn't report") distinct from 0
/// ("provider reported zero"). Catches column-order/type drift in INSERT_SQL
/// that the parser-based bind-count guard cannot.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn request_log_insert_round_trips_provider_cache_token_columns() {
    use tt_telemetry::request_logs::{
        postgres::PostgresRequestLogWriter, RequestLogRow, RequestLogWriter,
    };
    use uuid::Uuid;

    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url).await.expect("migrate");
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    let writer = PostgresRequestLogWriter::new(pool.clone());

    let base = RequestLogRow {
        id: Uuid::now_v7(),
        org_id: Uuid::nil(),
        api_key_id: Uuid::nil(),
        ts: chrono::Utc::now(),
        provider: "test-provider".into(),
        model: "test-1".into(),
        input_tokens: 120,
        output_tokens: 60,
        cached_tokens: 80,
        cost_usd: 0.001,
        baseline_cost_usd: 0.001,
        provider_cache_saved_usd: 0.0002,
        cache_bust_penalty_usd: 0.0,
        cached: false,
        cache_layer: None,
        route_id: None,
        latency_ms: 5,
        upstream_latency_ms: None,
        status: 200,
        tag: Some("db-cache-tokens".into()),
        error_class: None,
        trace_id: None,
        truncated: false,
        shadow_model: None,
        shadow_cost_usd: None,
        traffic_split_arm: None,
        cache_read_input_tokens: Some(80),
        cache_creation_input_tokens: Some(20),
        batch_eligible: false,
        batch_forgone_usd: 0.0,
        route_paused: false,
    };
    let reported_id = base.id;
    writer.write(base.clone()).await.expect("insert reported");

    let mut unreported = base.clone();
    unreported.id = Uuid::now_v7();
    unreported.cache_read_input_tokens = None;
    unreported.cache_creation_input_tokens = None;
    let unreported_id = unreported.id;
    writer.write(unreported).await.expect("insert unreported");

    let mut zero = base;
    zero.id = Uuid::now_v7();
    zero.cache_read_input_tokens = Some(0);
    zero.cache_creation_input_tokens = Some(0);
    let zero_id = zero.id;
    writer.write(zero).await.expect("insert zero");

    let fetch = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<i32>, Option<i32>)>(
                "SELECT cache_read_input_tokens, cache_creation_input_tokens \
                 FROM request_logs WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch row")
        }
    };

    assert_eq!(fetch(reported_id).await, (Some(80), Some(20)));
    assert_eq!(
        fetch(unreported_id).await,
        (None, None),
        "unreported must persist as SQL NULL"
    );
    assert_eq!(
        fetch(zero_id).await,
        (Some(0), Some(0)),
        "an explicit provider-reported zero must persist as 0, not NULL"
    );

    // Cleanup so reruns / other DB tests see a stable table.
    sqlx::query("DELETE FROM request_logs WHERE tag = 'db-cache-tokens'")
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// DB-gated: a real `PostgresRequestLogWriter` INSERT against the migrated
/// schema round-trips the advisory batch-eligibility columns (migration
/// 0017). `batch_eligible = true` + a nonzero forgone discount survive
/// write→read; the NOT NULL DEFAULTs cover legacy/unmarked rows (a row
/// written with `false`/`0.0` reads back exactly that). Catches column-order
/// or type drift in INSERT_SQL that the parser-based bind-count guard cannot.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn request_log_insert_round_trips_batch_columns() {
    use tt_telemetry::request_logs::{
        postgres::PostgresRequestLogWriter, RequestLogRow, RequestLogWriter,
    };
    use uuid::Uuid;

    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url).await.expect("migrate");
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    let writer = PostgresRequestLogWriter::new(pool.clone());

    let marked = RequestLogRow {
        route_paused: false,
        id: Uuid::now_v7(),
        org_id: Uuid::nil(),
        api_key_id: Uuid::nil(),
        ts: chrono::Utc::now(),
        provider: "test-provider".into(),
        model: "batch-eligible".into(),
        input_tokens: 1000,
        output_tokens: 500,
        cached_tokens: 0,
        cost_usd: 0.025,
        baseline_cost_usd: 0.025,
        provider_cache_saved_usd: 0.0,
        cache_bust_penalty_usd: 0.0,
        cached: false,
        cache_layer: None,
        route_id: None,
        latency_ms: 5,
        upstream_latency_ms: None,
        status: 200,
        tag: Some("db-batch-columns".into()),
        error_class: None,
        trace_id: None,
        truncated: false,
        shadow_model: None,
        shadow_cost_usd: None,
        traffic_split_arm: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        batch_eligible: true,
        batch_forgone_usd: 0.0125,
    };
    let marked_id = marked.id;
    writer.write(marked.clone()).await.expect("insert marked");

    let mut unmarked = marked;
    unmarked.id = Uuid::now_v7();
    unmarked.batch_eligible = false;
    unmarked.batch_forgone_usd = 0.0;
    let unmarked_id = unmarked.id;
    writer.write(unmarked).await.expect("insert unmarked");

    let fetch = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (bool, f64)>(
                "SELECT batch_eligible, batch_forgone_usd::FLOAT8 \
                 FROM request_logs WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch row")
        }
    };

    let (eligible, forgone) = fetch(marked_id).await;
    assert!(eligible, "batch_eligible=true must persist");
    assert!(
        (forgone - 0.0125).abs() < 1e-9,
        "forgone discount must round-trip, got {forgone}"
    );
    assert_eq!(
        fetch(unmarked_id).await,
        (false, 0.0),
        "unmarked traffic persists as false/0 (the NOT NULL DEFAULTs)"
    );

    // Cleanup so reruns / other DB tests see a stable table.
    sqlx::query("DELETE FROM request_logs WHERE tag = 'db-batch-columns'")
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// DB-gated T0: the FULL `PostgresRequestLogWriter::write` bind chain executes
/// against a real Postgres and the row round-trips. The parser-based
/// `insert_sql_column_placeholder_bind_counts_match` guard only checks the SQL
/// STRING; it cannot see the `.bind(...)` chain itself — #163 shipped a stray
/// duplicate bind (32 binds against 31 placeholders) that the string guard
/// could not catch. (Empirically, sqlx 0.8 silently IGNORES surplus binds
/// beyond the prepared statement's parameter count, so that wart was benign in
/// production — but a chain that is short, mis-ordered, or type-mismatched is
/// NOT benign, and only a real round-trip exercises it.) This test pins the
/// chain itself.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn request_logs_insert_round_trips_against_postgres() {
    use tt_telemetry::request_logs::{
        postgres::PostgresRequestLogWriter, RequestLogRow, RequestLogWriter,
    };
    use uuid::Uuid;

    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url).await.expect("migrate");
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    let writer = PostgresRequestLogWriter::new(pool.clone());

    let row = RequestLogRow {
        id: Uuid::now_v7(),
        org_id: Uuid::nil(),
        api_key_id: Uuid::nil(),
        ts: chrono::Utc::now(),
        provider: "test-provider".into(),
        model: "test-1".into(),
        input_tokens: 100,
        output_tokens: 50,
        cached_tokens: 0,
        cost_usd: 0.0045,
        baseline_cost_usd: 0.0045,
        provider_cache_saved_usd: 0.0,
        cache_bust_penalty_usd: 0.0,
        cached: false,
        cache_layer: None,
        route_id: None,
        latency_ms: 800,
        upstream_latency_ms: Some(750),
        status: 200,
        tag: Some("db-t0-bind-chain".into()),
        error_class: None,
        trace_id: Some("trace-t0".into()),
        truncated: false,
        shadow_model: None,
        shadow_cost_usd: None,
        traffic_split_arm: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        batch_eligible: false,
        batch_forgone_usd: 0.0,
        route_paused: true,
    };
    let id = row.id;
    writer
        .write(row)
        .await
        .expect("PostgresRequestLogWriter::write must succeed (bind chain == placeholders)");

    let (provider, route_paused) = sqlx::query_as::<_, (String, bool)>(
        "SELECT provider, route_paused FROM request_logs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("fetch row");
    assert_eq!(provider, "test-provider");
    assert!(route_paused, "route_paused=true must survive write→read");

    sqlx::query("DELETE FROM request_logs WHERE tag = 'db-t0-bind-chain'")
        .execute(&pool)
        .await
        .expect("cleanup");
}
