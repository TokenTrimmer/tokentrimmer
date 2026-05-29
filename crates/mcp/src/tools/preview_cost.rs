//! preview_cost tool — calls tt-preview directly when the `preview` feature
//! is on; otherwise returns a 501-style result.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct PreviewCostTool;

#[async_trait]
impl Tool for PreviewCostTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "preview_cost",
            description: "Estimate the cost of an LLM request before sending it. Returns current-model cost, cheaper-equivalent suggestions with quality risk bands, and cache hit probability.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "model": { "type": "string" },
                    "messages": { "type": "array" },
                    "max_tokens": { "type": "integer" }
                },
                "required": ["model", "messages"]
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        #[cfg(feature = "preview")]
        {
            let req: tt_preview::PreviewRequest = serde_json::from_value(params)
                .map_err(|e| McpError::InvalidParams(e.to_string()))?;
            let resp = tt_preview::preview(&req).map_err(|e| McpError::Internal(e.to_string()))?;
            return Ok(serde_json::to_value(resp).unwrap());
        }
        #[cfg(not(feature = "preview"))]
        {
            let _ = params;
            return Err(McpError::Internal(
                "preview feature disabled at build time".into(),
            ));
        }
    }
}
