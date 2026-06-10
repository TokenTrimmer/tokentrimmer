//! Cost-control backend seam for the MCP cost tools
//! (`get_spend_today`, `check_budget_remaining`, `set_cost_limit`).
//!
//! ## Why a trait seam (and what is *not* wired here)
//!
//! The spend/budget data an agent stack wants to read and adjust lives in the
//! **cloud** control plane, not in this public crate. Concretely (cloud repo,
//! `crates/api`): month-to-date spend is `SUM(request_logs.cost_usd)`, the org
//! cap is `org_budget_caps.monthly_cap_usd`, and per-key caps are
//! `api_key_budget_caps` (read/written by `key_budget_caps::{get,set}_key_budget_cap`).
//! Those are **database helpers**, and the cloud `tt-api` exposes **no
//! per-org-API-key-authenticated HTTP endpoint** for them today — every
//! `/v1/admin/*` route is gated on the operator `TT_ADMIN_TOKEN`, not on a
//! tenant's `tt_live_*` key. So the data is **not reachable** from this public
//! MCP server with the org's own key.
//!
//! Rather than fabricate plausible numbers, the cost tools are defined against
//! this [`CostControlBackend`] trait. The hosted side can plug in a real
//! implementation (DB-backed, or an org-scoped HTTP endpoint once one exists);
//! the public-repo default is [`UnconfiguredBackend`], which returns a clearly
//! marked `"unconfigured"` response so a caller can tell the difference between
//! "you have $0 of spend" and "no backend is wired".
//!
//! ## Auth scoping
//!
//! Every operation takes the **bound** `org_id` — the org resolved from the
//! operator's verified key at server boot (design §8: one key ⇒ one org for the
//! process lifetime). The tools are constructed with that bound org and pass it
//! to the backend; a caller therefore **cannot** read or mutate cost state for a
//! *different* org by putting another `org_id` in the tool arguments. The
//! `set_cost_limit` tool rejects any caller-supplied `org_id` that does not
//! match the bound org with [`McpError::Unauthorized`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::McpError;

/// Current-day spend for an org, in USD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendToday {
    /// The org this figure is for (the bound org — never a caller-chosen one).
    pub org_id: Uuid,
    /// Spend so far for the current UTC day, in USD.
    pub spend_usd: f64,
    /// `true` when the figure comes from a real, wired backend; `false` when no
    /// backend is configured (the public-repo default). When `false`,
    /// `spend_usd` is `0.0` as a placeholder and MUST NOT be treated as real.
    pub configured: bool,
}

/// Remaining budget / headroom for an org against its monthly cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetRemaining {
    /// The org this figure is for (the bound org).
    pub org_id: Uuid,
    /// The org's effective monthly cap in USD, if one is set. `None` = uncapped.
    pub monthly_cap_usd: Option<f64>,
    /// Month-to-date spend in USD.
    pub spend_mtd_usd: f64,
    /// Headroom under the cap (`cap - spend`, floored at 0). `None` when
    /// uncapped (no finite headroom to report).
    pub remaining_usd: Option<f64>,
    /// `true` when from a real wired backend; `false` for the unconfigured
    /// default (in which case the numeric fields are placeholders).
    pub configured: bool,
}

/// Outcome of setting/adjusting a cost limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostLimitSet {
    /// The org the limit was set for (always the bound org).
    pub org_id: Uuid,
    /// The key the limit was scoped to, when a per-key limit was requested.
    /// `None` = an org-level limit.
    pub key_id: Option<Uuid>,
    /// The monthly cap in USD that was applied (echo of the requested value).
    pub monthly_cap_usd: Option<f64>,
    /// `true` when a real backend applied the change; `false` for the
    /// unconfigured default (the change was *not* persisted anywhere).
    pub applied: bool,
}

/// The scope a cost limit applies to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LimitScope {
    /// An org-wide monthly cap.
    Org,
    /// A per-key monthly cap.
    Key(Uuid),
}

/// Cost-control data source for the MCP cost tools. Every method is **org
/// scoped**: callers pass the bound `org_id` and the implementation must only
/// ever touch that org's data.
///
/// Implementations live on the hosted side (DB- or HTTP-backed); the
/// public-repo default is [`UnconfiguredBackend`].
#[async_trait]
pub trait CostControlBackend: Send + Sync {
    /// Current-day spend for `org_id`, in USD.
    async fn spend_today(&self, org_id: Uuid) -> Result<SpendToday, McpError>;

    /// Remaining budget headroom for `org_id` against its monthly cap.
    async fn budget_remaining(&self, org_id: Uuid) -> Result<BudgetRemaining, McpError>;

    /// Set/adjust a monthly cost limit (org-wide or per-key) for `org_id`.
    /// A `monthly_cap_usd` of `None` clears the cap on that scope.
    async fn set_cost_limit(
        &self,
        org_id: Uuid,
        scope: LimitScope,
        monthly_cap_usd: Option<f64>,
    ) -> Result<CostLimitSet, McpError>;
}

/// The public-repo default backend: no spend/budget data source is wired.
///
/// It never fabricates spend or budget figures. Reads return `configured:
/// false` with placeholder zeros; `set_cost_limit` returns `applied: false`
/// (the change is not persisted). This lets a hosted deployment ship the tool
/// *definitions* and swap in a real [`CostControlBackend`] without changing the
/// tool surface, while keeping the open-source build honest about having no
/// data behind it.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnconfiguredBackend;

#[async_trait]
impl CostControlBackend for UnconfiguredBackend {
    async fn spend_today(&self, org_id: Uuid) -> Result<SpendToday, McpError> {
        Ok(SpendToday {
            org_id,
            spend_usd: 0.0,
            configured: false,
        })
    }

    async fn budget_remaining(&self, org_id: Uuid) -> Result<BudgetRemaining, McpError> {
        Ok(BudgetRemaining {
            org_id,
            monthly_cap_usd: None,
            spend_mtd_usd: 0.0,
            remaining_usd: None,
            configured: false,
        })
    }

    async fn set_cost_limit(
        &self,
        org_id: Uuid,
        scope: LimitScope,
        monthly_cap_usd: Option<f64>,
    ) -> Result<CostLimitSet, McpError> {
        let key_id = match scope {
            LimitScope::Org => None,
            LimitScope::Key(k) => Some(k),
        };
        Ok(CostLimitSet {
            org_id,
            key_id,
            monthly_cap_usd,
            applied: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unconfigured_spend_is_marked_not_fabricated() {
        let org = Uuid::now_v7();
        let s = UnconfiguredBackend.spend_today(org).await.unwrap();
        assert_eq!(s.org_id, org);
        assert!(!s.configured, "must signal no backend, not a real $0");
        assert_eq!(s.spend_usd, 0.0);
    }

    #[tokio::test]
    async fn unconfigured_budget_is_marked_not_fabricated() {
        let org = Uuid::now_v7();
        let b = UnconfiguredBackend.budget_remaining(org).await.unwrap();
        assert_eq!(b.org_id, org);
        assert!(!b.configured);
        assert_eq!(b.monthly_cap_usd, None);
        assert_eq!(b.remaining_usd, None);
    }

    #[tokio::test]
    async fn unconfigured_set_limit_is_not_applied() {
        let org = Uuid::now_v7();
        let key = Uuid::now_v7();
        let r = UnconfiguredBackend
            .set_cost_limit(org, LimitScope::Key(key), Some(25.0))
            .await
            .unwrap();
        assert_eq!(r.org_id, org);
        assert_eq!(r.key_id, Some(key));
        assert_eq!(r.monthly_cap_usd, Some(25.0));
        assert!(!r.applied, "unconfigured backend must not claim to persist");
    }

    #[tokio::test]
    async fn unconfigured_set_org_limit_has_no_key() {
        let org = Uuid::now_v7();
        let r = UnconfiguredBackend
            .set_cost_limit(org, LimitScope::Org, Some(100.0))
            .await
            .unwrap();
        assert_eq!(r.key_id, None);
    }
}
