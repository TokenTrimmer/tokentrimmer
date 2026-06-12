//! Tool trait + registry. Each tool is invoked by name from `tools/call`.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;
use crate::protocol::ToolDef;

#[async_trait]
pub trait Tool: Send + Sync {
    fn def(&self) -> ToolDef;
    async fn call(&self, params: Value) -> Result<Value, McpError>;
}

pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }
    pub fn register(&mut self, t: Box<dyn Tool>) {
        self.tools.push(t);
    }
    pub fn list(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|t| t.def()).collect()
    }
    pub async fn call(&self, name: &str, params: Value) -> Result<Value, McpError> {
        for t in &self.tools {
            if t.def().name == name {
                return t.call(params).await;
            }
        }
        Err(McpError::MethodNotFound(format!("tool {name}")))
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

pub mod add_route;
pub mod apply_plan;
pub mod batch_savings;
pub mod cost_control;
pub mod find_route_for;
pub mod inspect_diff;
pub mod list_datasets;
pub mod lookup_semantic_cache;
pub mod preview_cost;
pub mod run_query;
pub mod simulate_plan;
