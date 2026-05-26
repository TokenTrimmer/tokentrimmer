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

use crate::types::PlanResult;

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
    /// Atomically transition `plan_id` from status `'projected'` to
    /// `'applied'` and stamp `applied_at`. Return the previous status when
    /// the row exists, or `None` when no such row.
    ///
    /// MUST be atomic: a partial update that leaves status mismatched with
    /// applied_at violates the audit promise.
    async fn mark_applied(
        &self,
        plan_id: Uuid,
        applied_at: DateTime<Utc>,
    ) -> Result<Option<String>, ApplyError>;
}

/// In-memory store for tests. Tracks status per `plan_id`.
#[derive(Default)]
pub struct InMemoryPlanStore {
    rows: Arc<Mutex<HashMap<Uuid, InMemoryRow>>>,
}

#[derive(Debug, Clone)]
struct InMemoryRow {
    status: String,
    applied_at: Option<DateTime<Utc>>,
}

impl InMemoryPlanStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
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
}

#[async_trait]
impl PlanStore for InMemoryPlanStore {
    async fn mark_applied(
        &self,
        plan_id: Uuid,
        applied_at: DateTime<Utc>,
    ) -> Result<Option<String>, ApplyError> {
        let mut g = self
            .rows
            .lock()
            .map_err(|e| ApplyError::Store(e.to_string()))?;
        let Some(row) = g.get_mut(&plan_id) else {
            return Ok(None);
        };
        let prev = row.status.clone();
        if prev == "projected" {
            row.status = "applied".into();
            row.applied_at = Some(applied_at);
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

/// Mark a Plan as applied and emit a `plan.applied` audit row.
///
/// Two-step:
///   1. `store.mark_applied(plan_id, now)` — atomic state transition.
///   2. `audit_writer.write(plan.applied, payload)` — tamper-evident record.
///
/// # Errors
///
/// - [`ApplyError::NotFound`] — no row matches `result.plan_id`.
/// - [`ApplyError::InvalidState`] — row exists but is not in `projected`
///   (already applied, reverted, or failed).
/// - [`ApplyError::Store`] — store update could not complete.
/// - [`ApplyError::Audit`] — store succeeded but audit emission failed.
///   The plan IS applied when this is returned; out-of-band recovery is
///   the caller's responsibility.
pub async fn apply_plan<S: PlanStore, A: AuditWriter>(
    store: &S,
    audit_writer: &A,
    result: &PlanResult,
    actor: Actor,
) -> Result<(), ApplyError> {
    let now = Utc::now();
    let prev_status = store.mark_applied(result.plan_id, now).await?;
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
    use crate::types::{Aggregates, ConfidenceIntervals, PlanResult};
    use chrono::Utc;
    use tt_telemetry::audit::{verify_chain, InMemoryAuditWriter};

    fn make_plan_result(plan_id: Uuid, org_id: Uuid) -> PlanResult {
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
}
