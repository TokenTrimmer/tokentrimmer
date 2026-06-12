//! Plan apply path — mark a Plan as applied + emit a `plan.applied` audit row.
//!
//! Mirrors the [`tt_auth::revoke_key`] pattern: a small free function that
//! couples a store mutation with an audit emission so callers can't acquire
//! "apply" semantics without leaving a tamper-evident chain entry.
//!
//! The store is fronted by a trait so this library doesn't drag in sqlx —
//! the hosted cloud worker provides a `PostgresPlanStore`; tests use
//! [`InMemoryPlanStore`] from this module.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tt_telemetry::audit::{Actor, AuditError, AuditWriter};
use uuid::Uuid;

use crate::types::{PlanResult, ProposedRoute};

/// Errors returned by the plan-apply path.
#[derive(Debug, Error)]
pub enum ApplyError {
    /// The plan row was not found in the store. Either the id is wrong or
    /// the plan has been purged.
    #[error("plan not found")]
    NotFound,
    /// The plan was already in a terminal state (applied / reverted /
    /// failed) — apply is idempotent only from `projected`.
    #[error("plan is in terminal state '{state}', cannot re-apply")]
    InvalidState {
        /// The status the store reported.
        state: String,
    },
    /// Store-side failure. The mutation may or may not have committed —
    /// callers should treat as "needs investigation" rather than retrying
    /// blindly.
    #[error("store: {0}")]
    Store(String),
    /// Audit row failed to write AFTER the store mutation committed. The
    /// plan IS applied; the chain entry was not. Caller should re-attempt
    /// the audit emission out-of-band rather than re-applying.
    #[error("audit: {0}")]
    Audit(#[from] AuditError),
}

/// Persistence contract for plan_runs rows. Implementations: [`InMemoryPlanStore`]
/// (this file, for tests), `PostgresPlanStore` (lands in the cloud worker
/// crate when the sqlx-pool wiring is done).
#[async_trait]
pub trait PlanStore: Send + Sync {
    /// Atomically (1) transition `plan_id` from status `'projected'` to
    /// `'applied'` and stamp `applied_at`, AND (2) persist `routes` to the
    /// Gateway routing config the live gateway reads. Return the previous
    /// status when the row exists, or `None` when no such row.
    ///
    /// MUST be atomic across BOTH effects: the status flip and the route
    /// write commit together or not at all. A partial update — status
    /// flipped but routes not written, applied_at stamped but status
    /// mismatched — violates the audit promise, because the `plan.applied`
    /// chain entry asserts that the routes are now live. Implementations
    /// MUST use a single transaction (Postgres: `BEGIN; UPDATE plan_runs;
    /// INSERT INTO routes; COMMIT`).
    ///
    /// The store MUST only write `routes` when the transition actually
    /// happens (previous status was `'projected'`). When the row is already
    /// in a terminal state, return the previous status and write nothing —
    /// the free function [`apply_plan`] turns that into
    /// [`ApplyError::InvalidState`] and emits no audit row.
    ///
    /// `routes` is the caller's authored route set (see
    /// [`PlanResult::proposed_routes`]); it may be empty, in which case the
    /// status flips and no routes are written (the legacy no-op shape, for
    /// plan rows persisted before routes were carried on the result).
    async fn mark_applied(
        &self,
        plan_id: Uuid,
        applied_at: DateTime<Utc>,
        routes: &[ProposedRoute],
    ) -> Result<Option<String>, ApplyError>;
}

/// In-memory store for tests. Tracks status + applied routes per `plan_id`.
#[derive(Default)]
pub struct InMemoryPlanStore {
    rows: Arc<Mutex<HashMap<Uuid, InMemoryRow>>>,
    /// When set, [`PlanStore::mark_applied`] returns
    /// [`ApplyError::Store`] *before* mutating anything — used by tests to
    /// prove that a route-write failure leaves the row untouched AND emits
    /// no audit row. Mirrors a Postgres `INSERT INTO routes` failing inside
    /// the transaction (which rolls the whole txn back).
    fail_route_write: bool,
}

#[derive(Debug, Clone)]
struct InMemoryRow {
    status: String,
    applied_at: Option<DateTime<Utc>>,
    /// Routes written when the row transitioned to `applied`. Empty until
    /// (and unless) a successful apply with a non-empty route set.
    applied_routes: Vec<ProposedRoute>,
}

impl InMemoryPlanStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a store whose [`PlanStore::mark_applied`] always fails the
    /// route write (and therefore the whole atomic operation). For tests
    /// that assert no audit row is emitted when persistence fails.
    pub fn with_failing_route_write() -> Self {
        Self {
            fail_route_write: true,
            ..Self::default()
        }
    }

    /// Seed a row in `status='projected'`. Returns the id for callers to
    /// pass to [`apply_plan`].
    pub fn seed_projected(&self) -> Uuid {
        let id = Uuid::now_v7();
        let mut g = self.rows.lock().expect("rows lock");
        g.insert(
            id,
            InMemoryRow {
                status: "projected".into(),
                applied_at: None,
                applied_routes: Vec::new(),
            },
        );
        id
    }

    /// Read-only status lookup, used by tests to assert state transitions.
    pub fn status(&self, plan_id: Uuid) -> Option<String> {
        let g = self.rows.lock().expect("rows lock");
        g.get(&plan_id).map(|r| r.status.clone())
    }

    /// Read-only applied-at lookup.
    pub fn applied_at(&self, plan_id: Uuid) -> Option<DateTime<Utc>> {
        let g = self.rows.lock().expect("rows lock");
        g.get(&plan_id).and_then(|r| r.applied_at)
    }

    /// Read-only lookup of the routes persisted by a successful apply. Used
    /// by tests to assert the proposed routes actually landed in the store
    /// (the heart of the rv-plan-apply-writes-routes fix).
    pub fn applied_routes(&self, plan_id: Uuid) -> Option<Vec<ProposedRoute>> {
        let g = self.rows.lock().expect("rows lock");
        g.get(&plan_id).map(|r| r.applied_routes.clone())
    }
}

#[async_trait]
impl PlanStore for InMemoryPlanStore {
    async fn mark_applied(
        &self,
        plan_id: Uuid,
        applied_at: DateTime<Utc>,
        routes: &[ProposedRoute],
    ) -> Result<Option<String>, ApplyError> {
        // Simulate the route-write leg of the transaction failing. We fail
        // BEFORE touching any row state so the post-condition matches a real
        // txn rollback: nothing committed, so no audit row is emitted by the
        // caller.
        if self.fail_route_write {
            return Err(ApplyError::Store("simulated route-write failure".into()));
        }

        let mut g = self
            .rows
            .lock()
            .map_err(|e| ApplyError::Store(e.to_string()))?;
        let Some(row) = g.get_mut(&plan_id) else {
            return Ok(None);
        };
        let prev = row.status.clone();
        // Both effects happen together, and only on the projected→applied
        // transition. Holding the single `rows` mutex for the whole block is
        // this store's stand-in for the Postgres transaction the contract
        // requires.
        if prev == "projected" {
            row.status = "applied".into();
            row.applied_at = Some(applied_at);
            row.applied_routes = routes.to_vec();
        }
        Ok(Some(prev))
    }
}

/// Apply audit payload — just the public-safe fields. NEVER includes the
/// full proposed config diff (which can contain customer-specific routing
/// patterns); that's already on the plan_runs row for join-time retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplyPayload {
    plan_id: Uuid,
    applied_at: String,
    sample_size: u32,
    projected_savings_usd: f64,
}

/// Mark a Plan as applied, persist its proposed routes, and emit a
/// `plan.applied` audit row.
///
/// Two-step:
///   1. `store.mark_applied(plan_id, now, routes)` — atomic state transition
///      AND route persistence (both commit together; see [`PlanStore`]).
///   2. `audit_writer.write(plan.applied, payload)` — tamper-evident record,
///      emitted only AFTER step 1 succeeds, so the chain never asserts an
///      apply that didn't actually write its routes.
///
/// The routes come from [`PlanResult::proposed_routes`], which `replay()`
/// populates from the [`crate::types::PlanInput`]. Callers persisting the
/// result must keep that field populated; a result deserialized from a
/// legacy row (no routes) applies the status flip with an empty route set.
///
/// # Errors
///
/// - [`ApplyError::NotFound`] — no row matches `result.plan_id`.
/// - [`ApplyError::InvalidState`] — row exists but is not in `projected`
///   (already applied, reverted, or failed). No routes were written.
/// - [`ApplyError::Store`] — the atomic status-flip + route-write could not
///   complete; by contract nothing was committed, so no audit row is emitted.
/// - [`ApplyError::Audit`] — store succeeded but audit emission failed.
///   The plan IS applied (and routes ARE written) when this is returned;
///   out-of-band recovery is the caller's responsibility.
pub async fn apply_plan<S: PlanStore, A: AuditWriter>(
    store: &S,
    audit_writer: &A,
    result: &PlanResult,
    actor: Actor,
) -> Result<(), ApplyError> {
    let now = Utc::now();
    let prev_status = store
        .mark_applied(result.plan_id, now, &result.proposed_routes)
        .await?;
    match prev_status {
        None => return Err(ApplyError::NotFound),
        Some(s) if s != "projected" => {
            return Err(ApplyError::InvalidState { state: s });
        }
        _ => {}
    }

    let payload = ApplyPayload {
        plan_id: result.plan_id,
        applied_at: now.to_rfc3339(),
        sample_size: result.sample_size,
        projected_savings_usd: result.aggregates.projected_savings_usd,
    };
    let payload_value = serde_json::to_value(&payload)
        .map_err(|e| ApplyError::Store(format!("serialize payload: {e}")))?;

    audit_writer
        .write(
            result.org_id,
            actor,
            "plan.applied".to_string(),
            payload_value,
        )
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Aggregates, ConfidenceIntervals, PlanResult, RouteAction, RouteConditions};
    use chrono::Utc;
    use tt_telemetry::audit::{verify_chain, InMemoryAuditWriter};

    /// One non-trivial proposed route, so apply has something real to persist.
    fn sample_routes() -> Vec<ProposedRoute> {
        vec![ProposedRoute {
            id: Uuid::now_v7(),
            name: "haiku-for-cheap-classification".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["claude-sonnet-4-5".into()],
                input_tokens_lt: Some(2_000),
                input_tokens_gt: None,
                tag_equals: None,
                has_images: None,
                has_audio: None,
                prompt_contains_any_of: vec![],
                estimated_cost_gt: None,
                estimated_cost_lt: None,
                upstream_latency_ms_p95_gt: None,
            },
            then: RouteAction {
                format_switch: None,
                diff: false,
                target_model: "claude-haiku-4-5".into(),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
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
        }]
    }

    fn make_plan_result(plan_id: Uuid, org_id: Uuid) -> PlanResult {
        make_plan_result_with_routes(plan_id, org_id, sample_routes())
    }

    fn make_plan_result_with_routes(
        plan_id: Uuid,
        org_id: Uuid,
        proposed_routes: Vec<ProposedRoute>,
    ) -> PlanResult {
        PlanResult {
            plan_id,
            org_id,
            window_start: Utc::now(),
            window_end: Utc::now(),
            sample_size: 100,
            aggregates: Aggregates {
                total_baseline_cost_usd: 10.0,
                total_projected_cost_usd: 6.0,
                projected_savings_usd: 4.0,
                projected_savings_pct: 40.0,
                cache_hit_rate_projected: 0.0,
                p50_latency_ms_projected: 100.0,
                p95_latency_ms_projected: 250.0,
                requests_rerouted: 50,
                requests_unchanged: 50,
                requests_unprice_able: 0,
                l2_projections: Vec::new(),
                l2_poisoning_candidates: 0,
                l2_per_class: Vec::new(),
            },
            confidence_intervals: ConfidenceIntervals {
                savings_usd_95: (3.5, 4.5),
                savings_pct_95: (35.0, 45.0),
                cache_hit_rate_95: (0.0, 0.0),
                p50_latency_ms_95: (90.0, 110.0),
                p95_latency_ms_95: (200.0, 300.0),
            },
            per_route_breakdown: Vec::new(),
            caveats: Vec::new(),
            quality: None,
            proposed_routes,
        }
    }

    #[tokio::test]
    async fn apply_marks_row_applied_and_emits_audit() {
        let store = InMemoryPlanStore::new();
        let audit = InMemoryAuditWriter::new();
        let plan_id = store.seed_projected();
        let org_id = Uuid::now_v7();
        let result = make_plan_result(plan_id, org_id);

        apply_plan(&store, &audit, &result, Actor::System)
            .await
            .expect("apply ok");

        assert_eq!(store.status(plan_id).as_deref(), Some("applied"));
        assert!(store.applied_at(plan_id).is_some());

        // (a) The proposed routes were persisted alongside the status flip.
        let written = store.applied_routes(plan_id).expect("row exists");
        assert_eq!(written.len(), 1, "the one proposed route must be written");
        assert_eq!(written[0].then.target_model, "claude-haiku-4-5");
        assert_eq!(written[0].name, "haiku-for-cheap-classification");

        let entries = audit.list(org_id).await.expect("list ok");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, "plan.applied");
        assert!(entries[0]
            .payload
            .to_string()
            .contains(&plan_id.to_string()));

        // Chain integrity.
        let vk = audit.verifying_key();
        verify_chain(&entries, &vk).expect("chain verifies");
    }

    #[tokio::test]
    async fn apply_returns_not_found_for_unknown_plan() {
        let store = InMemoryPlanStore::new();
        let audit = InMemoryAuditWriter::new();
        let result = make_plan_result(Uuid::now_v7(), Uuid::now_v7());

        let err = apply_plan(&store, &audit, &result, Actor::System)
            .await
            .expect_err("unknown plan must fail");
        assert!(matches!(err, ApplyError::NotFound));

        // No audit row for a failed apply.
        let entries = audit.list(result.org_id).await.expect("list ok");
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn apply_twice_returns_invalid_state() {
        let store = InMemoryPlanStore::new();
        let audit = InMemoryAuditWriter::new();
        let plan_id = store.seed_projected();
        let org_id = Uuid::now_v7();
        let result = make_plan_result(plan_id, org_id);

        apply_plan(&store, &audit, &result, Actor::System)
            .await
            .expect("first apply ok");
        let err = apply_plan(&store, &audit, &result, Actor::System)
            .await
            .expect_err("re-apply must fail");
        match err {
            ApplyError::InvalidState { state } => assert_eq!(state, "applied"),
            other => panic!("expected InvalidState, got {other:?}"),
        }

        // Only one audit row — second attempt didn't emit.
        let entries = audit.list(org_id).await.expect("list ok");
        assert_eq!(entries.len(), 1);
    }

    /// (a) Applying a projected plan records BOTH `status='applied'` AND the
    /// exact proposed routes — the core rv-plan-apply-writes-routes contract.
    #[tokio::test]
    async fn apply_persists_proposed_routes_atomically_with_status() {
        let store = InMemoryPlanStore::new();
        let audit = InMemoryAuditWriter::new();
        let plan_id = store.seed_projected();
        let org_id = Uuid::now_v7();

        let routes = sample_routes();
        let route_id = routes[0].id;
        let result = make_plan_result_with_routes(plan_id, org_id, routes);

        apply_plan(&store, &audit, &result, Actor::System)
            .await
            .expect("apply ok");

        // Status flipped.
        assert_eq!(store.status(plan_id).as_deref(), Some("applied"));
        // Routes written, identity preserved.
        let written = store.applied_routes(plan_id).expect("row exists");
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].id, route_id);
        assert_eq!(written[0].priority, 100);
        assert_eq!(written[0].when.model_in, vec!["claude-sonnet-4-5"]);
        assert_eq!(written[0].then.target_model, "claude-haiku-4-5");

        // And the audit row was emitted (after both effects).
        let entries = audit.list(org_id).await.expect("list ok");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, "plan.applied");
    }

    /// (c) When the store fails the route write, the operation is rejected
    /// AND no audit row is emitted — the chain must never assert an apply
    /// whose routes didn't land.
    #[tokio::test]
    async fn route_write_failure_emits_no_audit_row() {
        // This store has a row in `projected` but fails the atomic write.
        let store = InMemoryPlanStore::with_failing_route_write();
        let audit = InMemoryAuditWriter::new();
        // Seed a row so the failure is the route write, not a missing row.
        let plan_id = store.seed_projected();
        let org_id = Uuid::now_v7();
        let result = make_plan_result(plan_id, org_id);

        let err = apply_plan(&store, &audit, &result, Actor::System)
            .await
            .expect_err("route-write failure must surface");
        assert!(matches!(err, ApplyError::Store(_)), "got {err:?}");

        // Row untouched: still projected, no routes, no applied_at.
        assert_eq!(store.status(plan_id).as_deref(), Some("projected"));
        assert!(store.applied_at(plan_id).is_none());
        assert_eq!(
            store.applied_routes(plan_id).expect("row exists").len(),
            0,
            "no routes may be written when the txn fails"
        );

        // No audit row.
        let entries = audit.list(org_id).await.expect("list ok");
        assert!(
            entries.is_empty(),
            "audit row must NOT be emitted on a failed route write"
        );
    }

    /// A legacy result (no carried routes) still applies — flips status and
    /// writes zero routes, matching the pre-fix no-op shape rather than
    /// erroring. Guards the `#[serde(default)]` round-trip path.
    #[tokio::test]
    async fn apply_with_empty_routes_flips_status_and_writes_none() {
        let store = InMemoryPlanStore::new();
        let audit = InMemoryAuditWriter::new();
        let plan_id = store.seed_projected();
        let org_id = Uuid::now_v7();
        let result = make_plan_result_with_routes(plan_id, org_id, Vec::new());

        apply_plan(&store, &audit, &result, Actor::System)
            .await
            .expect("apply ok even with no routes");

        assert_eq!(store.status(plan_id).as_deref(), Some("applied"));
        assert_eq!(store.applied_routes(plan_id).expect("row exists").len(), 0);
        let entries = audit.list(org_id).await.expect("list ok");
        assert_eq!(entries.len(), 1);
    }
}
