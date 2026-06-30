//! Cost-control backend seam for the MCP cost tools
//! (`get_spend_today`, `check_budget_remaining`, `set_cost_limit`).
//!
//! ## Backends
//!
//! The cost tools are defined against the [`CostControlBackend`] trait so the
//! data source is pluggable:
//!
//! * [`HttpCostBackend`] (hosted default) — reads spend from the gateway's
//!   tenant-authed `GET /v1/spend` (org derived from the bound `tt_live_*` key,
//!   so a caller can only see their own org). The READ tools are fully wired.
//!   The WRITE tool (`set_cost_limit`) reports `applied: false` until the
//!   cloud-owned cap-write endpoint ships: `org_budget_caps.monthly_cap_usd` /
//!   `api_key_budget_caps` are owned by the cloud schema, so a tenant-authed cap
//!   *write* belongs on a cloud surface (a follow-up).
//! * [`UnconfiguredBackend`] (open-source default) — never fabricates numbers:
//!   reads return `configured: false` with placeholder zeros so a caller can
//!   tell "you have $0 of spend" from "no backend is wired".
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
use reqwest::StatusCode;
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

/// Hosted backend: reads the org's spend from the gateway's tenant-authed
/// `GET /v1/spend` endpoint, presenting the bound `tt_live_*` key. The gateway
/// re-derives the org from the key, so the figures are always the key's own
/// org's — a caller cannot read another org's spend.
///
/// The WRITE half (`set_cost_limit`) is **not yet wired**: the cap tables
/// (`org_budget_caps` / `api_key_budget_caps`) are owned by the cloud schema, so
/// a tenant-authed cap write belongs on a cloud surface (a follow-up). Until
/// then `set_cost_limit` returns `applied: false`, honestly signalling that
/// nothing was persisted (same contract as [`UnconfiguredBackend`] for writes).
pub struct HttpCostBackend {
    /// Gateway base URL, e.g. `https://api.tokentrimmer.com`. GETs `{base}/v1/spend`.
    pub gateway_base: String,
    /// The bound operator key, forwarded as the bearer token. The gateway
    /// derives the org from it.
    pub api_key: String,
    /// Shared outbound HTTP client.
    pub http: reqwest::Client,
}

/// Decoded `GET /v1/spend` body. Mirrors `tt_core::spend::SpendSummary` but is
/// kept local so this crate does not depend on `tt-core`. The wire `configured`
/// field (a cap is set) is intentionally not read — the backend-level
/// `configured` ("a real backend is wired") is always `true` here, and cap
/// presence is conveyed by `monthly_cap_usd` being `Some`/`None`.
#[derive(Debug, Deserialize)]
struct SpendApiResponse {
    spent_today_usd: f64,
    spend_mtd_usd: f64,
    monthly_cap_usd: Option<f64>,
    remaining_usd: Option<f64>,
}

/// Map a non-2xx `GET /v1/spend` status to an [`McpError`].
fn map_spend_error(status: StatusCode) -> McpError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            McpError::Unauthorized(format!("gateway rejected the key ({status})"))
        }
        StatusCode::SERVICE_UNAVAILABLE => {
            McpError::Internal("gateway spend reporting is not configured".into())
        }
        _ => McpError::Internal(format!("gateway spend error ({status})")),
    }
}

impl HttpCostBackend {
    async fn fetch_spend(&self) -> Result<SpendApiResponse, McpError> {
        let resp = self
            .http
            .get(format!(
                "{}/v1/spend",
                self.gateway_base.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| McpError::Internal(format!("gateway spend request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(map_spend_error(status));
        }
        resp.json::<SpendApiResponse>()
            .await
            .map_err(|e| McpError::Internal(format!("decode gateway spend response: {e}")))
    }
}

#[async_trait]
impl CostControlBackend for HttpCostBackend {
    async fn spend_today(&self, org_id: Uuid) -> Result<SpendToday, McpError> {
        let s = self.fetch_spend().await?;
        Ok(SpendToday {
            org_id,
            spend_usd: s.spent_today_usd,
            configured: true,
        })
    }

    async fn budget_remaining(&self, org_id: Uuid) -> Result<BudgetRemaining, McpError> {
        let s = self.fetch_spend().await?;
        Ok(BudgetRemaining {
            org_id,
            monthly_cap_usd: s.monthly_cap_usd,
            spend_mtd_usd: s.spend_mtd_usd,
            remaining_usd: s.remaining_usd,
            configured: true,
        })
    }

    async fn set_cost_limit(
        &self,
        org_id: Uuid,
        scope: LimitScope,
        monthly_cap_usd: Option<f64>,
    ) -> Result<CostLimitSet, McpError> {
        // WRITE half not yet wired — the cloud-owned cap tables need a
        // tenant-authed write endpoint (a follow-up). Report honestly that the
        // change was not persisted, exactly like the unconfigured default.
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

    // ── HttpCostBackend: reads the gateway's GET /v1/spend ───────────────────
    use httpmock::prelude::*;
    use serde_json::json;

    fn http_backend(base: String) -> HttpCostBackend {
        HttpCostBackend {
            gateway_base: base,
            api_key: "tt_live_operator".into(),
            http: reqwest::Client::new(),
        }
    }

    #[tokio::test]
    async fn http_spend_today_parses_gateway_response() {
        let server = MockServer::start_async().await;
        let org = Uuid::now_v7();
        let m = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/v1/spend")
                    .header("authorization", "Bearer tt_live_operator");
                then.status(200).json_body(json!({
                    "org_id": org.to_string(),
                    "spent_today_usd": 2.5,
                    "spend_mtd_usd": 11.0,
                    "monthly_cap_usd": 50.0,
                    "remaining_usd": 39.0,
                    "configured": true
                }));
            })
            .await;
        let s = http_backend(server.base_url())
            .spend_today(org)
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(s.org_id, org);
        assert!((s.spend_usd - 2.5).abs() < 1e-12);
        assert!(s.configured, "a wired HTTP backend is configured");
    }

    #[tokio::test]
    async fn http_budget_remaining_parses_cap_and_remaining() {
        let server = MockServer::start_async().await;
        let org = Uuid::now_v7();
        server
            .mock_async(|when, then| {
                when.method(GET).path("/v1/spend");
                then.status(200).json_body(json!({
                    "org_id": org.to_string(),
                    "spent_today_usd": 0.0,
                    "spend_mtd_usd": 11.0,
                    "monthly_cap_usd": 50.0,
                    "remaining_usd": 39.0,
                    "configured": true
                }));
            })
            .await;
        let b = http_backend(server.base_url())
            .budget_remaining(org)
            .await
            .unwrap();
        assert_eq!(b.monthly_cap_usd, Some(50.0));
        assert!((b.spend_mtd_usd - 11.0).abs() < 1e-12);
        assert!((b.remaining_usd.unwrap() - 39.0).abs() < 1e-12);
        assert!(b.configured);
    }

    #[tokio::test]
    async fn http_uncapped_org_has_none_cap_but_is_configured() {
        let server = MockServer::start_async().await;
        let org = Uuid::now_v7();
        server
            .mock_async(|when, then| {
                when.method(GET).path("/v1/spend");
                then.status(200).json_body(json!({
                    "org_id": org.to_string(),
                    "spent_today_usd": 1.0,
                    "spend_mtd_usd": 4.0,
                    "monthly_cap_usd": null,
                    "remaining_usd": null,
                    "configured": false
                }));
            })
            .await;
        let b = http_backend(server.base_url())
            .budget_remaining(org)
            .await
            .unwrap();
        // No cap set, but the backend IS wired → configured stays true; the
        // uncapped state is conveyed by monthly_cap_usd: None.
        assert_eq!(b.monthly_cap_usd, None);
        assert_eq!(b.remaining_usd, None);
        assert!(b.configured);
    }

    #[tokio::test]
    async fn http_maps_401_to_unauthorized() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/v1/spend");
                then.status(401);
            })
            .await;
        let err = http_backend(server.base_url())
            .spend_today(Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn http_set_cost_limit_reports_not_applied() {
        // The WRITE half is a cloud follow-up; the HTTP backend must not claim
        // persistence (no network call is even made).
        let org = Uuid::now_v7();
        let r = http_backend("http://gateway.invalid".into())
            .set_cost_limit(org, LimitScope::Org, Some(25.0))
            .await
            .unwrap();
        assert!(!r.applied, "write half not wired → applied must be false");
        assert_eq!(r.monthly_cap_usd, Some(25.0));
    }
}
