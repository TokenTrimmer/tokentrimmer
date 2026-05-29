//! Resource trait + registry.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;
use crate::protocol::ResourceDef;

#[async_trait]
pub trait Resource: Send + Sync {
    fn def(&self) -> ResourceDef;
    async fn read(&self) -> Result<Value, McpError>;
}

pub struct Registry {
    resources: Vec<Box<dyn Resource>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            resources: Vec::new(),
        }
    }
    pub fn register(&mut self, r: Box<dyn Resource>) {
        self.resources.push(r);
    }
    pub fn list(&self) -> Vec<ResourceDef> {
        self.resources.iter().map(|r| r.def()).collect()
    }
    pub async fn read(&self, uri: &str) -> Result<Value, McpError> {
        for r in &self.resources {
            if r.def().uri == uri {
                return r.read().await;
            }
        }
        Err(McpError::MethodNotFound(format!("resource {uri}")))
    }
}
impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

pub mod cost_ledger;
pub mod inspect_baseline;
