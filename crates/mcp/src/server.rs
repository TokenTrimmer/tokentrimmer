//! Server dispatcher. Owns the tool + resource registries.

use uuid::Uuid;

use serde_json::{json, Value};

use crate::auth::Authenticator;
use crate::error::McpError;
use crate::protocol::{methods, JsonRpcRequest, JsonRpcResponse};
use crate::resources::Registry as ResourceRegistry;
use crate::tools::add_route::AddRouteTool;
use crate::tools::apply_plan::ApplyPlanTool;
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
    /// Whether mutating ("write") tools (`add_route`, `apply_plan`) may be
    /// exposed. **Off by default** — read-only is the safe default. A write
    /// tool is registered ONLY when this is true (see
    /// [`Server::register_write_tools`]); when false those tools are never in
    /// the registry, so `tools/list` omits them and `tools/call` on them
    /// returns `MethodNotFound` (-32601) — NOT `Unauthorized`. The boot wires
    /// this from `--allow-write` / `TT_MCP_ALLOW_WRITE`.
    write_enabled: bool,
}

impl Server {
    pub fn new() -> Self {
        Self {
            tools: ToolRegistry::new(),
            resources: ResourceRegistry::new(),
            authenticator: None,
            write_enabled: false,
        }
    }

    /// Enable the mutating write tools (`add_route`, `apply_plan`). Off by
    /// default; the boot sets this from `--allow-write` / `TT_MCP_ALLOW_WRITE`.
    /// Enabling the flag does not by itself register any tool — call
    /// [`Server::register_write_tools`] (which is a no-op unless this is set) to
    /// add them once the bound org + outbound HTTP client are known.
    #[must_use]
    pub fn with_write_enabled(mut self, write_enabled: bool) -> Self {
        self.write_enabled = write_enabled;
        self
    }

    /// Whether write tools are permitted on this server (the read-only safe
    /// default is `false`). The boot consults this before registering write
    /// tools; exposed so callers don't duplicate the gating decision.
    #[must_use]
    pub fn write_enabled(&self) -> bool {
        self.write_enabled
    }

    /// Conditionally register the mutating write tools, scoped to the bound
    /// `org_id` (the operator key's verified org). A **no-op when
    /// [`Server::write_enabled`] is false** — this is the single gate that keeps
    /// write tools off by default: when disabled they are never added to the
    /// registry, so `tools/list` cannot reveal them and `tools/call` falls
    /// through to `MethodNotFound` (-32601) rather than `Unauthorized`.
    ///
    /// `gateway_base` is where `add_route` POSTs `/v1/routes`; `cloud_base` is
    /// where `apply_plan` POSTs `/v1/admin/plans/:id/apply`. Both forward
    /// `api_key` as the operator bearer (the only org that can be written). The
    /// `http` client is shared by both tools.
    pub fn register_write_tools(
        &mut self,
        org_id: Uuid,
        gateway_base: impl Into<String>,
        cloud_base: impl Into<String>,
        api_key: impl Into<String>,
        http: reqwest::Client,
    ) {
        if !self.write_enabled {
            return;
        }
        let api_key = api_key.into();
        self.tools.register(Box::new(AddRouteTool {
            gateway_base: gateway_base.into(),
            api_key: api_key.clone(),
            http: http.clone(),
            org_id,
        }));
        self.tools.register(Box::new(ApplyPlanTool {
            cloud_base: cloud_base.into(),
            api_key,
            http,
            org_id,
        }));
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

    // ── write-tool gating (SAFETY: off by default) ───────────────────────────

    fn tool_names(server: &Server) -> Vec<&'static str> {
        server.tools.list().into_iter().map(|d| d.name).collect()
    }

    fn register_writes(server: &mut Server) {
        server.register_write_tools(
            Uuid::now_v7(),
            "http://gateway.invalid",
            "http://cloud.invalid",
            "tt_live_op",
            reqwest::Client::new(),
        );
    }

    fn call_tool_req(name: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            method: methods::TOOLS_CALL.into(),
            params: json!({ "name": name, "arguments": {} }),
            id: Some(json!(7)),
        }
    }

    /// Default server is read-only: `write_enabled()` is false and registering
    /// write tools is a no-op — neither write tool appears in `tools/list`.
    #[tokio::test]
    async fn write_disabled_by_default_hides_write_tools() {
        let mut server = Server::new();
        assert!(!server.write_enabled(), "read-only must be the default");
        register_writes(&mut server); // no-op while disabled
        let names = tool_names(&server);
        assert!(
            !names.contains(&"add_route"),
            "add_route must be hidden by default"
        );
        assert!(
            !names.contains(&"apply_plan"),
            "apply_plan must be hidden by default"
        );
    }

    /// With write disabled, `tools/call` on a write tool returns
    /// `MethodNotFound` (-32601) — NOT `Unauthorized`. A read-only deployment
    /// behaves as if the tool simply does not exist.
    #[tokio::test]
    async fn write_disabled_tools_call_returns_method_not_found() {
        let mut server = Server::new();
        register_writes(&mut server); // no-op while disabled
        for name in ["add_route", "apply_plan"] {
            let resp = server.dispatch(call_tool_req(name)).await;
            let err = resp.error.unwrap_or_else(|| panic!("{name} must error"));
            assert_eq!(
                err.code, -32601,
                "{name} must be MethodNotFound, not Unauthorized"
            );
            assert_ne!(err.code, -32001, "{name} must NOT be Unauthorized");
        }
    }

    /// With write enabled, both write tools are registered and appear in
    /// `tools/list`.
    #[tokio::test]
    async fn write_enabled_exposes_both_write_tools() {
        let mut server = Server::new().with_write_enabled(true);
        assert!(server.write_enabled());
        register_writes(&mut server);
        let names = tool_names(&server);
        assert!(names.contains(&"add_route"), "add_route must be listed");
        assert!(names.contains(&"apply_plan"), "apply_plan must be listed");
    }

    /// Enabling the flag without calling `register_write_tools` still exposes
    /// nothing — registration is the second, explicit step.
    #[tokio::test]
    async fn write_enabled_without_registration_exposes_nothing() {
        let server = Server::new().with_write_enabled(true);
        let names = tool_names(&server);
        assert!(!names.contains(&"add_route"));
        assert!(!names.contains(&"apply_plan"));
    }
}
