//! Pull a real `request_logs` telemetry window from the gateway's Postgres into
//! `Vec<tt_plan_core::RequestLog>`, so `tt inspect --suggest-plan --from-db` can
//! emit an immediately-runnable PlanInput.
//!
//! The money columns (`cost_usd`, `baseline_cost_usd`) are `NUMERIC(12,6)` in
//! Postgres; binding them straight into an `f64` errors at decode time (the
//! DB-2 reconciliation bug). The SELECT casts them to `float8` so they decode
//! cleanly.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use tt_plan_core::types::{L2TaskClass, RequestLog};

/// One row from `request_logs`, with NUMERIC money columns cast to float8.
#[derive(sqlx::FromRow)]
struct WindowRow {
    id: Uuid,
    org_id: Uuid,
    ts: DateTime<Utc>,
    provider: String,
    model: String,
    requested_model: Option<String>,
    input_tokens: i32,
    output_tokens: i32,
    cached_tokens: i32,
    cost_usd: f64,
    baseline_cost_usd: f64,
    cached: bool,
    cache_layer: Option<String>,
    route_id: Option<Uuid>,
    latency_ms: i32,
    upstream_latency_ms: Option<i32>,
    status: i32,
    tag: Option<String>,
}

impl WindowRow {
    fn into_request_log(self) -> RequestLog {
        RequestLog {
            id: self.id,
            org_id: self.org_id,
            ts: self.ts,
            provider: self.provider,
            model: self.model,
            requested_model: self.requested_model,
            input_tokens: self.input_tokens.max(0) as u32,
            output_tokens: self.output_tokens.max(0) as u32,
            cached_tokens: self.cached_tokens.max(0) as u32,
            cost_usd: self.cost_usd,
            baseline_cost_usd: self.baseline_cost_usd,
            cached: self.cached,
            cache_layer: self.cache_layer,
            matched_route_id: self.route_id,
            latency_ms: self.latency_ms.max(0) as u32,
            upstream_latency_ms: self.upstream_latency_ms.map(|v| v.max(0) as u32),
            status: self.status.clamp(0, u16::MAX as i32) as u16,
            tag: self.tag,
            // v1 maps base columns only; the L2/quality enrichment join is v2.
            embedding: None,
            finish_reason: None,
            body: None,
            response_body: None,
            task_class: L2TaskClass::default(),
            diff_saved_usd: None,
            minify_saved_est_usd: None,
        }
    }
}

/// Fetch the `request_logs` window `[since, until)` for an org.
///
/// `org`:
/// - `Some(id)` → pull exactly that org.
/// - `None` → auto-detect: exactly one distinct org in the window → use it;
///   zero or more than one → error (the caller must pass `--org`).
///
/// Returns `(resolved_org, rows)` ordered by `ts ASC`.
pub async fn fetch_window(
    pool: &PgPool,
    org: Option<Uuid>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> anyhow::Result<(Uuid, Vec<RequestLog>)> {
    let resolved = match org {
        Some(o) => o,
        None => resolve_single_org(pool, since, until).await?,
    };

    let rows = sqlx::query_as::<_, WindowRow>(
        "SELECT id, org_id, ts, provider, model, requested_model, input_tokens, output_tokens, \
                cached_tokens, cost_usd::float8 AS cost_usd, \
                baseline_cost_usd::float8 AS baseline_cost_usd, cached, cache_layer, \
                route_id, latency_ms, upstream_latency_ms, status, tag \
         FROM request_logs \
         WHERE org_id = $1 AND ts >= $2 AND ts < $3 \
         ORDER BY ts ASC",
    )
    .bind(resolved)
    .bind(since)
    .bind(until)
    .fetch_all(pool)
    .await
    .context("query request_logs window")?;

    Ok((
        resolved,
        rows.into_iter().map(WindowRow::into_request_log).collect(),
    ))
}

async fn resolve_single_org(
    pool: &PgPool,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> anyhow::Result<Uuid> {
    let orgs: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT DISTINCT org_id FROM request_logs WHERE ts >= $1 AND ts < $2 LIMIT 11",
    )
    .bind(since)
    .bind(until)
    .fetch_all(pool)
    .await
    .context("auto-detect org in window")?;

    match orgs.as_slice() {
        [] => anyhow::bail!(
            "no request_logs rows in the selected window — nothing to pull \
             (widen --window-days, or check DATABASE_URL points at the gateway's DB)"
        ),
        [one] => Ok(one.0),
        many => anyhow::bail!(
            "{} distinct orgs have rows in the window — pass --org <uuid> to choose one (found: {})",
            many.len(),
            many.iter()
                .map(|o| o.0.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod pg_tests {
    use super::*;
    use chrono::TimeZone;

    // TEST_DATABASE_URL-gated; runs against the MERGE_RUNBOOK pgvector gate.
    async fn pool() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        tt_core::migrate_only(&url).await.expect("migrations apply");
        Some(tt_core::connect(&url, 2).await.expect("connect"))
    }

    // The gate is persistent and these tests use fixed historical timestamps,
    // so clear each test's window first to stay idempotent across reruns /
    // concurrent prior-run residue (otherwise auto-detect sees stale orgs).
    async fn clear_window(pool: &PgPool, since: DateTime<Utc>, until: DateTime<Utc>) {
        sqlx::query("DELETE FROM request_logs WHERE ts >= $1 AND ts < $2")
            .bind(since)
            .bind(until)
            .execute(pool)
            .await
            .expect("clear window");
    }

    async fn seed(pool: &PgPool, org: Uuid, ts: DateTime<Utc>, cost: f64, baseline: f64) {
        sqlx::query(
            "INSERT INTO request_logs \
             (id, org_id, api_key_id, ts, provider, model, input_tokens, output_tokens, \
              cost_usd, baseline_cost_usd, cached, latency_ms, status) \
             VALUES (gen_random_uuid(), $1, gen_random_uuid(), $2, 'openai', 'gpt-4o', \
                     1000, 500, $3, $4, false, 120, 200)",
        )
        .bind(org)
        .bind(ts)
        .bind(cost)
        .bind(baseline)
        .execute(pool)
        .await
        .expect("seed request_logs");
    }

    // (a) NUMERIC money columns decode to f64 (the ::float8 cast regression),
    //     and the window filter + explicit org work.
    #[tokio::test]
    async fn fetches_window_and_decodes_numeric() {
        let Some(pool) = pool().await else { return };
        let org = Uuid::new_v4();
        // Unique fixed historical instant so concurrent tests don't collide.
        let ts = Utc.with_ymd_and_hms(2019, 1, 1, 12, 0, 0).unwrap();
        let since = Utc.with_ymd_and_hms(2019, 1, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2019, 1, 2, 0, 0, 0).unwrap();
        clear_window(&pool, since, until).await;
        seed(&pool, org, ts, 0.005, 0.010).await;

        let (resolved, rows) = fetch_window(&pool, Some(org), since, until)
            .await
            .expect("fetch ok");

        assert_eq!(resolved, org);
        assert_eq!(rows.len(), 1);
        assert!((rows[0].cost_usd - 0.005).abs() < 1e-9, "NUMERIC decoded");
        assert!((rows[0].baseline_cost_usd - 0.010).abs() < 1e-9);
        assert_eq!(rows[0].model, "gpt-4o");
        assert_eq!(rows[0].task_class, L2TaskClass::default());
    }

    // (b) Auto-detect resolves a single org in the window.
    #[tokio::test]
    async fn auto_detects_single_org() {
        let Some(pool) = pool().await else { return };
        let org = Uuid::new_v4();
        let ts = Utc.with_ymd_and_hms(2019, 2, 1, 12, 0, 0).unwrap();
        let since = Utc.with_ymd_and_hms(2019, 2, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2019, 2, 2, 0, 0, 0).unwrap();
        clear_window(&pool, since, until).await;
        seed(&pool, org, ts, 0.001, 0.002).await;

        let (resolved, rows) = fetch_window(&pool, None, since, until)
            .await
            .expect("auto-detect ok");
        assert_eq!(resolved, org);
        assert_eq!(rows.len(), 1);
    }

    // (c) Empty window → auto-detect errors with a helpful message.
    #[tokio::test]
    async fn empty_window_errors() {
        let Some(pool) = pool().await else { return };
        let since = Utc.with_ymd_and_hms(1990, 1, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(1990, 1, 2, 0, 0, 0).unwrap();
        clear_window(&pool, since, until).await;
        let err = fetch_window(&pool, None, since, until)
            .await
            .expect_err("empty window must error");
        assert!(err.to_string().contains("no request_logs rows"));
    }

    // (d) Ambiguous window (two orgs) → auto-detect errors asking for --org.
    #[tokio::test]
    async fn ambiguous_window_errors() {
        let Some(pool) = pool().await else { return };
        let ts = Utc.with_ymd_and_hms(2019, 3, 1, 12, 0, 0).unwrap();
        let since = Utc.with_ymd_and_hms(2019, 3, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2019, 3, 2, 0, 0, 0).unwrap();
        clear_window(&pool, since, until).await;
        seed(&pool, Uuid::new_v4(), ts, 0.001, 0.002).await;
        seed(&pool, Uuid::new_v4(), ts, 0.001, 0.002).await;

        let err = fetch_window(&pool, None, since, until)
            .await
            .expect_err("ambiguous window must error");
        assert!(err.to_string().contains("--org"));
    }
}
