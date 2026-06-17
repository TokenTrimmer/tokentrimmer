# PROD-1 Inspect → Plan → apply loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the two stubbed proof-loop handoffs so a self-hosted (DB-connected) operator can run `tt inspect --suggest-plan --from-db` → `tt plan --input` → `tt plan --apply` → `tt audit verify` as one motion.

**Architecture:** All changes are in the public OSS repo. (1) A new `telemetry_window` reader pulls a real `request_logs` window from the gateway's Postgres (via `DATABASE_URL`) and `--suggest-plan --from-db` freezes those rows into the emitted `PlanInput`. (2) `tt plan --apply` is rewired from an honest stub to a real local apply: it validates + idempotently writes the proposed routes to the cloud-shaped `routes` table via `PostgresRoutingStore::create_route`, then emits a signed `plan.applied` entry to `.claude/AUDIT-CHAIN.jsonl` (a new file-backed signed-append path on a per-machine key at `~/.tokentrimmer/audit-signing-key`). No cloud endpoint; opt-in/default-off; no replay/golden changes.

**Tech Stack:** Rust, `sqlx` (Postgres), `tt-plan-core`, `tt-routing` (Postgres `RoutingStore`), `tt-telemetry::audit` (Ed25519 + BLAKE3 chain), clap, `ed25519_dalek`, `hex`.

**Spec:** `docs/superpowers/specs/2026-06-16-prod1-inspect-plan-apply-loop-design.md`

**Reference — exact current code locations (verified this session):**
- `crates/cli/src/plan_suggest.rs` — `build_plan_input_json(path)` builds the skeleton (`org_id` nil, `requests: []`).
- `crates/cli/src/main.rs` — `Inspect` clap variant lines 34–64; `Plan` variant lines 72–93; `main()` dispatch arms lines 535–559; `run_suggest_plan` lines 2166–2181; `run_plan` lines 2283–2345 (the `if apply { bail!() }` stub is lines ~2333–2342).
- `crates/plan-core/src/types.rs` — `RequestLog` lines 23–118; `L2TaskClass` (has `Default = ChatCompletions`) lines 129–138; `PlanInput` lines 486–515.
- `crates/core/migrations/0001_request_logs.up.sql` — `request_logs` (NOT-NULL: id, org_id, api_key_id, ts, provider, model, input_tokens, output_tokens, cost_usd, baseline_cost_usd, cached, latency_ms, status; `cached_tokens` DEFAULT 0; money cols `NUMERIC(12,6)`).
- `crates/routing/src/store.rs` — `NewRoute { name, priority, enabled, when: RouteConditions, then: RouteAction }`; `RoutingStore::{create_route, list_all_for_org}`; `PostgresRoutingStore::new(pool)`.
- `crates/routing/src/validate.rs:201` — `pub fn validate_route_has_effect(then: &RouteAction) -> Result<(), ValidationError>`.
- `crates/telemetry/src/audit/mod.rs` — `AuditEntry`, `Actor`, `PayloadFields`, `compute_hash`, `verify_chain` (all pub); `pub use writer::{AuditWriter, InMemoryAuditWriter}`.
- `crates/telemetry/src/audit/writer.rs` — `InMemoryAuditWriter::write` (lines ~84–155) builds entries (genesis vs `prev.hash`/`seq+1`, `compute_hash`, `try_sign`).
- `crates/cli/src/audit.rs` — `tt audit verify` reads `.claude/AUDIT-CHAIN.jsonl`: optional `{"meta":true,"verifying_key":"<hex>"}` preamble + one `AuditEntry` JSON per line.
- `crates/core/src/db.rs:49` — `tt_core::connect(url, max_connections) -> Result<PgPool, sqlx::Error>` (the standard pool builder).
- `crates/core/tests/route_pauses_pg.rs:32-66` — the `TEST_DATABASE_URL` + `migrate_only` + advisory-locked `CREATE TABLE IF NOT EXISTS routes (... id UUID PRIMARY KEY DEFAULT gen_random_uuid() ...)` bootstrap pattern for the cloud-owned `routes` table.

**Type-bridging note (used in Task 5):** `tt_plan_core::types::{RouteConditions, RouteAction}` and `tt_routing::{RouteConditions, RouteAction}` are *mirrored* (distinct) types with an identical JSON shape (the HTTP `POST /v1/routes` path already round-trips between them). Bridge plan-core → routing via serde: `serde_json::from_value(serde_json::to_value(&x)?)?`.

**Testing note:** CLI `cli_spawn_smoke` tests time out in the local sandbox but pass in CI. DB-backed tests are `TEST_DATABASE_URL`-gated and run locally against the MERGE_RUNBOOK `pgvector:pg17` gate. To avoid cross-test interference on the shared DB, each DB test seeds rows at a **unique fixed historical timestamp** and queries a matching narrow window.

---

### Task 1: `telemetry_window` reader

**Files:**
- Create: `crates/cli/src/telemetry_window.rs`
- Modify: `crates/cli/src/lib.rs` (add `pub mod telemetry_window;`)
- Test: in-module `#[cfg(test)] mod pg_tests` (TEST_DATABASE_URL-gated)

- [ ] **Step 1: Declare the module**

In `crates/cli/src/lib.rs`, add next to the existing `pub mod plan_suggest;`:

```rust
pub mod telemetry_window;
```

- [ ] **Step 2: Write the reader (no test yet — DB test needs the type to exist)**

Create `crates/cli/src/telemetry_window.rs`:

```rust
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
        "SELECT id, org_id, ts, provider, model, input_tokens, output_tokens, \
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
```

- [ ] **Step 3: Build to verify it compiles**

Run: `cargo build -p tt-cli`
Expected: PASS (no warnings about unused — `fetch_window` is `pub`).

- [ ] **Step 4: Write the DB-gated test**

Append to `crates/cli/src/telemetry_window.rs`:

```rust
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

    async fn seed(
        pool: &PgPool,
        org: Uuid,
        ts: DateTime<Utc>,
        cost: f64,
        baseline: f64,
    ) {
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
        seed(&pool, org, ts, 0.005, 0.010).await;

        let since = Utc.with_ymd_and_hms(2019, 1, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2019, 1, 2, 0, 0, 0).unwrap();
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
        seed(&pool, org, ts, 0.001, 0.002).await;

        let since = Utc.with_ymd_and_hms(2019, 2, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2019, 2, 2, 0, 0, 0).unwrap();
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
        seed(&pool, Uuid::new_v4(), ts, 0.001, 0.002).await;
        seed(&pool, Uuid::new_v4(), ts, 0.001, 0.002).await;

        let since = Utc.with_ymd_and_hms(2019, 3, 1, 0, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2019, 3, 2, 0, 0, 0).unwrap();
        let err = fetch_window(&pool, None, since, until)
            .await
            .expect_err("ambiguous window must error");
        assert!(err.to_string().contains("--org"));
    }
}
```

- [ ] **Step 5: Run the DB test against the live gate**

Run: `TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p tt-cli telemetry_window -- --include-ignored`
Expected: PASS (4 tests) when the pgvector gate is up; the tests early-return (pass) when `TEST_DATABASE_URL` is unset.

- [ ] **Step 6: Commit**

```bash
git add crates/cli/src/telemetry_window.rs crates/cli/src/lib.rs
git commit -m "feat(cli): request_logs telemetry-window reader for PlanInput (PROD-1)"
```

---

### Task 2: `--suggest-plan --from-db` materializes the window

**Files:**
- Modify: `crates/cli/src/plan_suggest.rs` (refactor `build_plan_input_json` to accept org/requests/window)
- Modify: `crates/cli/src/main.rs` (`Inspect` clap variant + dispatch arm + `run_suggest_plan`)
- Test: `crates/cli/src/plan_suggest.rs` in-module (non-DB: the inner builder freezes requests)

- [ ] **Step 1: Write the failing test for the inner builder**

In `crates/cli/src/plan_suggest.rs`, add to `#[cfg(test)] mod tests`:

```rust
    // (e) The inner builder injects org_id + frozen requests into the PlanInput.
    #[test]
    fn inner_builder_freezes_org_and_requests() {
        use tt_plan_core::types::{L2TaskClass, RequestLog};

        let req = RequestLog {
            id: Uuid::new_v4(),
            org_id: Uuid::from_u128(7),
            ts: Utc::now(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            input_tokens: 1000,
            output_tokens: 500,
            cached_tokens: 0,
            cost_usd: 0.005,
            baseline_cost_usd: 0.010,
            cached: false,
            cache_layer: None,
            matched_route_id: None,
            latency_ms: 100,
            upstream_latency_ms: None,
            status: 200,
            tag: None,
            embedding: None,
            finish_reason: None,
            body: None,
            response_body: None,
            task_class: L2TaskClass::default(),
            diff_saved_usd: None,
            minify_saved_est_usd: None,
        };
        let now = Utc::now();
        // A path with no model strings → empty proposed_routes, but requests + org
        // must still be frozen in.
        let dir = std::env::temp_dir();
        let json = build_plan_input_json_inner(
            dir.to_str().unwrap(),
            Uuid::from_u128(7),
            &[req],
            now - chrono::Duration::days(3),
            now,
        )
        .expect("inner builder ok");

        let parsed: PlanInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.org_id, Uuid::from_u128(7));
        assert_eq!(parsed.requests.len(), 1);
        assert_eq!(parsed.requests[0].model, "gpt-4o");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p tt-cli inner_builder_freezes_org_and_requests`
Expected: FAIL — `build_plan_input_json_inner` not found.

- [ ] **Step 3: Refactor `build_plan_input_json` to delegate to an inner builder**

In `crates/cli/src/plan_suggest.rs`, replace the body of `build_plan_input_json` (lines 96–168) so the skeleton path delegates, and add the inner builder. Keep all model-scan / pricing logic identical — only `org_id`, `requests`, and the window become parameters:

```rust
pub fn build_plan_input_json(path: &str) -> anyhow::Result<String> {
    let now = Utc::now();
    build_plan_input_json_inner(
        path,
        Uuid::nil(),
        &[],
        now - chrono::Duration::days(7),
        now,
    )
}

/// Build a `PlanInput` JSON for `path`, injecting a concrete `org_id`, a frozen
/// set of `requests`, and an explicit replay window. `build_plan_input_json`
/// calls this with the skeleton defaults (nil org, empty requests, 7-day
/// window); `--from-db` calls it with a real telemetry window.
pub fn build_plan_input_json_inner(
    path: &str,
    org_id: Uuid,
    requests: &[tt_plan_core::types::RequestLog],
    window_start: chrono::DateTime<Utc>,
    window_end: chrono::DateTime<Utc>,
) -> anyhow::Result<String> {
    let models = collect_models_from_path(path)?;

    let mut proposed_routes: Vec<ProposedRoute> = Vec::new();
    for current_model in &models {
        if let Ok(hit) = tt_preview::pricing::lookup(current_model) {
            let current_cost =
                tt_preview::pricing::cost_usd(STD_INPUT_TOKENS, STD_OUTPUT_TOKENS, &hit);
            let suggestions = route_suggestions::suggest(
                current_model,
                current_cost,
                STD_INPUT_TOKENS,
                STD_OUTPUT_TOKENS,
                tt_preview::classifier::TaskClass::Classification,
            );
            proposed_routes.extend(suggestions_to_proposed_routes(current_model, &suggestions));
        } else {
            tracing::warn!(
                model = %current_model,
                "model not in pricing catalog — skipping route suggestion"
            );
        }
    }

    let plan_id = Uuid::new_v4();

    let mut pricing_table: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for route in &proposed_routes {
        let Some(target) = route.then.target_model.as_deref() else {
            continue;
        };
        if let Ok(hit) = tt_preview::pricing::lookup(target) {
            pricing_table
                .entry(format!("{}:{}", hit.provider, target))
                .or_insert_with(|| {
                    serde_json::json!({
                        "input_per_million": hit.input_per_m,
                        "output_per_million": hit.output_per_m,
                        "cached_input_per_million": null
                    })
                });
        }
    }

    let plan_input = serde_json::json!({
        "plan_id": plan_id,
        "org_id": org_id,
        "window_start": window_start.to_rfc3339(),
        "window_end": window_end.to_rfc3339(),
        "requests": requests,
        "proposed_routes": proposed_routes,
        "pricing": pricing_table,
        "config": {
            "l1_ttl_seconds": null,
            "l2_threshold_sweep": [0.85, 0.90, 0.92, 0.95],
            "l2_ttl_seconds": null
        },
        "seed": 42,
        "bootstrap_iterations": 1000
    });

    serde_json::to_string_pretty(&plan_input).map_err(|e| anyhow::anyhow!("serialize: {e}"))
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p tt-cli plan_suggest`
Expected: PASS (existing 4 tests + the new `inner_builder_freezes_org_and_requests`).

- [ ] **Step 5: Add the clap flags to the `Inspect` variant**

In `crates/cli/src/main.rs`, inside the `Inspect { … }` variant (after the `suggest_plan` field, before the closing `}` at line 64), add:

```rust
        /// With --suggest-plan: pull a real `request_logs` telemetry window
        /// from the gateway's Postgres (DATABASE_URL) into the emitted
        /// PlanInput's `requests`, making the file immediately runnable.
        #[arg(long, requires = "suggest_plan")]
        from_db: bool,
        /// With --from-db: the org UUID to pull. If omitted, auto-detected when
        /// the window has exactly one org (errors if ambiguous).
        #[arg(long)]
        org: Option<String>,
        /// With --from-db: telemetry window size in days.
        #[arg(long, default_value_t = 7)]
        window_days: i64,
```

- [ ] **Step 6: Thread the new fields through the dispatch arm**

In `crates/cli/src/main.rs`, update the `Command::Inspect { … }` match arm (lines 535–549) to destructure + pass the new fields, and `.await` the now-async suggest path:

```rust
        Command::Inspect {
            path,
            fail_on,
            output,
            cost_diff,
            base,
            fail_on_cost_increase,
            suggest_plan,
            from_db,
            org,
            window_days,
        } => {
            if cost_diff {
                run_cost_diff(&path, &base, output.as_deref(), fail_on_cost_increase)?;
            } else if suggest_plan {
                run_suggest_plan(&path, output.as_deref(), from_db, org.as_deref(), window_days)
                    .await?;
            } else {
                run_inspect(&path, &fail_on, output.as_deref())?;
            }
        }
```

- [ ] **Step 7: Make `run_suggest_plan` async and pull from the DB when `--from-db`**

In `crates/cli/src/main.rs`, replace `run_suggest_plan` (lines 2166–2181) with:

```rust
async fn run_suggest_plan(
    path: &str,
    output: Option<&str>,
    from_db: bool,
    org: Option<&str>,
    window_days: i64,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let json = if from_db {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .context(
                "--from-db requires DATABASE_URL (the gateway's Postgres connection string)",
            )?;
        let pool = tt_core::connect(&url, 4)
            .await
            .context("connect to DATABASE_URL")?;
        let until = chrono::Utc::now();
        let since = until - chrono::Duration::days(window_days.max(1));
        let org_uuid = match org {
            Some(s) => Some(uuid::Uuid::parse_str(s).context("--org must be a UUID")?),
            None => None,
        };
        let (resolved_org, requests) =
            tt_cli::telemetry_window::fetch_window(&pool, org_uuid, since, until).await?;
        tt_cli::ui::note(&format!(
            "pulled {} request_logs rows for org {} ({}-day window)",
            requests.len(),
            resolved_org,
            window_days
        ));
        tt_cli::plan_suggest::build_plan_input_json_inner(
            path,
            resolved_org,
            &requests,
            since,
            until,
        )?
    } else {
        tt_cli::plan_suggest::build_plan_input_json(path)?
    };

    match output {
        Some(p) if !p.is_empty() && p != "-" => {
            std::fs::write(p, &json)
                .map_err(|e| anyhow::anyhow!("failed to write plan input to {p}: {e}"))?;
            let hint = if from_db {
                format!("wrote runnable plan-input to {p}  (then: tt plan --input {p})")
            } else {
                format!("wrote plan-input skeleton to {p}  (edit org_id + requests, then: tt plan --input {p})")
            };
            tt_cli::ui::note(&hint);
        }
        _ => {
            print!("{json}");
        }
    }

    Ok(())
}
```

- [ ] **Step 8: Build + run the workspace tests**

Run: `cargo build -p tt-cli && cargo test -p tt-cli plan_suggest`
Expected: PASS. (`cargo build` confirms the async dispatch + new clap fields compile.)

- [ ] **Step 9: Commit**

```bash
git add crates/cli/src/plan_suggest.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt inspect --suggest-plan --from-db freezes a real telemetry window (PROD-1)"
```

---

### Task 3: Extract a shared `build_entry` + `generate_signing_key` in telemetry

**Files:**
- Modify: `crates/telemetry/src/audit/writer.rs` (extract `build_entry`, add `generate_signing_key`, refactor `InMemoryAuditWriter::write`)
- Modify: `crates/telemetry/src/audit/mod.rs` (re-export both)
- Test: existing `writer.rs` tests prove no behavior change; add one direct `build_entry` chaining test

- [ ] **Step 1: Add `build_entry` + `generate_signing_key` to `writer.rs`**

In `crates/telemetry/src/audit/writer.rs`, add these free functions (alongside the existing imports for `SigningKey`, `compute_hash`, `PayloadFields`, `Actor`, `AuditEntry`, `AuditError`). The signing-trait import matches what `InMemoryAuditWriter::write` already uses (`ed25519_dalek::signature::Signer` for `try_sign`):

```rust
/// Generate a fresh Ed25519 signing key (OS RNG). Kept here so callers don't
/// need a direct `rand_core` dependency.
pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut rand_core::OsRng)
}

/// Build (hash + Ed25519-sign) a new audit entry chaining onto `prev` (or
/// genesis when `prev` is `None`). Shared by every writer so the chain rules
/// live in exactly one place.
pub fn build_entry(
    signing_key: &SigningKey,
    prev: Option<&AuditEntry>,
    org_id: uuid::Uuid,
    actor: Actor,
    event: String,
    payload: serde_json::Value,
) -> Result<AuditEntry, AuditError> {
    use ed25519_dalek::signature::Signer;

    let seq = prev.map_or(0, |p| p.seq + 1);
    let (prev_hash_str, prev_hash_bytes): (String, [u8; 32]) = match prev {
        Some(p) => {
            let decoded = hex::decode(&p.hash).map_err(|e| AuditError::Storage(e.to_string()))?;
            if decoded.len() != 32 {
                return Err(AuditError::Storage(format!(
                    "prev hash decoded to {} bytes, expected 32",
                    decoded.len()
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&decoded);
            (p.hash.clone(), arr)
        }
        None => {
            let zeroes = [0u8; 32];
            (hex::encode(zeroes), zeroes)
        }
    };

    let id = uuid::Uuid::new_v4();
    let timestamp = chrono::Utc::now();
    let fields = PayloadFields {
        id,
        org_id,
        timestamp,
        actor: &actor,
        event: &event,
        payload: &payload,
        seq,
    };
    let hash = compute_hash(&prev_hash_bytes, &fields)?;
    let hash_hex = hash.to_hex().to_string();
    let signature = signing_key
        .try_sign(hash.as_bytes())
        .map_err(|e| AuditError::Signing(e.to_string()))?;
    let signature_hex = hex::encode(signature.to_bytes());

    Ok(AuditEntry {
        id,
        org_id,
        seq,
        timestamp,
        actor,
        event,
        payload,
        prev_hash: prev_hash_str,
        hash: hash_hex,
        signature: signature_hex,
    })
}
```

> Note: if `writer.rs` doesn't already `use` `compute_hash` / `PayloadFields` / `Actor` / `AuditEntry` / `AuditError` at module scope, add `use super::{compute_hash, Actor, AuditEntry, AuditError, PayloadFields};`. Confirm the exact `try_sign` trait import the existing `write` impl uses and reuse it.

- [ ] **Step 2: Refactor `InMemoryAuditWriter::write` to call `build_entry`**

In `crates/telemetry/src/audit/writer.rs`, replace the body of the `write` impl (the lock + seq/prev_hash/hash/sign block, lines ~84–155) so it delegates — preserving the lock-once-per-org semantics:

```rust
    async fn write(
        &self,
        org_id: Uuid,
        actor: Actor,
        event: String,
        payload: serde_json::Value,
    ) -> Result<AuditEntry, AuditError> {
        let mut guard = self
            .chains
            .lock()
            .map_err(|_| AuditError::Storage("mutex poisoned".to_string()))?;
        let chain = guard.entry(org_id).or_default();
        let entry = build_entry(&self.signing_key, chain.last(), org_id, actor, event, payload)?;
        chain.push(entry.clone());
        Ok(entry)
    }
```

- [ ] **Step 3: Re-export from `mod.rs`**

In `crates/telemetry/src/audit/mod.rs`, update the re-export line 25:

```rust
pub use writer::{build_entry, generate_signing_key, AuditWriter, InMemoryAuditWriter};
```

- [ ] **Step 4: Add a direct chaining test for `build_entry`**

In `crates/telemetry/src/audit/writer.rs` `#[cfg(test)]` tests, add:

```rust
    #[test]
    fn build_entry_chains_and_verifies() {
        let key = super::generate_signing_key();
        let org = uuid::Uuid::new_v4();

        let g = super::build_entry(&key, None, org, Actor::System, "genesis".into(), serde_json::json!({}))
            .expect("genesis");
        assert_eq!(g.seq, 0);
        assert_eq!(g.prev_hash, "0".repeat(64));

        let next = super::build_entry(&key, Some(&g), org, Actor::System, "plan.applied".into(), serde_json::json!({"k":"v"}))
            .expect("next");
        assert_eq!(next.seq, 1);
        assert_eq!(next.prev_hash, g.hash);

        super::super::verify_chain(&[g, next], &key.verifying_key()).expect("chain verifies");
    }
```

> Adjust the `verify_chain` path (`super::super::verify_chain` vs `crate::audit::verify_chain`) to match the module layout.

- [ ] **Step 5: Run the telemetry audit tests**

Run: `cargo test -p tt-telemetry audit`
Expected: PASS — all existing writer/verify tests (proving no behavior change) plus `build_entry_chains_and_verifies`.

- [ ] **Step 6: Commit**

```bash
git add crates/telemetry/src/audit/writer.rs crates/telemetry/src/audit/mod.rs
git commit -m "refactor(telemetry): extract shared audit build_entry + generate_signing_key (PROD-1)"
```

---

### Task 4: `local_audit` — per-machine key + file-backed signed append

**Files:**
- Create: `crates/cli/src/local_audit.rs`
- Modify: `crates/cli/src/lib.rs` (add `pub mod local_audit;`)
- Test: in-module `#[cfg(test)]` (temp-path append + verify; no real `$HOME`/cwd touched)

- [ ] **Step 1: Declare the module**

In `crates/cli/src/lib.rs`, add:

```rust
pub mod local_audit;
```

- [ ] **Step 2: Write the module**

Create `crates/cli/src/local_audit.rs`:

```rust
//! Local signed audit-chain append for `tt plan --apply`.
//!
//! The public CLI otherwise only *verifies* chains; this is the minimal local
//! APPEND path so a `plan.applied` entry lands in `.claude/AUDIT-CHAIN.jsonl`
//! and is provable with `tt audit verify`. The Ed25519 signing key is persisted
//! per-machine at `~/.tokentrimmer/audit-signing-key` (mode 0600), generated on
//! first use — a local-operator trust model.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use ed25519_dalek::SigningKey;
use uuid::Uuid;

use tt_telemetry::audit::{build_entry, generate_signing_key, Actor, AuditEntry};

/// Default local audit chain path (same file `tt audit verify` reads).
pub const DEFAULT_CHAIN_PATH: &str = ".claude/AUDIT-CHAIN.jsonl";

/// Load the per-machine signing key, generating + persisting one at
/// `~/.tokentrimmer/audit-signing-key` (mode 0600) on first use.
pub fn load_or_create_signing_key() -> anyhow::Result<SigningKey> {
    let path = signing_key_path()?;
    if path.exists() {
        let hex_str = std::fs::read_to_string(&path)
            .with_context(|| format!("read signing key {}", path.display()))?;
        let bytes = hex::decode(hex_str.trim()).context("signing key hex decode")?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes (64 hex chars)"))?;
        Ok(SigningKey::from_bytes(&arr))
    } else {
        let key = generate_signing_key();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        write_private(&path, &hex::encode(key.to_bytes()))?;
        Ok(key)
    }
}

fn signing_key_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .context("HOME not set — cannot locate ~/.tokentrimmer/audit-signing-key")?;
    Ok(PathBuf::from(home)
        .join(".tokentrimmer")
        .join("audit-signing-key"))
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create signing key {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)
        .with_context(|| format!("create signing key {}", path.display()))
}

/// Append a signed entry to the JSONL chain at `chain_path`, returning the
/// verifying-key hex. Creates the file with a
/// `{"meta":true,"verifying_key":"<hex>"}` preamble when absent; otherwise
/// chains onto the last entry.
pub fn append_entry(
    chain_path: &Path,
    signing_key: &SigningKey,
    org_id: Uuid,
    event: &str,
    payload: serde_json::Value,
) -> anyhow::Result<String> {
    let verifying_hex = hex::encode(signing_key.verifying_key().to_bytes());

    let existing = read_entries(chain_path)?;
    let entry = build_entry(
        signing_key,
        existing.last(),
        org_id,
        Actor::System,
        event.to_string(),
        payload,
    )
    .context("build audit entry")?;

    let new_file = !chain_path.exists();
    if let Some(parent) = chain_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(chain_path)
        .with_context(|| format!("open chain {}", chain_path.display()))?;
    if new_file {
        let preamble = serde_json::json!({"meta": true, "verifying_key": verifying_hex});
        writeln!(f, "{}", serde_json::to_string(&preamble)?)?;
    }
    writeln!(f, "{}", serde_json::to_string(&entry)?)?;
    Ok(verifying_hex)
}

/// Read audit entries from a JSONL chain file (skipping a `meta` preamble line).
/// Returns an empty vec when the file does not exist.
fn read_entries(chain_path: &Path) -> anyhow::Result<Vec<AuditEntry>> {
    if !chain_path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(chain_path)
        .with_context(|| format!("read chain {}", chain_path.display()))?;
    let mut entries = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(t).context("parse chain line")?;
        if v.get("meta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        entries.push(serde_json::from_value(v).context("parse audit entry")?);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_preamble_then_chains_and_verifies() {
        let dir = std::env::temp_dir().join(format!("tt-local-audit-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let chain = dir.join("AUDIT-CHAIN.jsonl");
        let key = generate_signing_key();
        let org = Uuid::new_v4();

        // First append → file created with preamble + 1 entry.
        let vk1 = append_entry(&chain, &key, org, "plan.applied", serde_json::json!({"n":1}))
            .expect("first append");
        // Second append → chains onto it (seq 1).
        let vk2 = append_entry(&chain, &key, org, "plan.applied", serde_json::json!({"n":2}))
            .expect("second append");
        assert_eq!(vk1, vk2, "stable verifying key");

        let entries = read_entries(&chain).expect("read");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[1].prev_hash, entries[0].hash);

        // The chain verifies under the verifying key parsed from the hex.
        let vk_bytes: [u8; 32] = hex::decode(&vk1).unwrap().try_into().unwrap();
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&vk_bytes).unwrap();
        tt_telemetry::audit::verify_chain(&entries, &vk).expect("verifies");

        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p tt-cli local_audit`
Expected: PASS (`append_creates_preamble_then_chains_and_verifies`).

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/local_audit.rs crates/cli/src/lib.rs
git commit -m "feat(cli): local file-backed signed audit-chain append + per-machine key (PROD-1)"
```

---

### Task 5: `plan_apply` — validate + idempotent route write + signed `plan.applied`

**Files:**
- Create: `crates/cli/src/plan_apply.rs`
- Modify: `crates/cli/src/lib.rs` (add `pub mod plan_apply;`)
- Test: in-module pure unit tests (`plan_routes_to_apply`) + a DB-gated `apply_routes` integration test

- [ ] **Step 1: Declare the module**

In `crates/cli/src/lib.rs`, add:

```rust
pub mod plan_apply;
```

- [ ] **Step 2: Write the failing pure-helper test**

Create `crates/cli/src/plan_apply.rs` with just the helper + its tests first:

```rust
//! Local `tt plan --apply`: write the projected routes to the gateway's
//! Postgres `routes` table and emit a signed `plan.applied` audit row.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::Context;
use ed25519_dalek::SigningKey;
use sqlx::PgPool;
use uuid::Uuid;

use tt_plan_core::types::{PlanResult, ProposedRoute};
use tt_routing::{validate_route_has_effect, NewRoute, PostgresRoutingStore, RoutingStore};

use crate::local_audit;

/// Outcome of planning which routes to create.
pub struct ApplyPlan {
    /// Routes that pass validation and don't already exist (by name).
    pub to_create: Vec<NewRoute>,
    /// Names skipped because the route has no effect (no-op).
    pub skipped_noop: Vec<String>,
    /// Names skipped because a route with that name already exists.
    pub skipped_existing: Vec<String>,
}

/// Convert proposed routes (plan-core types) into `tt_routing::NewRoute` specs,
/// dropping no-ops and names that already exist. Pure — no DB, no IO.
///
/// The plan-core and routing `RouteConditions`/`RouteAction` are mirrored types
/// with an identical JSON shape; bridge them via serde (same as the HTTP path).
pub fn plan_routes_to_apply(
    existing_names: &HashSet<String>,
    proposed: &[ProposedRoute],
) -> anyhow::Result<ApplyPlan> {
    let mut to_create = Vec::new();
    let mut skipped_noop = Vec::new();
    let mut skipped_existing = Vec::new();

    for r in proposed {
        let when: tt_routing::RouteConditions =
            serde_json::from_value(serde_json::to_value(&r.when).context("encode conditions")?)
                .context("decode conditions as tt_routing::RouteConditions")?;
        let then: tt_routing::RouteAction =
            serde_json::from_value(serde_json::to_value(&r.then).context("encode action")?)
                .context("decode action as tt_routing::RouteAction")?;

        if validate_route_has_effect(&then).is_err() {
            skipped_noop.push(r.name.clone());
            continue;
        }
        if existing_names.contains(&r.name) {
            skipped_existing.push(r.name.clone());
            continue;
        }
        to_create.push(NewRoute {
            name: r.name.clone(),
            priority: r.priority,
            enabled: r.enabled,
            when,
            then,
        });
    }

    Ok(ApplyPlan {
        to_create,
        skipped_noop,
        skipped_existing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_plan_core::types::{RouteAction, RouteConditions};

    fn proposed(name: &str, target: Option<&str>) -> ProposedRoute {
        ProposedRoute {
            id: Uuid::new_v4(),
            name: name.to_string(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            then: RouteAction {
                format_switch: None,
                diff: false,
                target_model: target.map(String::from),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
            },
        }
    }

    #[test]
    fn keeps_effective_new_routes() {
        let existing = HashSet::new();
        let plan = plan_routes_to_apply(&existing, &[proposed("swap-mini", Some("gpt-4o-mini"))])
            .expect("ok");
        assert_eq!(plan.to_create.len(), 1);
        assert_eq!(plan.to_create[0].name, "swap-mini");
    }

    #[test]
    fn skips_noop_route() {
        // target_model None + no other modifier == no effect.
        let existing = HashSet::new();
        let plan = plan_routes_to_apply(&existing, &[proposed("noop", None)]).expect("ok");
        assert!(plan.to_create.is_empty());
        assert_eq!(plan.skipped_noop, vec!["noop".to_string()]);
    }

    #[test]
    fn skips_existing_by_name() {
        let mut existing = HashSet::new();
        existing.insert("swap-mini".to_string());
        let plan = plan_routes_to_apply(&existing, &[proposed("swap-mini", Some("gpt-4o-mini"))])
            .expect("ok");
        assert!(plan.to_create.is_empty());
        assert_eq!(plan.skipped_existing, vec!["swap-mini".to_string()]);
    }
}
```

- [ ] **Step 3: Run the pure-helper tests to verify they pass**

Run: `cargo test -p tt-cli plan_apply::tests`
Expected: PASS (3 tests). If `skips_noop_route` fails, confirm a `target_model: None` action with no other modifier is what `validate_route_has_effect` rejects (it is — that's the no-op guard); adjust the fixture's modifiers if the validator's no-op definition differs.

- [ ] **Step 4: Add `apply_routes` (orchestration)**

Append to `crates/cli/src/plan_apply.rs` (before the `#[cfg(test)]` module):

```rust
/// Apply the proposed routes for `org_id`: validate + dedup, confirm,
/// idempotently create, then emit a signed `plan.applied` audit entry to
/// `chain_path`. `signing_key` + `chain_path` are injected so this is testable
/// without touching the real `$HOME`/cwd.
#[allow(clippy::too_many_arguments)]
pub async fn apply_routes(
    pool: &PgPool,
    org_id: Uuid,
    plan_id: Uuid,
    proposed: &[ProposedRoute],
    result: &PlanResult,
    assume_yes: bool,
    signing_key: &SigningKey,
    chain_path: &Path,
) -> anyhow::Result<()> {
    if org_id.is_nil() {
        anyhow::bail!(
            "PlanInput.org_id is nil — regenerate with `tt inspect --suggest-plan --from-db --org <id>` \
             (or set a real org_id) before applying"
        );
    }

    let store = PostgresRoutingStore::new(pool.clone());
    let existing = store
        .list_all_for_org(org_id)
        .await
        .map_err(|e| anyhow::anyhow!("list existing routes: {e}"))?;
    let existing_names: HashSet<String> = existing.into_iter().map(|r| r.name).collect();

    let plan = plan_routes_to_apply(&existing_names, proposed)?;
    for n in &plan.skipped_noop {
        crate::ui::note(&format!("skipping no-op route '{n}'"));
    }
    for n in &plan.skipped_existing {
        crate::ui::note(&format!("skipping '{n}' (already exists)"));
    }
    if plan.to_create.is_empty() {
        crate::ui::note("nothing to apply (no new routes with an effect)");
        return Ok(());
    }

    crate::ui::note(&format!(
        "about to create {} route(s) for org {org_id}:",
        plan.to_create.len()
    ));
    for r in &plan.to_create {
        let tgt = r.then.target_model.as_deref().unwrap_or("(modifier-only)");
        crate::ui::note(&format!("  + {} → {}", r.name, tgt));
    }

    if !assume_yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "refusing to apply without confirmation on a non-interactive stdin — pass --yes to proceed"
            );
        }
        print!("Apply these {} route(s)? [y/N] ", plan.to_create.len());
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("read confirmation")?;
        let ans = line.trim().to_ascii_lowercase();
        if ans != "y" && ans != "yes" {
            crate::ui::note("aborted — no routes applied");
            return Ok(());
        }
    }

    let mut created: Vec<String> = Vec::new();
    for spec in plan.to_create {
        let name = spec.name.clone();
        store
            .create_route(org_id, spec)
            .await
            .map_err(|e| anyhow::anyhow!("create route '{name}': {e}"))?;
        created.push(name);
    }
    crate::ui::ok(&format!(
        "created {} route(s); the gateway applies them on its next refresh (~60s)",
        created.len()
    ));

    let payload = serde_json::json!({
        "plan_id": plan_id.to_string(),
        "applied_routes": created,
        "projected_savings_usd": result.aggregates.projected_savings_usd,
        "projected_savings_pct": result.aggregates.projected_savings_pct,
    });
    let verifying_hex =
        local_audit::append_entry(chain_path, signing_key, org_id, "plan.applied", payload)?;
    crate::ui::ok(&format!(
        "recorded plan.applied to {} — verify with: tt audit verify --key-hex {}",
        chain_path.display(),
        verifying_hex
    ));

    Ok(())
}
```

> Confirm the `tt_routing` re-export paths compile (`tt_routing::{RouteConditions, RouteAction, NewRoute, PostgresRoutingStore, RoutingStore, validate_route_has_effect}`). If any isn't re-exported at the crate root, import it from its submodule (e.g. `tt_routing::store::NewRoute`). Confirm `result.aggregates.projected_savings_usd` / `projected_savings_pct` field names match `PlanResult` (they're the same fields `run_plan` already prints).

- [ ] **Step 5: Add the DB-gated integration test**

Append to the `#[cfg(test)] mod tests` in `crates/cli/src/plan_apply.rs`:

```rust
    // Minimal cloud-shaped `routes` table (mirrors tokentrimmer-cloud 0002),
    // same bootstrap the routing-store pg tests use.
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

    async fn db_with_routes() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        tt_core::migrate_only(&url).await.expect("migrations");
        let pool = tt_core::connect(&url, 2).await.expect("connect");
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query("SELECT pg_advisory_xact_lock(0x74740017)")
            .execute(&mut *tx)
            .await
            .expect("advisory lock");
        sqlx::query(CREATE_ROUTES_TABLE)
            .execute(&mut *tx)
            .await
            .expect("create routes table");
        tx.commit().await.expect("commit");
        Some(pool)
    }

    fn empty_result() -> PlanResult {
        // A zero/default PlanResult is enough for the audit payload; replay
        // correctness is covered by plan-core's own tests.
        serde_json::from_value(serde_json::json!({
            "plan_id": Uuid::nil(),
            "sample_size": 0,
            "aggregates": {
                "projected_savings_usd": 0.0,
                "projected_savings_pct": 0.0,
                "requests_rerouted": 0
            },
            "per_route": [],
            "caveats": []
        }))
        .expect("PlanResult shape — adjust to match the real struct if this fails")
    }

    #[tokio::test]
    async fn apply_creates_routes_idempotently_and_records_proof() {
        let Some(pool) = db_with_routes().await else { return };
        let org = Uuid::new_v4();
        let key = tt_telemetry::audit::generate_signing_key();
        let dir = std::env::temp_dir().join(format!("tt-apply-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let chain = dir.join("AUDIT-CHAIN.jsonl");

        let routes = vec![proposed("swap-mini-itest", Some("gpt-4o-mini"))];
        let result = empty_result();

        // First apply → creates the route + records plan.applied.
        apply_routes(&pool, org, Uuid::new_v4(), &routes, &result, true, &key, &chain)
            .await
            .expect("first apply");
        let after_first = PostgresRoutingStore::new(pool.clone())
            .list_all_for_org(org)
            .await
            .expect("list");
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].name, "swap-mini-itest");

        // Second apply → idempotent (skips existing-by-name), no new route.
        apply_routes(&pool, org, Uuid::new_v4(), &routes, &result, true, &key, &chain)
            .await
            .expect("second apply");
        let after_second = PostgresRoutingStore::new(pool.clone())
            .list_all_for_org(org)
            .await
            .expect("list");
        assert_eq!(after_second.len(), 1, "no duplicate route created");

        // The proof chain verifies and has exactly one plan.applied entry
        // (the second apply was a no-op, so it appended nothing).
        let entries = super::read_entries(&chain).expect("read chain");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, "plan.applied");
        tt_telemetry::audit::verify_chain(&entries, &key.verifying_key()).expect("verifies");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn apply_rejects_nil_org() {
        let Some(pool) = db_with_routes().await else { return };
        let key = tt_telemetry::audit::generate_signing_key();
        let chain = std::env::temp_dir().join("unused-nil-org.jsonl");
        let err = apply_routes(
            &pool,
            Uuid::nil(),
            Uuid::new_v4(),
            &[proposed("x", Some("gpt-4o-mini"))],
            &empty_result(),
            true,
            &key,
            &chain,
        )
        .await
        .expect_err("nil org must error");
        assert!(err.to_string().contains("nil"));
    }
```

> `empty_result()` builds a `PlanResult` from JSON to avoid hand-constructing every field. If the real `PlanResult` shape differs, fix the JSON (or construct it directly) — the only fields read are `aggregates.projected_savings_usd` / `projected_savings_pct`.

- [ ] **Step 6: Run the tests against the live gate**

Run: `TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p tt-cli plan_apply`
Expected: PASS — 3 pure tests + 2 DB tests (which early-return when `TEST_DATABASE_URL` is unset).

- [ ] **Step 7: Commit**

```bash
git add crates/cli/src/plan_apply.rs crates/cli/src/lib.rs
git commit -m "feat(cli): local tt plan --apply — validate + idempotent route write + signed plan.applied (PROD-1)"
```

---

### Task 6: Wire `tt plan --apply` (replace the stub) + `--yes` flag

**Files:**
- Modify: `crates/cli/src/main.rs` (`Plan` clap variant + dispatch arm + `run_plan`)

- [ ] **Step 1: Add the `--yes` flag to the `Plan` variant**

In `crates/cli/src/main.rs`, inside the `Plan { … }` variant (after the `apply` field, before the closing `}` at line 93), add:

```rust
        /// With --apply: skip the interactive confirmation prompt (for CI /
        /// automation). Ignored without --apply.
        #[arg(long)]
        yes: bool,
```

- [ ] **Step 2: Update the dispatch arm to pass `yes` and `.await`**

In `crates/cli/src/main.rs`, update the `Command::Plan { … }` match arm (lines 550–559):

```rust
        Command::Plan {
            input,
            output,
            example,
            apply,
            yes,
        } => {
            run_plan(input.as_deref(), output.as_deref(), example, apply, yes).await?;
        }
```

- [ ] **Step 3: Make `run_plan` async, capture fields before the move, and replace the stub**

In `crates/cli/src/main.rs`, change `run_plan`'s signature to `async` + add `yes`, capture the fields the apply path needs *before* `plan_input` is moved into `replay`, and replace the `if apply { anyhow::bail!(...) }` block (lines ~2333–2342) with a real apply:

```rust
async fn run_plan(
    input: Option<&str>,
    output: Option<&str>,
    example: bool,
    apply: bool,
    yes: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;

    if example {
        print_plan_example();
        return Ok(());
    }
    let input_path = input.ok_or_else(|| {
        anyhow::anyhow!("usage: tt plan --input <plan_input.json>  (or --example)")
    })?;

    let raw = std::fs::read_to_string(input_path)
        .map_err(|e| anyhow::anyhow!("read {input_path}: {e}"))?;
    let plan_input: tt_plan_core::PlanInput =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parse {input_path}: {e}"))?;

    // Capture what the apply path needs before `plan_input` is consumed.
    let org_id = plan_input.org_id;
    let plan_id = plan_input.plan_id;
    let proposed = plan_input.proposed_routes.clone();

    let result =
        tt_plan_core::replay(plan_input).map_err(|e| anyhow::anyhow!("replay failed: {e}"))?;

    let payload = match output {
        Some(p) if p.ends_with(".json") => serde_json::to_string_pretty(&result)?,
        _ => format_plan_text(&result),
    };

    match output {
        Some(p) if p != "-" => {
            std::fs::write(p, &payload)?;
            tt_cli::ui::note(&format!("wrote plan result to {p}"));
        }
        _ => {
            print!("{payload}");
        }
    }

    let agg = &result.aggregates;
    if agg.projected_savings_usd > 0.0 {
        tt_cli::ui::ok(&format!(
            "Projected savings ${:.4} ({:.1}%) · {} of {} requests rerouted",
            agg.projected_savings_usd,
            agg.projected_savings_pct,
            agg.requests_rerouted,
            result.sample_size
        ));
    } else {
        tt_cli::ui::note("No projected savings for this config.");
    }

    if apply {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .context(
                "tt plan --apply requires DATABASE_URL (the gateway's Postgres connection string)",
            )?;
        let pool = tt_core::connect(&url, 4)
            .await
            .context("connect to DATABASE_URL")?;
        let signing_key = tt_cli::local_audit::load_or_create_signing_key()?;
        let chain_path = std::path::Path::new(tt_cli::local_audit::DEFAULT_CHAIN_PATH);
        tt_cli::plan_apply::apply_routes(
            &pool,
            org_id,
            plan_id,
            &proposed,
            &result,
            yes,
            &signing_key,
            chain_path,
        )
        .await?;
    }

    Ok(())
}
```

- [ ] **Step 4: Build the whole workspace + run all CLI tests**

Run: `cargo build --workspace && cargo test -p tt-cli`
Expected: PASS. (`build --workspace` confirms the async `run_plan`, new clap fields, and cross-crate references compile.)

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/main.rs
git commit -m "feat(cli): wire real local tt plan --apply + --yes (PROD-1)"
```

---

### Task 7: Document the closed loop in `GETTING_STARTED.md`

**Files:**
- Modify: `GETTING_STARTED.md` (the `tt plan` section, ~lines 230–247)

- [ ] **Step 1: Replace the documented flow**

In `GETTING_STARTED.md`, replace the `tt plan` section (the bash block + the `tt plan --apply` not-wired note around lines 230–247) with the closed, self-hosted loop:

````markdown
# 3. `tt` proof loop — discover → simulate → realize → prove

For a self-hosted gateway (your own `DATABASE_URL`), the whole loop runs locally:

```bash
# Discover + freeze a real telemetry window into a runnable PlanInput
#   --org is auto-detected when the window has exactly one org.
tt inspect --suggest-plan . --from-db --window-days 7 --output plan.json

# Simulate — deterministic replay of the frozen window (text summary)
tt plan --input plan.json

# (optional) full PlanResult as JSON
tt plan --input plan.json --output result.json

# Realize — write the proposed routes to the gateway's routes table.
#   Dry-runs + prompts for confirmation; the gateway applies them within ~60s.
#   Use --yes in CI to skip the prompt.
tt plan --input plan.json --apply

# Prove — verify the signed plan.applied entry recorded by --apply
tt audit verify
```

Without `--from-db`, `tt inspect --suggest-plan` still emits a skeleton you fill in
by hand. `--from-db` and `--apply` both require `DATABASE_URL` (the gateway's
Postgres). `--apply` records a signed `plan.applied` entry to
`.claude/AUDIT-CHAIN.jsonl` using a per-machine key at
`~/.tokentrimmer/audit-signing-key`, and prints the verifying key for `tt audit verify`.
````

- [ ] **Step 2: Sanity-check the docs build / inspect gate**

Run: `tt inspect GETTING_STARTED.md` (or `cargo run -p tt-cli -- inspect GETTING_STARTED.md`)
Expected: no errors (the required `tt inspect .` CI check scans docs; ensure no accidental high-severity model-string findings were introduced).

- [ ] **Step 3: Commit**

```bash
git add GETTING_STARTED.md
git commit -m "docs: document the closed Inspect→Plan→apply→verify loop (PROD-1)"
```

---

## Final verification (after all tasks)

- [ ] **Full workspace gate (mirror the required CI checks):**

```bash
cargo fmt -p tt-cli -p tt-telemetry -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
  cargo test -p tt-cli -- --include-ignored      # telemetry_window + plan_apply DB tests
cargo test -p tt-telemetry audit
cargo run -p tt-cli -- inspect .                  # tt inspect . required check
cargo test -p tt-plan-core                        # determinism goldens untouched — must stay green
```

Expected: all green. The plan-replay determinism goldens MUST be unchanged (this work touches neither the replay math nor the bootstrap).

> Per the CI memory: signature/field-ripple changes can pass `cargo build` but fail test targets — always run `clippy --workspace --all-targets` + `test --workspace --no-run`. The `cli_spawn_smoke` tests time out locally but pass in CI.

---

## Self-review (against the spec)

**Spec coverage:**
- Handoff 1 (runnable PlanInput from real `request_logs`, `::float8` cast, org auto-detect, frozen window) → Tasks 1 + 2. ✓
- Handoff 2 (validate → dry-run/confirm/`--yes` → idempotent `create_route` → signed `plan.applied`, nil-org reject, `DATABASE_URL` required) → Tasks 5 + 6. ✓
- Signed-append + key management (shared `build_entry`, `~/.tokentrimmer/audit-signing-key` 0600 generate-on-first-use, preamble, print verifying key) → Tasks 3 + 4. ✓
- Column→struct mapping incl. `route_id → matched_route_id`, `task_class` default, enrichment fields `None` → Task 1. ✓
- Non-goals respected: no cloud endpoint, no replay/projection change, no `routes` schema change, default-off (`--from-db`/`--apply` opt-in). ✓
- Docs (closed loop) → Task 7. ✓

**Type consistency:** `build_plan_input_json_inner` (Task 2) / `fetch_window → (Uuid, Vec<RequestLog>)` (Task 1) / `build_entry` (Task 3, used by Task 4) / `append_entry` (Task 4, used by Task 5) / `apply_routes` + `plan_routes_to_apply` (Task 5, called by Task 6) — signatures line up across tasks. The plan-core↔routing route-type bridge is serde round-trip (Task 5).

**Placeholder scan:** none — every code step shows complete code; DB tests are gated and self-skip; the two "confirm the exact re-export path / PlanResult shape" notes are verification guards, not deferred work.
