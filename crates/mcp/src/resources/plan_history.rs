//! plan_history resource — recent `plan_runs` rows from the cloud-side
//! tt-api. Read-only; never mutates. URI query string carries `last=N`
//! (default 10, max 100) for the caller to cap the response size.

use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use crate::error::McpError;
use crate::protocol::ResourceDef;
use crate::resources::Resource;

pub struct PlanHistoryResource {
    pub base_url: String,
    pub api_key: String,
    pub http: reqwest::Client,
}

const DEFAULT_LAST: u32 = 10;
const MAX_LAST: u32 = 100;
const URI: &str = "mcp://tokentrimmer/plan/history?last=10";

#[async_trait]
impl Resource for PlanHistoryResource {
    fn def(&self) -> ResourceDef {
        ResourceDef {
            uri: URI.into(),
            name: "Plan history".into(),
            description: Some(
                "Recent applied + unapplied plans from the hosted Plan engine.".into(),
            ),
            mime_type: "application/json",
        }
    }

    async fn read(&self) -> Result<Value, McpError> {
        // Default `last` — clients that want a different N should call the
        // tt-api directly. v1 resources don't carry per-call params (MCP
        // 0.1).
        self.fetch(DEFAULT_LAST).await
    }
}

impl PlanHistoryResource {
    pub async fn fetch(&self, last: u32) -> Result<Value, McpError> {
        let n = last.clamp(1, MAX_LAST);
        let url = format!("{}/v1/admin/plans?last={n}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| McpError::Internal(format!("http: {e}")))?;
        if !resp.status().is_success() {
            return Err(McpError::Internal(format!(
                "plan_history upstream {}",
                resp.status()
            )));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| McpError::Internal(format!("json: {e}")))?;
        Ok(serde_json::json!({
            "uri": URI,
            "plans": body
        }))
    }

    /// Parse `last=N` out of an mcp:// URI like
    /// `mcp://tokentrimmer/plan/history?last=25`. Returns `DEFAULT_LAST`
    /// when the query is absent or malformed; clamps to `[1, MAX_LAST]`.
    pub fn parse_last(uri: &str) -> u32 {
        let n = Url::parse(uri)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "last")
                    .and_then(|(_, v)| v.parse::<u32>().ok())
            })
            .unwrap_or(DEFAULT_LAST);
        n.clamp(1, MAX_LAST)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    fn make_resource(base_url: String) -> PlanHistoryResource {
        PlanHistoryResource {
            base_url,
            api_key: "tt_live_test".into(),
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn parse_last_defaults_when_absent() {
        assert_eq!(
            PlanHistoryResource::parse_last("mcp://tokentrimmer/plan/history"),
            DEFAULT_LAST
        );
    }

    #[test]
    fn parse_last_reads_explicit() {
        assert_eq!(
            PlanHistoryResource::parse_last("mcp://tokentrimmer/plan/history?last=25"),
            25
        );
    }

    #[test]
    fn parse_last_clamps_max() {
        assert_eq!(
            PlanHistoryResource::parse_last("mcp://tokentrimmer/plan/history?last=999"),
            MAX_LAST
        );
    }

    #[test]
    fn parse_last_clamps_zero_to_one() {
        assert_eq!(
            PlanHistoryResource::parse_last("mcp://tokentrimmer/plan/history?last=0"),
            1
        );
    }

    #[tokio::test]
    async fn read_calls_admin_plans_and_returns_body() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/v1/admin/plans")
                    .query_param("last", "10");
                then.status(200).json_body(json!([
                    { "plan_id": "abc", "projected_savings_usd": 1.23 }
                ]));
            })
            .await;
        let r = make_resource(server.base_url());
        let result = r.read().await.unwrap();
        assert_eq!(result["uri"], URI);
        assert_eq!(result["plans"][0]["plan_id"], "abc");
    }

    #[tokio::test]
    async fn read_returns_internal_on_5xx() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(GET).path("/v1/admin/plans");
                then.status(500).body("boom");
            })
            .await;
        let r = make_resource(server.base_url());
        let err = r.read().await.unwrap_err();
        assert!(matches!(err, McpError::Internal(_)));
    }
}
