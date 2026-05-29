//! Server dispatcher. Owns the tool + resource registries.

use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::{methods, JsonRpcRequest, JsonRpcResponse};
use crate::resources::Registry as ResourceRegistry;
use crate::tools::Registry as ToolRegistry;

pub struct Server {
    pub tools: ToolRegistry,
    pub resources: ResourceRegistry,
}

impl Server {
    pub fn new() -> Self {
        Self { tools: ToolRegistry::new(), resources: ResourceRegistry::new() }
    }
    pub async fn dispatch(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone();
        let result = match req.method.as_str() {
            methods::INITIALIZE => Ok(json!({
                "protocolVersion": "0.1",
                "serverInfo": { "name": "tt-mcp", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "tools": {}, "resources": {} }
            })),
            methods::TOOLS_LIST => Ok(json!({ "tools": self.tools.list() })),
            methods::TOOLS_CALL => self.tools_call(req.params).await,
            methods::RESOURCES_LIST => Ok(json!({ "resources": self.resources.list() })),
            methods::RESOURCES_READ => self.resources_read(req.params).await,
            methods::SHUTDOWN => Ok(json!({})),
            other => Err(McpError::MethodNotFound(other.into())),
        };
        match result {
            Ok(v) => JsonRpcResponse::ok(id, v),
            Err(e) => JsonRpcResponse::err(id, e.code(), e.to_string()),
        }
    }

    async fn tools_call(&self, params: Value) -> Result<Value, McpError> {
        let name = params.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| McpError::InvalidParams("missing tool name".into()))?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        self.tools.call(name, args).await
    }

    async fn resources_read(&self, params: Value) -> Result<Value, McpError> {
        let uri = params.get("uri").and_then(|v| v.as_str())
            .ok_or_else(|| McpError::InvalidParams("missing uri".into()))?;
        self.resources.read(uri).await
    }
}

impl Default for Server { fn default() -> Self { Self::new() } }
