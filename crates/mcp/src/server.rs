//! Server dispatcher. Owns the tool + resource registries.

use serde_json::{json, Value};

use crate::auth::Authenticator;
use crate::error::McpError;
use crate::protocol::{methods, JsonRpcRequest, JsonRpcResponse};
use crate::resources::Registry as ResourceRegistry;
use crate::tools::Registry as ToolRegistry;

pub struct Server {
    pub tools: ToolRegistry,
    pub resources: ResourceRegistry,
    /// Optional process-lifetime authenticator. When present, the server
    /// verifies the operator key against the key store on the first
    /// `tools/call` / `resources/read` and binds the resulting tenant for the
    /// process lifetime (design §8). When `None` (the local dev boot, where the
    /// transport's bearer guard is the gate and no key store exists to verify
    /// against), dispatch proceeds without store-backed verification.
    authenticator: Option<Authenticator>,
}

impl Server {
    pub fn new() -> Self {
        Self {
            tools: ToolRegistry::new(),
            resources: ResourceRegistry::new(),
            authenticator: None,
        }
    }

    /// Attach a process-lifetime [`Authenticator`] so the server performs real,
    /// store-backed key verification on the first tool/resource call and binds
    /// the verified tenant for the process lifetime (design §8). Without this,
    /// dispatch relies solely on the transport's loopback bearer guard.
    #[must_use]
    pub fn with_authenticator(mut self, authenticator: Authenticator) -> Self {
        self.authenticator = Some(authenticator);
        self
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
            methods::TOOLS_CALL => match self.authenticate().await {
                Ok(()) => self.tools_call(req.params).await,
                Err(e) => Err(e),
            },
            methods::RESOURCES_LIST => Ok(json!({ "resources": self.resources.list() })),
            methods::RESOURCES_READ => match self.authenticate().await {
                Ok(()) => self.resources_read(req.params).await,
                Err(e) => Err(e),
            },
            methods::SHUTDOWN => Ok(json!({})),
            other => Err(McpError::MethodNotFound(other.into())),
        };
        match result {
            Ok(v) => JsonRpcResponse::ok(id, v),
            Err(e) => JsonRpcResponse::err(id, e.code(), e.to_string()),
        }
    }

    /// Enforce store-backed auth before a tool/resource call when an
    /// [`Authenticator`] is wired (design §8: validate on first call, cache the
    /// `org_id` for the process lifetime). The verified [`tt_auth::ApiKeyContext`]
    /// is resolved here and bound for the process lifetime so tools act on the
    /// right tenant; an invalid/absent/revoked key fails the call closed with
    /// `unauthorized` (`-32001`). A no-op when no authenticator is wired.
    async fn authenticate(&self) -> Result<(), McpError> {
        match &self.authenticator {
            Some(auth) => {
                let ctx = auth.context().await?;
                tracing::trace!(org_id = %ctx.org_id, "MCP request bound to verified tenant");
                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn tools_call(&self, params: Value) -> Result<Value, McpError> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::InvalidParams("missing tool name".into()))?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        self.tools.call(name, args).await
    }

    async fn resources_read(&self, params: Value) -> Result<Value, McpError> {
        let uri = params
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::InvalidParams("missing uri".into()))?;
        self.resources.read(uri).await
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tt_auth::{issue, Environment, InMemoryKeyStore};
    use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
    use uuid::Uuid;

    use super::*;
    use crate::auth::Authenticator;
    use crate::tools::find_route_for::FindRouteForTool;

    /// Mint a real argon2-hashed key into `store` (via `tt_auth::issue`, so the
    /// genuine hashing path runs) and return its plaintext.
    async fn seed_key(store: &InMemoryKeyStore, org: Uuid) -> String {
        let audit = InMemoryAuditWriter::default();
        issue(
            store,
            &audit,
            org,
            "mcp-server-test",
            Environment::Live,
            Actor::System,
        )
        .await
        .expect("issue key")
        .plaintext
    }

    fn tools_call_req() -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: methods::TOOLS_CALL.into(),
            params: json!({
                "name": "find_route_for",
                "arguments": { "task_description": "classify this email as spam" }
            }),
            id: Some(json!(1)),
        }
    }

    fn server_with_tool() -> Server {
        let mut s = Server::new();
        s.tools.register(Box::new(FindRouteForTool));
        s
    }

    /// With a valid operator key wired, `tools/call` is authorized and reaches
    /// the tool (no `-32001`).
    #[tokio::test]
    async fn valid_key_authorizes_tools_call() {
        let store = InMemoryKeyStore::new();
        let org = Uuid::now_v7();
        let plaintext = seed_key(&store, org).await;

        let server =
            server_with_tool().with_authenticator(Authenticator::new(Arc::new(store), plaintext));

        let resp = server.dispatch(tools_call_req()).await;
        assert!(resp.error.is_none(), "valid key must authorize the call");
        assert!(resp.result.is_some());
    }

    /// The verified context binds the issuing org and is cached for the process
    /// lifetime (verified once, reused thereafter).
    #[tokio::test]
    async fn binds_and_caches_verified_org() {
        let store = InMemoryKeyStore::new();
        let org = Uuid::now_v7();
        let plaintext = seed_key(&store, org).await;

        let auth = Authenticator::new(Arc::new(store), plaintext);
        let first = auth.context().await.expect("first verify").org_id;
        let second = auth.context().await.expect("cached verify").org_id;
        assert_eq!(first, org, "verified context must carry the issuing org");
        assert_eq!(second, org, "cached context must be the same org");
    }

    /// An invalid operator key fails the tool call closed with `unauthorized`.
    #[tokio::test]
    async fn invalid_key_rejects_tools_call() {
        let store = InMemoryKeyStore::new(); // empty: key was never issued
        let server = server_with_tool()
            .with_authenticator(Authenticator::new(Arc::new(store), "tt_live_deadbeef0000"));

        let resp = server.dispatch(tools_call_req()).await;
        let err = resp.error.expect("invalid key must be rejected");
        assert_eq!(err.code, McpError::Unauthorized(String::new()).code());
        assert_eq!(err.code, -32001);
    }

    /// Without an authenticator wired (local dev boot), dispatch proceeds — the
    /// transport bearer guard is the gate. This preserves existing behaviour.
    #[tokio::test]
    async fn no_authenticator_allows_tools_call() {
        let server = server_with_tool();
        let resp = server.dispatch(tools_call_req()).await;
        assert!(
            resp.error.is_none(),
            "no authenticator → unguarded dispatch"
        );
    }
}
