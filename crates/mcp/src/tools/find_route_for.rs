//! find_route_for — cheap classifier + pricing table.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct FindRouteForTool;

#[derive(Deserialize)]
struct Input {
    task_description: String,
}

#[async_trait]
impl Tool for FindRouteForTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "find_route_for",
            description: "Given a task description in plain English, return the cheapest model that historically handles it with HIGH quality confidence.",
            input_schema: json!({
                "type": "object",
                "properties": { "task_description": { "type": "string" } },
                "required": ["task_description"]
            }),
        }
    }
    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let inp: Input = serde_json::from_value(params)
            .map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let lower = inp.task_description.to_lowercase();
        let (model, rationale) = if lower.contains("classify") || lower.contains("yes or no") {
            ("claude-haiku-4-5", "classification — Haiku is the cheapest model with high quality on yes/no tasks.")
        } else if lower.contains("extract") || lower.contains("parse") || lower.contains("json") {
            ("claude-haiku-4-5", "extraction — Haiku with explicit max_tokens is the cheapest model with reliable structured output.")
        } else if lower.contains("code") || lower.contains("function") || lower.contains("refactor") {
            ("claude-haiku-4-5", "code — Haiku handles small refactors; escalate to Sonnet if the diff is multi-file.")
        } else {
            ("claude-haiku-4-5", "chat — Haiku is the cost-discipline default; escalate only when you actually need Sonnet/Opus reasoning.")
        };
        Ok(json!({ "model": model, "rationale": rationale }))
    }
}
