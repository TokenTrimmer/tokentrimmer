//! `get_repo_context` — read-only MCP tool. Given a task, returns the most
//! relevant repo files (symbol/import-graph + lexical ranking) + outlines +
//! budget-bounded inlined content, so a coding agent skips exploration.
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct GetRepoContextTool;

#[derive(Deserialize)]
struct Input {
    #[serde(default = "default_path")]
    repo_path: String,
    task: String,
    #[serde(default = "default_max_files")]
    max_files: usize,
    #[serde(default = "default_budget")]
    token_budget: u32,
}
fn default_path() -> String {
    ".".into()
}
fn default_max_files() -> usize {
    12
}
fn default_budget() -> u32 {
    6000
}

#[async_trait]
impl Tool for GetRepoContextTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "get_repo_context",
            description: "Given a coding task, return the most relevant files in \
                the repo (ranked by symbol/import-graph + lexical match) with a \
                symbol outline, why each was chosen, and the top files' content \
                within a token budget — so you can skip exploring the codebase. \
                Read-only, fully local (no network).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string", "description": "Repo root to index (default: current dir)." },
                    "task": { "type": "string", "description": "The coding task in plain English." },
                    "max_files": { "type": "integer", "description": "Max files to describe (default 12)." },
                    "token_budget": { "type": "integer", "description": "Token cap for inlined file content (default 6000)." }
                },
                "required": ["task"]
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let inp: Input =
            serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let pack = tt_context::repo_context(
            std::path::Path::new(&inp.repo_path),
            &inp.task,
            inp.max_files,
            inp.token_budget,
        );
        serde_json::to_value(&pack).map_err(|e| McpError::Internal(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[tokio::test]
    async fn returns_ranked_files_for_a_task() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.py"),
            "def authenticate():\n    pass\n",
        )
        .unwrap();
        let tool = GetRepoContextTool;
        let out = tool
            .call(json!({
                "repo_path": dir.path().to_string_lossy(), "task": "fix authenticate", "max_files": 5, "token_budget": 1000
            }))
            .await
            .unwrap();
        assert!(out["files"].is_array());
        assert!(out["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"].as_str().unwrap().ends_with("auth.py")));
        assert!(out["token_estimate"].is_number());
        assert_eq!(tool.def().name, "get_repo_context");
    }
}
