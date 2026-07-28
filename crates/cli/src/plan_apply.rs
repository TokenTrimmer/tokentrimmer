//! Local `tt plan --apply`: write projected routes as disabled drafts to the
//! gateway's Postgres `routes` table and emit a signed `plan.applied` audit
//! row.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::Context;
use ed25519_dalek::SigningKey;
use sqlx::PgPool;
use uuid::Uuid;

use tt_plan_core::types::{PlanResult, ProposedRoute};
use tt_routing::{
    canonicalize_route_value, validate_route_has_effect, NewRoute, PostgresRoutingStore,
    RoutingStore,
};

use crate::local_audit;

/// Outcome of planning which routes to create.
#[derive(Debug)]
pub struct ApplyPlan {
    pub to_create: Vec<NewRoute>,
    pub skipped_noop: Vec<String>,
    pub skipped_existing: Vec<String>,
}

/// Convert proposed routes (plan-core types) into canonical, disabled-draft
/// `tt_routing::NewRoute` specs, dropping no-ops and names that already exist.
/// Pure — no DB, no IO.
///
/// `tt_plan_core::types::{RouteConditions, RouteAction}` and the
/// `tt_routing::{RouteConditions, RouteAction}` are MIRRORED but DISTINCT types
/// with the same JSON shape (the HTTP `POST /v1/routes` path round-trips between
/// them). We bridge plan-core → routing via serde so the apply path stays in
/// lockstep with the wire format. `validate_route_has_effect` takes a
/// `&tt_routing::RouteAction`, so we validate AFTER converting.
pub fn plan_routes_to_apply(
    existing_names: &HashSet<String>,
    proposed: &[ProposedRoute],
) -> anyhow::Result<ApplyPlan> {
    let mut to_create = Vec::new();
    let mut skipped_noop = Vec::new();
    let mut skipped_existing = Vec::new();

    for r in proposed {
        // A local plan apply writes directly to Postgres, without the dashboard
        // control plane's server-bound catalog intent or live owner/admin
        // re-check. Keep the catalog namespace out of this generic writer;
        // use the authenticated dashboard catalog enable/repair flow instead.
        if tt_routing::catalog::is_catalog_route_name(&r.name) {
            anyhow::bail!(
                "proposed catalog route '{}' is reserved for the dashboard catalog enable/repair flow with a fresh owner/admin confirmation",
                r.name
            );
        }
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

        // Plan proposals are advisory. This direct Postgres writer has no
        // control-plane activation confirmation, so it must never carry an
        // enabled proposal into a live route. Materialize every effective new
        // proposal as a disabled draft; the normal Dashboard/control-plane
        // activation flow performs the separate, freshly confirmed enable.
        let spec = NewRoute {
            name: r.name.clone(),
            priority: r.priority,
            enabled: false,
            when,
            then,
        };
        // Plan writeback is a direct Postgres writer, so it cannot rely on
        // the gateway HTTP handler to enforce the versioned route contract.
        // Preserve the deliberate no-op skip above, then fail closed on every
        // other definition the canonical gateway contract would reject.
        let canonical = canonicalize_route_value(
            serde_json::to_value(&spec).context("encode route for canonical validation")?,
        )
        .map_err(|issues| {
            let details = issues
                .iter()
                .map(|issue| format!("{}: {}", issue.field, issue.message))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::anyhow!(
                "proposed route '{}' fails canonical route validation: {details}",
                r.name
            )
        })?;
        to_create.push(canonical.route);
    }

    Ok(ApplyPlan {
        to_create,
        skipped_noop,
        skipped_existing,
    })
}

/// Apply the proposed routes for `org_id`: validate + dedup, confirm,
/// idempotently create disabled drafts, then emit a signed `plan.applied` audit
/// entry to `chain_path`. `signing_key` + `chain_path` are injected for
/// testability.
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
        "about to create {} disabled draft route(s) for org {org_id}:",
        plan.to_create.len()
    ));
    for r in &plan.to_create {
        let tgt = r.then.target_model.as_deref().unwrap_or("(modifier-only)");
        crate::ui::note(&format!("  + {} → {} (disabled draft)", r.name, tgt));
    }

    if !assume_yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "refusing to apply without confirmation on a non-interactive stdin — pass --yes to proceed"
            );
        }
        print!(
            "Create these {} disabled draft route(s)? [y/N] ",
            plan.to_create.len()
        );
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
        "created {} disabled draft route(s); they are not live. Activate them through the normal control-plane/Dashboard route activation flow after fresh confirmation.",
        created.len()
    ));

    let payload = serde_json::json!({
        "plan_id": plan_id.to_string(),
        "applied_routes": created,
        "projected_savings_usd": result.aggregates.projected_savings_usd,
        "projected_savings_pct": result.aggregates.projected_savings_pct,
    });
    let verifying_hex =
        local_audit::append_entry(chain_path, signing_key, org_id, "plan.applied", payload)
            .with_context(|| {
                format!(
            "{} route(s) were created as disabled drafts, but recording the plan.applied proof failed",
            created.len()
        )
            })?;
    crate::ui::ok(&format!(
        "recorded plan.applied to {} — verify with: tt audit verify --key-hex {}",
        chain_path.display(),
        verifying_hex
    ));

    Ok(())
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
                ..Default::default()
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
        assert!(
            !plan.to_create[0].enabled,
            "effective proposals must materialize as disabled drafts"
        );
    }

    #[test]
    fn enabled_and_disabled_proposals_both_materialize_as_disabled_drafts() {
        let enabled = proposed("enabled-proposal", Some("gpt-4o-mini"));
        let mut disabled = proposed("disabled-proposal", Some("gpt-4o-mini"));
        disabled.enabled = false;

        let plan = plan_routes_to_apply(&HashSet::new(), &[enabled, disabled]).expect("ok");

        assert_eq!(
            plan.to_create
                .iter()
                .map(|route| route.name.as_str())
                .collect::<Vec<_>>(),
            vec!["enabled-proposal", "disabled-proposal"]
        );
        assert!(
            plan.to_create.iter().all(|route| !route.enabled),
            "the local writer must create only disabled drafts regardless of proposal state"
        );
    }

    #[test]
    fn skips_noop_route() {
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

    #[test]
    fn rejects_effective_route_that_fails_the_canonical_contract() {
        let err = plan_routes_to_apply(&HashSet::new(), &[proposed("blank-target", Some("   "))])
            .expect_err("an effect check alone must not let an invalid route reach Postgres");

        let message = err.to_string();
        assert!(message.contains("blank-target"));
        assert!(message.contains("then.target_model"));
        assert!(message.contains("non-whitespace"));
    }

    #[test]
    fn rejects_catalog_managed_route_before_direct_postgres_write() {
        let error = plan_routes_to_apply(
            &HashSet::new(),
            &[proposed("catalog:openai->gpt-4o-mini", Some("gpt-4o-mini"))],
        )
        .expect_err("the direct plan writer cannot bypass catalog confirmation");

        assert!(error
            .to_string()
            .contains("dashboard catalog enable/repair flow"));
    }

    // --- DB-gated integration tests ---

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
        // `CREATE TABLE IF NOT EXISTS` is not race-safe in Postgres; serialize
        // the bootstrap behind a transaction-scoped advisory lock (mirrors
        // crates/core/tests/route_pauses_pg.rs).
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
        serde_json::from_value(serde_json::json!({
            "plan_id": Uuid::nil(),
            "org_id": Uuid::nil(),
            "window_start": "2026-01-01T00:00:00Z",
            "window_end": "2026-01-02T00:00:00Z",
            "sample_size": 0,
            "aggregates": {
                "total_baseline_cost_usd": 0.0,
                "total_projected_cost_usd": 0.0,
                "projected_savings_usd": 0.0,
                "projected_savings_pct": 0.0,
                "cache_hit_rate_projected": 0.0,
                "p50_latency_ms_projected": 0.0,
                "p95_latency_ms_projected": 0.0,
                "requests_rerouted": 0,
                "requests_unchanged": 0,
                "requests_unprice_able": 0
            },
            "confidence_intervals": {
                "savings_usd_95": [0.0, 0.0],
                "savings_pct_95": [0.0, 0.0],
                "cache_hit_rate_95": [0.0, 0.0]
            },
            "per_route_breakdown": [],
            "caveats": []
        }))
        .expect("PlanResult shape — adjust to match the real struct if this fails")
    }

    // Parse the JSONL chain inline (local_audit::read_entries is private).
    fn read_chain(path: &Path) -> Vec<tt_telemetry::audit::AuditEntry> {
        let content = std::fs::read_to_string(path).expect("read chain");
        content
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                if t.is_empty() {
                    return None;
                }
                let v: serde_json::Value = serde_json::from_str(t).expect("json");
                if v.get("meta").and_then(|m| m.as_bool()) == Some(true) {
                    return None;
                }
                Some(serde_json::from_value::<tt_telemetry::audit::AuditEntry>(v).expect("entry"))
            })
            .collect()
    }

    #[tokio::test]
    async fn apply_creates_routes_idempotently_and_records_proof() {
        let Some(pool) = db_with_routes().await else {
            return;
        };
        let org = Uuid::new_v4();
        let key = tt_telemetry::audit::generate_signing_key();
        let dir = std::env::temp_dir().join(format!("tt-apply-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let chain = dir.join("AUDIT-CHAIN.jsonl");

        let routes = vec![proposed("swap-mini-itest", Some("gpt-4o-mini"))];
        let result = empty_result();

        apply_routes(
            &pool,
            org,
            Uuid::new_v4(),
            &routes,
            &result,
            true,
            &key,
            &chain,
        )
        .await
        .expect("first apply");
        let after_first = PostgresRoutingStore::new(pool.clone())
            .list_all_for_org(org)
            .await
            .expect("list");
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].name, "swap-mini-itest");
        assert!(
            !after_first[0].enabled,
            "local plan apply must persist an enabled proposal as a disabled draft"
        );

        apply_routes(
            &pool,
            org,
            Uuid::new_v4(),
            &routes,
            &result,
            true,
            &key,
            &chain,
        )
        .await
        .expect("second apply");
        let after_second = PostgresRoutingStore::new(pool.clone())
            .list_all_for_org(org)
            .await
            .expect("list");
        assert_eq!(after_second.len(), 1, "no duplicate route created");

        let entries = read_chain(&chain);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, "plan.applied");
        tt_telemetry::audit::verify_chain(&entries, &key.verifying_key()).expect("verifies");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn apply_rejects_nil_org() {
        let Some(pool) = db_with_routes().await else {
            return;
        };
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
}
