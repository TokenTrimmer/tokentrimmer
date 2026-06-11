//! DB-gated round-trip test for the durable paired-A/B verdict sink
//! ([`tt_core::quality_persist::PostgresJudgeSink`] → `quality_verdicts`,
//! migration 0014).
//!
//! Run locally against an empty Postgres:
//! ```sh
//! docker run --rm -d --name tt-pg-ab-sampler -e POSTGRES_PASSWORD=postgres \
//!   -p 5545:5432 pgvector/pgvector:pg17
//! export TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5545/postgres
//! cargo test -p tt-core --test quality_verdicts_pg -- --include-ignored
//! docker rm -f tt-pg-ab-sampler
//! ```

use tt_core::quality_persist::PostgresJudgeSink;
use tt_core::quality_sample::{AbOrder, JudgeOutcome, JudgeSink};
use tt_plan_core::{JudgeVerdict, RiskBand, SampleScore};
use uuid::Uuid;

/// The full persisted column set read back in the round-trip assert:
/// (org_id, route_id, requested_model, served_model, verdict, reason,
/// judge_model, judge_cost_usd, baseline_cost_usd, baseline_dispatched,
/// optimized_position, orders_judged, orders_agreed).
type VerdictRow = (
    Uuid,
    Option<Uuid>,
    String,
    String,
    String,
    String,
    String,
    Option<f64>,
    Option<f64>,
    bool,
    Option<String>,
    i16,
    Option<bool>,
);

/// Build a fully-populated outcome for `request_id` (the metered happy path).
fn metered_outcome(org_id: Uuid, route_id: Uuid, request_id: Uuid) -> JudgeOutcome {
    JudgeOutcome {
        org_id,
        route_id: Some(route_id),
        requested_model: "gpt-4o".into(),
        served_model: "gpt-4o-mini".into(),
        score: SampleScore {
            request_id,
            verdict: JudgeVerdict::Degraded,
            reason: "dropped the dosage caveat".into(),
        },
        risk_band: RiskBand::High,
        judge_model: "judge-cheap".into(),
        judge_cost_usd: Some(0.000_1),
        baseline_cost_usd: Some(0.002),
        baseline_dispatched: true,
        optimized_position: AbOrder::OptimizedB,
        orders_judged: 2,
        orders_agreed: Some(true),
    }
}

const SELECT_ROW: &str = "SELECT org_id, route_id, requested_model, served_model, verdict, \
     reason, judge_model, judge_cost_usd::FLOAT8, baseline_cost_usd::FLOAT8, \
     baseline_dispatched, optimized_position, orders_judged, orders_agreed \
     FROM quality_verdicts WHERE request_id = $1";

/// Full-fidelity round trip: migrate an empty DB, record one outcome through
/// the production sink, read the row back and assert every persisted column;
/// then prove (a) an UNMETERED judge tax round-trips as SQL NULL (never 0),
/// and (b) the CHECK constraint rejects a bogus verdict string.
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_judge_sink_round_trips_verdict() {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("migrations (incl. 0014) apply cleanly");
    let pool = tt_core::connect(&url, 2).await.expect("connect");

    let org_id = Uuid::now_v7();
    let route_id = Uuid::now_v7();
    let request_id = Uuid::now_v7();
    let sink = PostgresJudgeSink::new(pool.clone());
    sink.record(metered_outcome(org_id, route_id, request_id))
        .await;

    // Read the row back. NUMERIC columns are cast to FLOAT8 for the assert.
    let row: VerdictRow = sqlx::query_as(SELECT_ROW)
        .bind(request_id)
        .fetch_one(&pool)
        .await
        .expect("the recorded verdict row must exist");

    assert_eq!(row.0, org_id);
    assert_eq!(row.1, Some(route_id));
    assert_eq!(row.2, "gpt-4o");
    assert_eq!(row.3, "gpt-4o-mini");
    assert_eq!(row.4, "degraded");
    assert_eq!(row.5, "dropped the dosage caveat");
    assert_eq!(row.6, "judge-cheap");
    let judge_cost = row.7.expect("metered judge tax persisted");
    assert!(
        (judge_cost - 0.000_1).abs() < 1e-9,
        "judge_cost_usd: {judge_cost}"
    );
    let baseline = row.8.expect("baseline_cost_usd persisted");
    assert!(
        (baseline - 0.002).abs() < 1e-9,
        "baseline_cost_usd: {baseline}"
    );
    assert!(row.9, "baseline_dispatched persisted");
    assert_eq!(row.10.as_deref(), Some("b"), "optimized slot persisted");
    assert_eq!(row.11, 2, "orders_judged persisted");
    assert_eq!(row.12, Some(true), "orders_agreed persisted");

    // An UNMETERED judge tax (no catalog pricing) + an unmetered-but-billed
    // baseline round-trip as SQL NULL — never a fabricated 0 — with
    // baseline_dispatched disambiguating "billed, unpriced" from "no dispatch".
    let unmetered_request = Uuid::now_v7();
    let mut unmetered = metered_outcome(org_id, route_id, unmetered_request);
    unmetered.judge_cost_usd = None;
    unmetered.baseline_cost_usd = None;
    sink.record(unmetered).await;
    let row: VerdictRow = sqlx::query_as(SELECT_ROW)
        .bind(unmetered_request)
        .fetch_one(&pool)
        .await
        .expect("the unmetered verdict row must exist");
    assert_eq!(row.7, None, "unmetered judge tax must be NULL, never 0");
    assert_eq!(row.8, None, "unmetered baseline must be NULL, never 0");
    assert!(row.9, "baseline_dispatched still marks the billed dispatch");

    // The CHECK constraint rejects a verdict outside the closed vocabulary —
    // a raw INSERT (bypassing the typed sink) must fail.
    let bogus = sqlx::query(
        "INSERT INTO quality_verdicts \
         (id, org_id, request_id, requested_model, served_model, verdict, judge_model) \
         VALUES ($1, $2, $3, 'm', 'm', 'bogus', 'j')",
    )
    .bind(Uuid::now_v7())
    .bind(org_id)
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await;
    assert!(
        bogus.is_err(),
        "verdict CHECK constraint must reject 'bogus'"
    );
}

/// The sink's documented fail-open invariant: a failing INSERT (here: a closed
/// pool) is warn-logged and swallowed — `record` returns normally, never
/// panics or propagates. This is load-bearing for the judge's fail-open
/// guarantee (the sink runs inside the detached judge task).
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (empty Postgres) — run with --include-ignored"]
async fn pg_judge_sink_insert_error_is_fail_open() {
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    tt_core::migrate_only(&url)
        .await
        .expect("migrations apply cleanly");
    let pool = tt_core::connect(&url, 2).await.expect("connect");
    let sink = PostgresJudgeSink::new(pool.clone());
    // Close the pool out from under the sink — every subsequent INSERT errors.
    pool.close().await;
    // Must complete without panicking; the error is warn-logged and swallowed.
    sink.record(metered_outcome(
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    ))
    .await;
}
