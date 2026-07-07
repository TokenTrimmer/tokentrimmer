//! P2a (learned-compression label-gap closure): verify migration 0034 adds the
//! `request_logs_trace_id_idx` partial index + the per-pair join
//! `quality_verdicts.request_id::text = request_logs.trace_id` plan uses it.
//!
//! These are `#[ignore]`d + gate on `TEST_DATABASE_URL` (an empty Postgres) —
//! the cloud CI gotcha: DB-integration tests are `#[ignore]`d + the public CI
//! doesn't run them. Run locally:
//! ```text
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
//! cargo test -p tt-core --test trace_id_index_pg -- --include-ignored
//! ```

use sqlx::Row;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("migrations (incl. 0034) apply cleanly");
    tt_core::connect(&url, 2).await.expect("connect")
}

/// Migration 0034 creates `request_logs_trace_id_idx` — the partial index on
/// `(org_id, trace_id) WHERE trace_id IS NOT NULL`. Confirms the index exists
/// with the right definition (partial + org_id-leading).
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
#[tokio::test]
async fn migration_0034_creates_trace_id_partial_index() {
    let pool = pool().await;

    let row = sqlx::query(
        "SELECT indexdef FROM pg_indexes \
         WHERE schemaname = 'public' AND tablename = 'request_logs' \
         AND indexname = 'request_logs_trace_id_idx'",
    )
    .fetch_one(&pool)
    .await
    .expect("the index row must exist (migration 0034 ran)");

    let indexdef: String = row.try_get("indexdef").expect("indexdef");
    assert!(
        indexdef.contains("request_logs_trace_id_idx"),
        "the index is named correctly: {indexdef}"
    );
    assert!(
        indexdef.contains("org_id, trace_id"),
        "the index leads with org_id (org-scoped join): {indexdef}"
    );
    assert!(
        indexdef.contains("WHERE (trace_id IS NOT NULL)"),
        "the index is PARTIAL on trace_id IS NOT NULL (NULL rows excluded): {indexdef}"
    );
}

/// The per-pair join `quality_verdicts.request_id::text = request_logs.trace_id`
/// (the RUNG 3 judge-verdict ↔ capture-pair join) CAN use the new partial index
/// when the planner isn't preferring a seq scan on a near-empty table. We
/// `SET enable_seqscan = off` to force the planner to surface the index choice
/// (the honest behavior at scale — on a multi-MB request_logs the index wins).
/// Confirms the cast direction + org-scoping produce an index-usable predicate.
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
#[tokio::test]
async fn trace_id_join_uses_partial_index() {
    let pool = pool().await;
    let org = uuid::Uuid::now_v7();
    let trace = uuid::Uuid::now_v7();

    // Insert one request_logs row with a trace_id so the join has a target.
    sqlx::query(
        "INSERT INTO request_logs (id, org_id, api_key_id, trace_id, ts, status, model, provider, \
         cost_usd, baseline_cost_usd, input_tokens, output_tokens, cached, latency_ms) \
         VALUES ($1, $2, $3, $4, now(), 200, 'gpt-4o', 'openai', 0, 0, 0, 0, false, 0)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(org)
    .bind(uuid::Uuid::now_v7())
    .bind(trace.to_string())
    .execute(&pool)
    .await
    .expect("insert request_logs row");

    // Force the planner off its near-empty-table seq-scan preference so the
    // index choice is visible.
    sqlx::query("SET enable_seqscan = off")
        .execute(&pool)
        .await
        .expect("SET enable_seqscan");

    // EXPLAIN the join (the cast direction: request_id::text = trace_id).
    // Fetch ALL plan lines (the Index Scan is rarely the first line).
    let rows = sqlx::query(
        "EXPLAIN (FORMAT TEXT) \
         SELECT count(*) FROM request_logs rl \
         JOIN quality_verdicts qv ON qv.org_id = rl.org_id \
         AND qv.request_id::text = rl.trace_id \
         WHERE rl.trace_id = $1 AND rl.org_id = $2",
    )
    .bind(trace.to_string())
    .bind(org)
    .fetch_all(&pool)
    .await
    .expect("EXPLAIN must succeed");
    let plan: String = rows
        .iter()
        .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        plan.contains("request_logs_trace_id_idx"),
        "the trace_id join must use the partial index (got plan):\n{plan}"
    );
}

/// A non-NULL `trace_id` filter CAN use the partial index; a NULL-trace filter
/// does NOT (the partial index excludes NULLs). With `enable_seqscan = off` the
/// planner surfaces the index for the NOT-NULL case + has NO index for the
/// NULL case (seq scan forced on). Confirms the partial-index predicate.
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
#[tokio::test]
async fn partial_index_excludes_null_trace_id() {
    let pool = pool().await;
    let org = uuid::Uuid::now_v7();

    // Insert one row with a trace_id + one without.
    for trace in [Some(uuid::Uuid::now_v7().to_string()), None] {
        sqlx::query(
            "INSERT INTO request_logs (id, org_id, api_key_id, trace_id, ts, status, model, provider, \
             cost_usd, baseline_cost_usd, input_tokens, output_tokens, cached, latency_ms) \
             VALUES ($1, $2, $3, $4, now(), 200, 'gpt-4o', 'openai', 0, 0, 0, 0, false, 0)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(org)
        .bind(uuid::Uuid::now_v7())
        .bind(trace)
        .execute(&pool)
        .await
        .expect("insert request_logs row");
    }

    sqlx::query("SET enable_seqscan = off")
        .execute(&pool)
        .await
        .expect("SET enable_seqscan");

    // IS NOT NULL filter → the partial index applies.
    let rows = sqlx::query(
        "EXPLAIN (FORMAT TEXT) SELECT count(*) FROM request_logs \
         WHERE org_id = $1 AND trace_id IS NOT NULL",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .expect("EXPLAIN");
    let plan_not_null: String = rows
        .iter()
        .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        plan_not_null.contains("request_logs_trace_id_idx"),
        "trace_id IS NOT NULL must use the partial index:\n{plan_not_null}"
    );

    // IS NULL filter → the partial index does NOT apply (it excludes NULLs).
    let rows = sqlx::query(
        "EXPLAIN (FORMAT TEXT) SELECT count(*) FROM request_logs \
         WHERE org_id = $1 AND trace_id IS NULL",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .expect("EXPLAIN");
    let plan_null: String = rows
        .iter()
        .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !plan_null.contains("request_logs_trace_id_idx"),
        "trace_id IS NULL must NOT use the partial index (it excludes NULLs):\n{plan_null}"
    );
}
