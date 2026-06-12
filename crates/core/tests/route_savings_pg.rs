//! DB-gated tests for the Phase-2.3 judge-tax netting query
//! ([`tt_core::route_savings::ROUTE_SAVINGS_SQL`] via
//! [`tt_core::route_savings::PostgresRouteSavingsSource`]) and the
//! [`tt_core::route_autopause::PgVerdictWindow`] recency window.
//!
//! Run locally against an empty Postgres:
//! ```sh
//! docker run --rm -d --name tt-pg-honesty -e POSTGRES_PASSWORD=postgres \
//!   -p 5553:5432 pgvector/pgvector:pg17
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5553/postgres
//! cargo test -p tt-core --test route_savings_pg -- --include-ignored
//! docker rm -f tt-pg-honesty
//! ```

use chrono::{DateTime, Duration, Utc};
use tt_core::route_autopause::{PgVerdictWindow, VerdictWindowSource};
use tt_core::route_savings::{PostgresRouteSavingsSource, RouteSavingsSource};
use uuid::Uuid;

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("migrations (incl. 0017) apply cleanly");
    tt_core::connect(&url, 2).await.expect("connect")
}

/// Insert one request_logs row with just the netting-relevant columns varied.
#[allow(clippy::too_many_arguments)]
async fn log_row(
    pool: &sqlx::PgPool,
    org: Uuid,
    route_id: Option<Uuid>,
    ts: DateTime<Utc>,
    cost: f64,
    baseline: f64,
    provider_cache_saved: f64,
    cache_bust_penalty: f64,
    shadow_model: Option<&str>,
    shadow_cost: Option<f64>,
) {
    sqlx::query(
        "INSERT INTO request_logs \
           (id, org_id, api_key_id, ts, provider, model, input_tokens, output_tokens, \
            cost_usd, baseline_cost_usd, provider_cache_saved_usd, cache_bust_penalty_usd, \
            cached, route_id, latency_ms, status, shadow_model, shadow_cost_usd) \
         VALUES ($1, $2, $3, $4, 'test', 'gpt-4o-mini', 10, 10, \
                 $5, $6, $7, $8, FALSE, $9, 5, 200, $10, $11)",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(Uuid::nil())
    .bind(ts)
    .bind(cost)
    .bind(baseline)
    .bind(provider_cache_saved)
    .bind(cache_bust_penalty)
    .bind(route_id)
    .bind(shadow_model)
    .bind(shadow_cost)
    .execute(pool)
    .await
    .expect("insert request_logs row");
}

/// Insert one quality_verdicts row with the tax columns varied.
#[allow(clippy::too_many_arguments)]
async fn verdict_row(
    pool: &sqlx::PgPool,
    org: Uuid,
    route_id: Option<Uuid>,
    ts: DateTime<Utc>,
    verdict: &str,
    judge_cost: Option<f64>,
    baseline_cost: Option<f64>,
    baseline_dispatched: bool,
) {
    sqlx::query(
        "INSERT INTO quality_verdicts \
           (id, org_id, route_id, request_id, ts, requested_model, served_model, \
            verdict, judge_model, judge_cost_usd, baseline_cost_usd, baseline_dispatched) \
         VALUES ($1, $2, $3, $4, $5, 'gpt-4o', 'gpt-4o-mini', $6, 'judge-cheap', $7, $8, $9)",
    )
    .bind(Uuid::now_v7())
    .bind(org)
    .bind(route_id)
    .bind(Uuid::now_v7())
    .bind(ts)
    .bind(verdict)
    .bind(judge_cost)
    .bind(baseline_cost)
    .bind(baseline_dispatched)
    .execute(pool)
    .await
    .expect("insert quality_verdicts row");
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// Full netting over seeded rows: per-route gross/judge-tax/shadow-tax/net,
/// the per-row GREATEST clamp (a negative-saving row contributes 0 to gross
/// — exactly `CostBreakdown::tt_saved_usd` semantics), unmetered rows counted
/// from BOTH sources, route-less rows excluded, `[start, end)` window bounds
/// respected, and a verdicts-only route surfacing a NEGATIVE net.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_route_savings_netting() {
    let pool = pool().await;
    let org = Uuid::now_v7();
    let route_a = Uuid::now_v7();
    let route_b = Uuid::now_v7();

    let end = Utc::now();
    let start = end - Duration::hours(24);
    let mid = end - Duration::hours(1);

    // ── request_logs: route A ────────────────────────────────────────────
    // A1 at EXACTLY window_start (inclusive): saved 0.010 - 0.002 = 0.008.
    log_row(
        &pool,
        org,
        Some(route_a),
        start,
        0.002,
        0.010,
        0.0,
        0.0,
        None,
        None,
    )
    .await;
    // A2: every subtrahend in play: 0.005 - 0.001 - 0.001 - 0.0005 = 0.0025.
    log_row(
        &pool,
        org,
        Some(route_a),
        mid,
        0.001,
        0.005,
        0.001,
        0.0005,
        None,
        None,
    )
    .await;
    // A3: NEGATIVE per-row saving (cost > baseline) → GREATEST clamps to 0,
    // it must NOT subtract from the others' gross.
    log_row(
        &pool,
        org,
        Some(route_a),
        mid,
        0.003,
        0.001,
        0.0,
        0.0,
        None,
        None,
    )
    .await;
    // A4: metered shadow row → shadow tax 0.0015, zero swap saving.
    log_row(
        &pool,
        org,
        Some(route_a),
        mid,
        0.004,
        0.004,
        0.0,
        0.0,
        Some("shadow-x"),
        Some(0.0015),
    )
    .await;
    // A5: UNMETERED shadow row (model set, cost NULL) → unmetered_tax_rows.
    log_row(
        &pool,
        org,
        Some(route_a),
        mid,
        0.002,
        0.002,
        0.0,
        0.0,
        Some("shadow-x"),
        None,
    )
    .await;
    // Route-less row (route_id NULL) with a huge saving → excluded entirely.
    log_row(&pool, org, None, mid, 0.001, 9.0, 0.0, 0.0, None, None).await;
    // At EXACTLY window_end (exclusive) → excluded.
    log_row(
        &pool,
        org,
        Some(route_a),
        end,
        0.001,
        9.0,
        0.0,
        0.0,
        None,
        None,
    )
    .await;

    // ── quality_verdicts: route A ────────────────────────────────────────
    // V1 metered acceptable: tax 0.0001 + 0.002.
    verdict_row(
        &pool,
        org,
        Some(route_a),
        mid,
        "acceptable",
        Some(0.0001),
        Some(0.002),
        true,
    )
    .await;
    // V2 metered degraded: tax 0.0002 + 0.001.
    verdict_row(
        &pool,
        org,
        Some(route_a),
        mid,
        "degraded",
        Some(0.0002),
        Some(0.001),
        true,
    )
    .await;
    // V3 unjudged spend row: unclear, judge_cost NULL → unmetered_tax_rows.
    verdict_row(&pool, org, Some(route_a), mid, "unclear", None, None, false).await;
    // Route-less verdict (L2-hit) → excluded from route attribution.
    verdict_row(
        &pool,
        org,
        None,
        mid,
        "acceptable",
        Some(0.1),
        Some(0.1),
        true,
    )
    .await;
    // Out-of-window verdict → excluded.
    verdict_row(
        &pool,
        org,
        Some(route_a),
        start - Duration::hours(1),
        "degraded",
        Some(0.1),
        Some(0.1),
        true,
    )
    .await;

    // ── route B: verdicts but NO traffic — the FULL OUTER JOIN must still
    // surface it, with a NEGATIVE net (verification spend, no swap saving). ─
    verdict_row(
        &pool,
        org,
        Some(route_b),
        mid,
        "degraded",
        Some(0.0005),
        Some(0.001),
        true,
    )
    .await;

    let source = PostgresRouteSavingsSource::new(pool.clone());
    let rows = source.route_savings(org, start, end).await.expect("query");
    assert_eq!(
        rows.len(),
        2,
        "exactly the two route-attributed routes: {rows:?}"
    );

    let a = rows
        .iter()
        .find(|r| r.route_id == route_a)
        .expect("route A");
    assert_eq!(
        a.requests, 5,
        "A1–A5 in-window; the window_end row excluded"
    );
    assert!(
        close(a.gross_saved_usd, 0.0105),
        "gross: {}",
        a.gross_saved_usd
    );
    assert!(
        close(a.judge_tax_usd, 0.0033),
        "judge tax: {}",
        a.judge_tax_usd
    );
    assert!(
        close(a.shadow_tax_usd, 0.0015),
        "shadow tax: {}",
        a.shadow_tax_usd
    );
    assert!(
        close(a.net_saved_usd, 0.0105 - 0.0033 - 0.0015),
        "net: {}",
        a.net_saved_usd
    );
    assert_eq!(
        a.unmetered_tax_rows, 2,
        "one unmetered shadow row + one unmetered verdict row"
    );
    assert_eq!(a.verdicts.judged, 3);
    assert_eq!(a.verdicts.acceptable, 1);
    assert_eq!(a.verdicts.degraded, 1);
    assert_eq!(a.verdicts.unclear, 1);
    assert!(close(a.verdicts.pass_rate.expect("classified"), 0.5));

    let b = rows
        .iter()
        .find(|r| r.route_id == route_b)
        .expect("route B");
    assert_eq!(b.requests, 0, "no traffic");
    assert!(close(b.gross_saved_usd, 0.0));
    assert!(close(b.judge_tax_usd, 0.0015));
    assert!(
        close(b.net_saved_usd, -0.0015),
        "negative net must survive at the aggregate level: {}",
        b.net_saved_usd
    );
    assert_eq!(b.verdicts.degraded, 1);
    assert!(close(b.verdicts.pass_rate.expect("classified"), 0.0));

    // A different org sees nothing.
    let other = source
        .route_savings(Uuid::now_v7(), start, end)
        .await
        .unwrap();
    assert!(other.is_empty());
}

/// [`PgVerdictWindow::recent_classified_counts`] excludes `unclear` rows and
/// honors LIMIT recency: 120 old degraded + 100 recent acceptable (+ 30 most
/// recent unclear) at limit 100 → (100, 0); at limit 150 the older degraded
/// tail enters → (100, 50).
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_verdict_window_recency_and_unclear_exclusion() {
    let pool = pool().await;
    let org = Uuid::now_v7();
    let route = Uuid::now_v7();

    // Distinct, strictly increasing ts per row (no DESC-order ties).
    let seed = |verdict: &'static str, base_offset_secs: i64, n: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO quality_verdicts \
                   (id, org_id, route_id, request_id, ts, requested_model, served_model, \
                    verdict, judge_model) \
                 SELECT gen_random_uuid(), $1, $2, gen_random_uuid(), \
                        now() - make_interval(secs => $3 - i), \
                        'gpt-4o', 'gpt-4o-mini', $4, 'judge-cheap' \
                 FROM generate_series(1, $5) i",
            )
            .bind(org)
            .bind(route)
            .bind(base_offset_secs as f64)
            .bind(verdict)
            .bind(n)
            .execute(&pool)
            .await
            .expect("seed verdicts");
        }
    };
    seed("degraded", 100_000, 120).await; // oldest
    seed("acceptable", 50_000, 100).await; // recent
    seed("unclear", 10_000, 30).await; // most recent — must never count

    let window = PgVerdictWindow::new(pool.clone());
    assert_eq!(
        window
            .recent_classified_counts(org, route, 100)
            .await
            .unwrap(),
        (100, 0),
        "limit must take the MOST RECENT classified verdicts (unclear skipped)"
    );
    assert_eq!(
        window
            .recent_classified_counts(org, route, 150)
            .await
            .unwrap(),
        (100, 50),
        "the older degraded tail enters at a larger limit"
    );
    // Unknown (org, route) → (0, 0), not an error.
    assert_eq!(
        window
            .recent_classified_counts(Uuid::now_v7(), route, 100)
            .await
            .unwrap(),
        (0, 0)
    );
}
