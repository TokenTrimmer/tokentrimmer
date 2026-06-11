//! `apply_plan` — a **write** MCP tool that applies a previously-simulated plan
//! by forwarding to the cloud's `POST /v1/admin/plans/:id/apply` endpoint, which
//! atomically flips the plan to `applied` and upserts its proposed routes into
//! the live `routes` table.
//!
//! Like [`crate::tools::add_route::AddRouteTool`], this tool is **off by
//! default** — registered only when the server is booted with write-tools
//! enabled. A read-only deployment never lists it, and `tools/call` on it
//! returns `MethodNotFound` (-32601), not `Unauthorized`.
//!
//! ## Auth scoping
//!
//! Constructed with the **bound** `org_id` (the operator key's org, verified at
//! boot). The bound org is sent to the cloud as the `org_id` query parameter —
//! the cloud scopes the apply to that org, so a plan id belonging to another
//! tenant reads as absent (404). A caller-supplied `org_id` argument, if
//! present, must equal the bound org or the call is rejected with `-32001`; the
//! caller can never apply a plan on behalf of a different org.
//!
//! ## No partial success
//!
//! The cloud applies the plan in a single transaction (status flip + route
//! upsert). On any cloud rollback (4xx/5xx) this tool returns an error rather
//! than a partial outcome. Idempotent: re-applying an already-applied plan
//! returns the existing row (the cloud reports it as a no-op).

use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

/// `apply_plan` — apply a simulated plan for the authenticated org.
pub struct ApplyPlanTool {
    /// Cloud API base URL. The tool POSTs to
    /// `{cloud_base}/v1/admin/plans/{plan_id}/apply?org_id={bound_org}`.
    pub cloud_base: String,
    /// The operator's bearer token, forwarded to the cloud.
    pub api_key: String,
    /// Outbound HTTP client (shared via the server state).
    pub http: reqwest::Client,
    /// The org bound from the operator's verified key. Sent as the cloud
    /// `org_id` query param; a caller-supplied `org_id` must match it.
    pub org_id: Uuid,
}

impl ApplyPlanTool {
    /// Extract the `plan_id` (required, must be a UUID) and validate any
    /// caller-supplied `org_id` against the bound org.
    fn parse(&self, params: &Value) -> Result<Uuid, McpError> {
        // Auth scoping: a caller-named org must be *their* org.
        if let Some(req_org) = params.get("org_id") {
            if !req_org.is_null() {
                let req_org = req_org
                    .as_str()
                    .ok_or_else(|| McpError::InvalidParams("org_id must be a string".into()))?;
                let req_org = Uuid::parse_str(req_org)
                    .map_err(|e| McpError::InvalidParams(format!("org_id not a UUID: {e}")))?;
                if req_org != self.org_id {
                    return Err(McpError::Unauthorized(
                        "cannot apply a plan for another organization".into(),
                    ));
                }
            }
        }

        let plan_id = params
            .get("plan_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::InvalidParams("missing plan_id".into()))?;
        Uuid::parse_str(plan_id)
            .map_err(|e| McpError::InvalidParams(format!("plan_id not a UUID: {e}")))
    }
}

#[async_trait]
impl Tool for ApplyPlanTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "apply_plan",
            description: "Apply a previously-simulated plan for YOUR authenticated \
                organization (write tool; available only when the server is \
                booted with write access). In one atomic cloud transaction the \
                plan is marked applied and its proposed routes are written live; \
                on any failure the whole apply rolls back (no partial success). \
                Always scoped to your own org — an `org_id` argument, if \
                supplied, must match your authenticated org. Re-applying an \
                already-applied plan is an idempotent no-op.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "UUID of the plan_run to apply (from simulate_plan)."
                    },
                    "org_id": {
                        "type": "string",
                        "description": "Optional; if present must equal your authenticated org. You cannot apply another org's plan."
                    }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let plan_id = self.parse(&params)?;

        let url = format!(
            "{}/v1/admin/plans/{}/apply",
            self.cloud_base.trim_end_matches('/'),
            plan_id
        );
        let resp = self
            .http
            .post(&url)
            // The bound org scopes the apply at the cloud — never a
            // caller-supplied value.
            .query(&[("org_id", self.org_id.to_string())])
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| McpError::Internal(format!("cloud request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(map_cloud_error(status, &body));
        }

        // Success: the cloud committed the transaction. Relay the plan row as
        // the apply outcome.
        let row: Value = resp
            .json()
            .await
            .map_err(|e| McpError::Internal(format!("decode cloud response: {e}")))?;
        Ok(json!({ "applied": true, "plan": row }))
    }
}

/// Map a non-2xx cloud response to the right [`McpError`]. 401/403 →
/// `Unauthorized`; 404 (plan absent / wrong org) and other 4xx → `InvalidParams`;
/// 5xx (rollback / server fault) → `Internal`.
fn map_cloud_error(status: StatusCode, body: &str) -> McpError {
    let trimmed = body.trim();
    let detail = if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    };
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            McpError::Unauthorized(format!("cloud rejected the operator key ({status})"))
        }
        s if s.is_client_error() => {
            McpError::InvalidParams(format!("cloud rejected the apply ({status}){detail}"))
        }
        s => McpError::Internal(format!("cloud rolled back the apply ({s}){detail}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn tool(base: String, org: Uuid) -> ApplyPlanTool {
        ApplyPlanTool {
            cloud_base: base,
            api_key: "tt_live_operator".into(),
            http: reqwest::Client::new(),
            org_id: org,
        }
    }

    // ── schema ───────────────────────────────────────────────────────────────

    #[test]
    fn def_requires_plan_id() {
        let d = tool("http://unused".into(), Uuid::now_v7()).def();
        assert_eq!(d.name, "apply_plan");
        let req = d
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(req.iter().any(|v| v.as_str() == Some("plan_id")));
    }

    // ── happy path: posts to plan-apply with the bound org query param ───────

    #[tokio::test]
    async fn posts_plan_apply_with_bound_org_and_returns_row() {
        let server = MockServer::start_async().await;
        let plan_id = Uuid::now_v7();
        let org = Uuid::now_v7();
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path(format!("/v1/admin/plans/{plan_id}/apply"))
                    .header("authorization", "Bearer tt_live_operator")
                    .query_param("org_id", org.to_string());
                then.status(200).json_body(json!({
                    "id": plan_id.to_string(),
                    "org_id": org.to_string(),
                    "status": "applied"
                }));
            })
            .await;

        let out = tool(server.base_url(), org)
            .call(json!({ "plan_id": plan_id.to_string() }))
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(out["applied"], true);
        assert_eq!(out["plan"]["status"], "applied");
        assert_eq!(out["plan"]["id"], plan_id.to_string());
    }

    #[tokio::test]
    async fn already_applied_is_idempotent_success() {
        let server = MockServer::start_async().await;
        let plan_id = Uuid::now_v7();
        let org = Uuid::now_v7();
        let _m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path(format!("/v1/admin/plans/{plan_id}/apply"));
                // Cloud returns the existing applied row (no-op).
                then.status(200).json_body(json!({
                    "id": plan_id.to_string(),
                    "status": "applied"
                }));
            })
            .await;
        let out = tool(server.base_url(), org)
            .call(json!({ "plan_id": plan_id.to_string() }))
            .await
            .unwrap();
        assert_eq!(out["applied"], true);
        assert_eq!(out["plan"]["status"], "applied");
    }

    // ── input validation ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn missing_plan_id_rejected() {
        let err = tool("http://unused".into(), Uuid::now_v7())
            .call(json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn non_uuid_plan_id_rejected() {
        let err = tool("http://unused".into(), Uuid::now_v7())
            .call(json!({ "plan_id": "not-a-uuid" }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    // ── rollback / error mapping ─────────────────────────────────────────────

    #[tokio::test]
    async fn cloud_404_maps_to_invalid_params() {
        let server = MockServer::start_async().await;
        let plan_id = Uuid::now_v7();
        let _m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path(format!("/v1/admin/plans/{plan_id}/apply"));
                then.status(404).body("no plan with that id");
            })
            .await;
        let err = tool(server.base_url(), Uuid::now_v7())
            .call(json!({ "plan_id": plan_id.to_string() }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
        assert_eq!(err.code(), -32602);
    }

    #[tokio::test]
    async fn cloud_500_rollback_maps_to_internal_error() {
        let server = MockServer::start_async().await;
        let plan_id = Uuid::now_v7();
        let _m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path(format!("/v1/admin/plans/{plan_id}/apply"));
                then.status(500)
                    .body("apply plan: route writeback rejected");
            })
            .await;
        let err = tool(server.base_url(), Uuid::now_v7())
            .call(json!({ "plan_id": plan_id.to_string() }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Internal(_)));
        assert_eq!(err.code(), -32603);
    }

    #[tokio::test]
    async fn cloud_401_maps_to_unauthorized() {
        let server = MockServer::start_async().await;
        let plan_id = Uuid::now_v7();
        let _m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path(format!("/v1/admin/plans/{plan_id}/apply"));
                then.status(401).body("bad key");
            })
            .await;
        let err = tool(server.base_url(), Uuid::now_v7())
            .call(json!({ "plan_id": plan_id.to_string() }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
        assert_eq!(err.code(), -32001);
    }

    #[tokio::test]
    async fn connectivity_fault_maps_to_internal_not_panic() {
        let err = tool("http://127.0.0.1:1".into(), Uuid::now_v7())
            .call(json!({ "plan_id": Uuid::now_v7().to_string() }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Internal(_)));
    }

    // ── org scoping ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rejects_foreign_org_id_before_network() {
        let server = MockServer::start_async().await;
        let plan_id = Uuid::now_v7();
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path(format!("/v1/admin/plans/{plan_id}/apply"));
                then.status(200).body("{}");
            })
            .await;
        let bound = Uuid::now_v7();
        let other = Uuid::now_v7();
        let err = tool(server.base_url(), bound)
            .call(json!({
                "plan_id": plan_id.to_string(),
                "org_id": other.to_string()
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
        assert_eq!(err.code(), -32001);
        assert_eq!(m.hits_async().await, 0);
    }

    #[tokio::test]
    async fn allows_matching_org_id_and_sends_bound_org() {
        let server = MockServer::start_async().await;
        let plan_id = Uuid::now_v7();
        let bound = Uuid::now_v7();
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path(format!("/v1/admin/plans/{plan_id}/apply"))
                    .query_param("org_id", bound.to_string());
                then.status(200).json_body(json!({
                    "id": plan_id.to_string(),
                    "status": "applied"
                }));
            })
            .await;
        let out = tool(server.base_url(), bound)
            .call(json!({
                "plan_id": plan_id.to_string(),
                "org_id": bound.to_string()
            }))
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(out["applied"], true);
    }
}
