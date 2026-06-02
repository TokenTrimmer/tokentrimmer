//! simulate_plan — admin tool that projects the savings of a proposed
//! routing config against historical traffic. Forwards the input to the
//! existing cloud-side `POST /v1/admin/plans` endpoint and returns its
//! response verbatim.
//!
//! Hidden when the server is booted with `--read-only` because the
//! underlying endpoint creates a new `plan_runs` row.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct SimulatePlanTool {
    pub base_url: String,
    pub api_key: String,
    pub http: reqwest::Client,
}

#[async_trait]
impl Tool for SimulatePlanTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "simulate_plan",
            description: "Project savings if a proposed routing config is applied to your historical traffic. Returns projected cost, savings, cache hit rate, quality risk band, with bootstrap CIs.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "proposed_config": { "type": "object" },
                    "window_days": { "type": "integer", "default": 7 }
                },
                "required": ["proposed_config"]
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let url = format!("{}/v1/admin/plans", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&params)
            .send()
            .await
            .map_err(|e| McpError::Internal(format!("http: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(McpError::Internal(format!(
                "simulate_plan upstream {status}"
            )));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| McpError::Internal(format!("json: {e}")))?;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn make_tool(base_url: String) -> SimulatePlanTool {
        SimulatePlanTool {
            base_url,
            api_key: "tt_live_test".into(),
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn def_advertises_required_field() {
        let t = make_tool("http://unused".into());
        let d = t.def();
        assert_eq!(d.name, "simulate_plan");
        let req = d
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(req.iter().any(|v| v.as_str() == Some("proposed_config")));
    }

    #[tokio::test]
    async fn call_forwards_to_admin_plans_and_returns_body() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/admin/plans");
                then.status(200).json_body(json!({
                    "plan_id": "abc",
                    "projected_savings_usd": 12.34
                }));
            })
            .await;
        let t = make_tool(server.base_url());
        let result = t
            .call(json!({
                "proposed_config": { "routes": [] },
                "window_days": 7
            }))
            .await
            .unwrap();
        assert_eq!(result["plan_id"], "abc");
        assert_eq!(result["projected_savings_usd"], 12.34);
    }

    #[tokio::test]
    async fn call_returns_internal_on_5xx() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/admin/plans");
                then.status(500).body("boom");
            })
            .await;
        let t = make_tool(server.base_url());
        let err = t.call(json!({ "proposed_config": {} })).await.unwrap_err();
        assert!(matches!(err, McpError::Internal(_)));
    }
}
