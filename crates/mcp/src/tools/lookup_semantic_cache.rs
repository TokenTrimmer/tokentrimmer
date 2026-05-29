//! lookup_semantic_cache — HTTP call to the cloud-side L2 cache lookup
//! endpoint. Returns a redacted summary (never raw cached text).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct LookupSemanticCacheTool {
    pub base_url: String,
    pub api_key: String,
    pub http: reqwest::Client,
}

#[derive(Deserialize)]
struct Input {
    prompt: String,
}

#[async_trait]
impl Tool for LookupSemanticCacheTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "lookup_semantic_cache",
            description: "Check if a semantically-similar prompt has been answered recently. Returns a redacted summary (no raw text).",
            input_schema: json!({
                "type": "object",
                "properties": { "prompt": { "type": "string" } },
                "required": ["prompt"]
            }),
        }
    }
    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let inp: Input =
            serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let url = format!("{}/v1/admin/cache/semantic-lookup", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&json!({ "prompt": inp.prompt }))
            .send()
            .await
            .map_err(|e| McpError::Internal(format!("http: {e}")))?;
        if !resp.status().is_success() {
            return Ok(json!({ "hit": false, "reason": format!("upstream {}", resp.status()) }));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| McpError::Internal(format!("json: {e}")))?;
        Ok(body)
    }
}
