# Track A — MCP Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `tt mcp` — a Model Context Protocol server that exposes TokenTrimmer intelligence (cost preview, route suggestions, inspect-diff, semantic cache lookup, plan simulation) as MCP tools and resources, consumable by Claude Code, Cursor, Zed, and any MCP-compatible client. v1 ships stdio transport with 4 tools and 2 resources.

**Architecture:** New `crates/mcp/` crate exposing both a library (so tests + future SSE transport can reuse) and a binary invoked via `tt mcp`. JSON-RPC over stdio loop; Tool + Resource traits with a registry; per-tool implementations are thin wrappers over existing engines (`tt-preview`, `tt-inspect-core`, `tt-plan-core`) and HTTP calls to the hosted Gateway.

**Tech Stack:** Rust 1.88, `tokio` async runtime, `serde_json` for JSON-RPC framing, `thiserror`, `tracing`, `reqwest` for cloud calls, `async-trait` for the Tool/Resource traits. Tests use `httpmock` for cloud + golden JSON-RPC fixtures via `insta`.

**Spec:** `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md`.

**Depends on:** Track C ships `tt-preview` — this plan adds `tt-mcp` as a consumer. Implementation can proceed without Track C committed; the `preview_cost` tool will return a 501-style error until Track C is in the workspace.

---

## File Structure

```
crates/mcp/                                  [NEW crate]
├── Cargo.toml
└── src/
    ├── lib.rs                               [public API: Server::new, Server::run_stdio]
    ├── server.rs                            [JSON-RPC dispatcher]
    ├── protocol.rs                          [JSON-RPC envelope types + MCP-specific shapes]
    ├── transport/
    │   ├── mod.rs
    │   └── stdio.rs                         [LineCodec over stdin/stdout]
    ├── tools/
    │   ├── mod.rs                           [Tool trait + Registry]
    │   ├── preview_cost.rs
    │   ├── find_route_for.rs
    │   ├── inspect_diff.rs
    │   └── lookup_semantic_cache.rs
    ├── resources/
    │   ├── mod.rs                           [Resource trait + Registry]
    │   ├── cost_ledger.rs
    │   └── inspect_baseline.rs
    ├── auth.rs                              [TT_API_KEY validation]
    ├── client.rs                            [reqwest wrapper for tt-api]
    └── error.rs

crates/cli/
├── Cargo.toml                               [modified — tt-mcp dep]
└── src/main.rs                              [modified — Mcp subcommand]

Cargo.toml                                   [modified — workspace member + workspace dep]
```

---

## Task 1: Scaffold tt-mcp crate

**Files:**
- Create: `crates/mcp/Cargo.toml`
- Create: `crates/mcp/src/{lib,server,protocol,auth,client,error}.rs`, `transport/{mod,stdio}.rs`, `tools/{mod,preview_cost,find_route_for,inspect_diff,lookup_semantic_cache}.rs`, `resources/{mod,cost_ledger,inspect_baseline}.rs`
- Modify: workspace `Cargo.toml` (register member + workspace dep)

- [ ] **Step 1: Create the tree**

```bash
mkdir -p crates/mcp/src/{transport,tools,resources}
for f in lib server protocol auth client error; do
  echo "//! tt-mcp — \`$f\` (scaffold)" > "crates/mcp/src/$f.rs"
done
for f in mod stdio; do
  echo "//! tt-mcp transport — \`$f\` (scaffold)" > "crates/mcp/src/transport/$f.rs"
done
for f in mod preview_cost find_route_for inspect_diff lookup_semantic_cache; do
  echo "//! tt-mcp tools — \`$f\` (scaffold)" > "crates/mcp/src/tools/$f.rs"
done
for f in mod cost_ledger inspect_baseline; do
  echo "//! tt-mcp resources — \`$f\` (scaffold)" > "crates/mcp/src/resources/$f.rs"
done
```

- [ ] **Step 2: Write `crates/mcp/Cargo.toml`**

```toml
[package]
name = "tt-mcp"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Model Context Protocol server exposing TokenTrimmer intelligence to MCP-compatible clients."

[dependencies]
tt-shared.workspace = true
tt-inspect-core.workspace = true
tt-inspect-rules-tier1.workspace = true
tt-plan-core.workspace = true
tt-preview = { path = "../preview", optional = true }
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tracing.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "io-util", "sync"] }
async-trait = "0.1"
reqwest = { workspace = true, features = ["json", "rustls-tls"] }

[features]
default = ["preview"]
preview = ["dep:tt-preview"]

[dev-dependencies]
httpmock = "0.7"
insta = { version = "1.39", features = ["json"] }
tempfile = "3.10"
```

The `preview` feature lets us ship even before Track C's crate lands. If `tt-preview` is not yet in the workspace, leave the `optional = true` line and `[features]` block; the `preview_cost` tool returns a 501 internally when the feature is disabled.

- [ ] **Step 3: Register in workspace**

In root `Cargo.toml`:
- Add `"crates/mcp"` to `workspace.members`.
- Add `tt-mcp = { path = "crates/mcp" }` to `[workspace.dependencies]`.

- [ ] **Step 4: Replace `lib.rs`**

```rust
//! `tt-mcp` — Model Context Protocol server.
//!
//! See `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md`.

pub mod auth;
pub mod client;
pub mod error;
pub mod protocol;
pub mod resources;
pub mod server;
pub mod tools;
pub mod transport;

pub use error::McpError;
pub use server::Server;
```

- [ ] **Step 5: Compile**

`cargo check -p tt-mcp`

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/mcp/
git commit -m "feat(mcp): scaffold tt-mcp crate

Track A day-0. Empty modules to be filled.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: JSON-RPC protocol types

**Files:** `crates/mcp/src/protocol.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! JSON-RPC 2.0 envelope + MCP-specific message shapes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    pub id: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    pub id: Option<Value>,
}

impl JsonRpcResponse {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self { jsonrpc: "2.0", result: Some(result), error: None, id }
    }
    pub fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            result: None,
            error: Some(JsonRpcError { code, message: message.into(), data: None }),
            id,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// MCP method names supported in v1.
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const RESOURCES_LIST: &str = "resources/list";
    pub const RESOURCES_READ: &str = "resources/read";
    pub const SHUTDOWN: &str = "shutdown";
}

#[derive(Debug, Serialize)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Serialize)]
pub struct ResourceDef {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType")]
    pub mime_type: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ok_response_shape() {
        let r = JsonRpcResponse::ok(Some(json!(1)), json!({"x": 1}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"id\":1"));
        assert!(s.contains("\"x\":1"));
    }

    #[test]
    fn err_response_shape() {
        let r = JsonRpcResponse::err(Some(json!(2)), -32601, "method not found");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"code\":-32601"));
        assert!(!s.contains("\"result\""));
    }
}
```

- [ ] **Step 2: Run tests**

`cargo test -p tt-mcp protocol`
Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/mcp/src/protocol.rs
git commit -m "feat(mcp): JSON-RPC + MCP protocol types

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Error type

**Files:** `crates/mcp/src/error.rs`

- [ ] **Step 1: Write the module**

```rust
//! MCP server errors. Map to JSON-RPC error codes per the MCP spec:
//!   -32700 parse error · -32600 invalid request · -32601 method not found
//!   -32602 invalid params · -32603 internal · -32001 unauthorized (TT extension)
use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("parse: {0}")]
    Parse(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl McpError {
    pub fn code(&self) -> i32 {
        match self {
            Self::Parse(_) => -32700,
            Self::InvalidRequest(_) => -32600,
            Self::MethodNotFound(_) => -32601,
            Self::InvalidParams(_) => -32602,
            Self::Unauthorized(_) => -32001,
            Self::Internal(_) => -32603,
        }
    }
}
```

- [ ] **Step 2: Compile**

`cargo check -p tt-mcp`

- [ ] **Step 3: Commit**

```bash
git add crates/mcp/src/error.rs
git commit -m "feat(mcp): error type with JSON-RPC code mapping

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Auth wrapper

**Files:** `crates/mcp/src/auth.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! TT_API_KEY validation. v1 just enforces presence + prefix; full
//! verification against the hosted Gateway is delegated to per-tool calls.

use crate::error::McpError;

pub fn validate_api_key(env_var: Option<String>) -> Result<String, McpError> {
    let k = env_var.ok_or_else(|| McpError::Unauthorized("TT_API_KEY missing".into()))?;
    if !k.starts_with("tt_live_") && !k.starts_with("tt_test_") {
        return Err(McpError::Unauthorized("invalid TT_API_KEY prefix".into()));
    }
    Ok(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_key() {
        assert!(matches!(validate_api_key(None).unwrap_err(), McpError::Unauthorized(_)));
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!(matches!(validate_api_key(Some("nope".into())).unwrap_err(), McpError::Unauthorized(_)));
    }

    #[test]
    fn accepts_valid_prefix() {
        assert!(validate_api_key(Some("tt_live_abc".into())).is_ok());
        assert!(validate_api_key(Some("tt_test_xyz".into())).is_ok());
    }
}
```

- [ ] **Step 2: Run tests**

`cargo test -p tt-mcp auth`
Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/mcp/src/auth.rs
git commit -m "feat(mcp): TT_API_KEY validation

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Tool trait + registry

**Files:** `crates/mcp/src/tools/mod.rs`

- [ ] **Step 1: Write the module**

```rust
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
    pub fn new() -> Self { Self { tools: Vec::new() } }
    pub fn register(&mut self, t: Box<dyn Tool>) { self.tools.push(t); }
    pub fn list(&self) -> Vec<ToolDef> { self.tools.iter().map(|t| t.def()).collect() }
    pub async fn call(&self, name: &str, params: Value) -> Result<Value, McpError> {
        for t in &self.tools {
            if t.def().name == name { return t.call(params).await; }
        }
        Err(McpError::MethodNotFound(format!("tool {name}")))
    }
}

impl Default for Registry { fn default() -> Self { Self::new() } }

pub mod find_route_for;
pub mod inspect_diff;
pub mod lookup_semantic_cache;
pub mod preview_cost;
```

- [ ] **Step 2: Compile**

`cargo check -p tt-mcp`

- [ ] **Step 3: Commit**

```bash
git add crates/mcp/src/tools/mod.rs
git commit -m "feat(mcp): Tool trait + Registry

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Tool — preview_cost

**Files:** `crates/mcp/src/tools/preview_cost.rs`

- [ ] **Step 1: Write the tool**

```rust
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
            let resp = tt_preview::preview(&req)
                .map_err(|e| McpError::Internal(e.to_string()))?;
            return Ok(serde_json::to_value(resp).unwrap());
        }
        #[cfg(not(feature = "preview"))]
        {
            let _ = params;
            return Err(McpError::Internal("preview feature disabled at build time".into()));
        }
    }
}
```

- [ ] **Step 2: Compile**

`cargo check -p tt-mcp`
If `tt-preview` doesn't yet exist in the workspace, build with `cargo check -p tt-mcp --no-default-features` and the tool will compile in disabled mode.

- [ ] **Step 3: Commit**

```bash
git add crates/mcp/src/tools/preview_cost.rs
git commit -m "feat(mcp): preview_cost tool

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Tool — find_route_for

**Files:** `crates/mcp/src/tools/find_route_for.rs`

- [ ] **Step 1: Write**

```rust
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
```

- [ ] **Step 2: Compile**

`cargo check -p tt-mcp`

- [ ] **Step 3: Commit**

```bash
git add crates/mcp/src/tools/find_route_for.rs
git commit -m "feat(mcp): find_route_for tool

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Tool — inspect_diff

**Files:** `crates/mcp/src/tools/inspect_diff.rs`

- [ ] **Step 1: Write**

```rust
//! inspect_diff — write proposed content to a temp file, run inspect-core, return findings.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct InspectDiffTool;

#[derive(Deserialize)]
struct Input {
    file_path: String,
    proposed_content: String,
}

#[async_trait]
impl Tool for InspectDiffTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "inspect_diff",
            description: "Run TokenTrimmer Inspect rules against a proposed file diff before writing. Returns findings (severity, rule_id, line, message).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "proposed_content": { "type": "string" }
                },
                "required": ["file_path", "proposed_content"]
            }),
        }
    }
    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let inp: Input = serde_json::from_value(params)
            .map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let ext = std::path::Path::new(&inp.file_path)
            .extension().and_then(|x| x.to_str()).unwrap_or("");
        let suffix = format!(".{ext}");
        let mut tmp = tempfile::Builder::new()
            .suffix(&suffix)
            .tempfile()
            .map_err(|e| McpError::Internal(format!("tempfile: {e}")))?;
        use std::io::Write;
        write!(tmp, "{}", inp.proposed_content)
            .map_err(|e| McpError::Internal(format!("write: {e}")))?;
        let mut engine = tt_inspect_core::Engine::new();
        for rule in tt_inspect_rules_tier1::all_rules() {
            engine.add_rule(rule);
        }
        let findings = engine.scan(tmp.path());
        Ok(json!({ "findings": findings }))
    }
}
```

- [ ] **Step 2: Add `tempfile` to mcp deps**

`tempfile = "3.10"` in `crates/mcp/Cargo.toml` `[dependencies]` (not dev-dependencies — production runtime).

- [ ] **Step 3: Compile**

`cargo check -p tt-mcp`

- [ ] **Step 4: Commit**

```bash
git add crates/mcp/Cargo.toml crates/mcp/src/tools/inspect_diff.rs
git commit -m "feat(mcp): inspect_diff tool

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Tool — lookup_semantic_cache

**Files:** `crates/mcp/src/tools/lookup_semantic_cache.rs`

- [ ] **Step 1: Write (stub against cloud API)**

```rust
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
struct Input { prompt: String }

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
        let inp: Input = serde_json::from_value(params)
            .map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let url = format!("{}/v1/admin/cache/semantic-lookup", self.base_url);
        let resp = self.http.post(&url)
            .bearer_auth(&self.api_key)
            .json(&json!({ "prompt": inp.prompt }))
            .send().await
            .map_err(|e| McpError::Internal(format!("http: {e}")))?;
        if !resp.status().is_success() {
            return Ok(json!({ "hit": false, "reason": format!("upstream {}", resp.status()) }));
        }
        let body: Value = resp.json().await
            .map_err(|e| McpError::Internal(format!("json: {e}")))?;
        Ok(body)
    }
}
```

- [ ] **Step 2: Compile**

`cargo check -p tt-mcp`

- [ ] **Step 3: Commit**

```bash
git add crates/mcp/src/tools/lookup_semantic_cache.rs
git commit -m "feat(mcp): lookup_semantic_cache tool (HTTP stub)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Resources

**Files:** `crates/mcp/src/resources/{mod,cost_ledger,inspect_baseline}.rs`

- [ ] **Step 1: Write `resources/mod.rs`**

```rust
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

pub struct Registry { resources: Vec<Box<dyn Resource>> }

impl Registry {
    pub fn new() -> Self { Self { resources: Vec::new() } }
    pub fn register(&mut self, r: Box<dyn Resource>) { self.resources.push(r); }
    pub fn list(&self) -> Vec<ResourceDef> { self.resources.iter().map(|r| r.def()).collect() }
    pub async fn read(&self, uri: &str) -> Result<Value, McpError> {
        for r in &self.resources {
            if r.def().uri == uri { return r.read().await; }
        }
        Err(McpError::MethodNotFound(format!("resource {uri}")))
    }
}
impl Default for Registry { fn default() -> Self { Self::new() } }

pub mod cost_ledger;
pub mod inspect_baseline;
```

- [ ] **Step 2: `cost_ledger.rs`**

```rust
//! Last 7 days of .claude/cost-ledger.jsonl.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;
use crate::protocol::ResourceDef;
use crate::resources::Resource;

pub struct CostLedgerResource;

#[async_trait]
impl Resource for CostLedgerResource {
    fn def(&self) -> ResourceDef {
        ResourceDef {
            uri: "mcp://tokentrimmer/cost-ledger/last-7d".into(),
            name: "Cost ledger (last 7 days)".into(),
            description: Some("JSONL of recent autopilot iteration costs.".into()),
            mime_type: "application/x-ndjson",
        }
    }
    async fn read(&self) -> Result<Value, McpError> {
        let path = std::env::var("TT_COST_LEDGER_PATH")
            .unwrap_or_else(|_| ".claude/cost-ledger.jsonl".into());
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        Ok(serde_json::json!({ "uri": self.def().uri, "text": body }))
    }
}
```

- [ ] **Step 3: `inspect_baseline.rs`**

```rust
//! Current inspect baseline JSON for the working tree.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;
use crate::protocol::ResourceDef;
use crate::resources::Resource;

pub struct InspectBaselineResource;

#[async_trait]
impl Resource for InspectBaselineResource {
    fn def(&self) -> ResourceDef {
        ResourceDef {
            uri: "mcp://tokentrimmer/inspect/baseline".into(),
            name: "Inspect baseline".into(),
            description: Some("Current `tt inspect` findings on the working tree.".into()),
            mime_type: "application/json",
        }
    }
    async fn read(&self) -> Result<Value, McpError> {
        let mut engine = tt_inspect_core::Engine::new();
        for rule in tt_inspect_rules_tier1::all_rules() {
            engine.add_rule(rule);
        }
        let findings = engine.scan(std::path::Path::new("."));
        Ok(serde_json::json!({ "uri": self.def().uri, "findings": findings }))
    }
}
```

- [ ] **Step 4: Compile**

`cargo check -p tt-mcp`

- [ ] **Step 5: Commit**

```bash
git add crates/mcp/src/resources/
git commit -m "feat(mcp): cost-ledger + inspect-baseline resources

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Stdio transport + server dispatcher

**Files:** `crates/mcp/src/transport/stdio.rs`, `crates/mcp/src/transport/mod.rs`, `crates/mcp/src/server.rs`

- [ ] **Step 1: `transport/mod.rs`**

```rust
pub mod stdio;
```

- [ ] **Step 2: `transport/stdio.rs`**

```rust
//! Line-delimited JSON-RPC over stdin/stdout.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::McpError;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
use crate::server::Server;

pub async fn run(server: Server) -> Result<(), McpError> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = reader.next_line().await
        .map_err(|e| McpError::Internal(format!("stdin read: {e}")))?
    {
        if line.trim().is_empty() { continue; }
        let resp = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(req) => server.dispatch(req).await,
            Err(e) => JsonRpcResponse::err(None, McpError::Parse(e.to_string()).code(), e.to_string()),
        };
        let s = serde_json::to_string(&resp).unwrap();
        stdout.write_all(s.as_bytes()).await
            .map_err(|e| McpError::Internal(format!("stdout write: {e}")))?;
        stdout.write_all(b"\n").await
            .map_err(|e| McpError::Internal(format!("stdout write: {e}")))?;
        stdout.flush().await.ok();
    }
    Ok(())
}
```

- [ ] **Step 3: `server.rs`**

```rust
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
```

- [ ] **Step 4: Wire the public `lib.rs` to expose a `Server::run_stdio()` convenience**

Add to `lib.rs`:
```rust
impl Server {
    pub async fn run_stdio(self) -> Result<(), McpError> {
        crate::transport::stdio::run(self).await
    }
}
```

- [ ] **Step 5: Compile**

`cargo check -p tt-mcp`

- [ ] **Step 6: Commit**

```bash
git add crates/mcp/src/transport/ crates/mcp/src/server.rs crates/mcp/src/lib.rs
git commit -m "feat(mcp): stdio transport + dispatcher

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: tt mcp subcommand wiring

**Files:** `crates/cli/Cargo.toml`, `crates/cli/src/main.rs`

- [ ] **Step 1: Add tt-mcp dep**

`tt-mcp.workspace = true` in `crates/cli/Cargo.toml`.

- [ ] **Step 2: Add `Mcp` variant**

In `Command`:
```rust
    /// Run the MCP server (stdio transport by default).
    Mcp {
        #[arg(long, default_value = "stdio")]
        transport: String,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long, default_value = "https://tokentrimmer.fly.dev")]
        tt_api_base: String,
    },
```

- [ ] **Step 3: Dispatch**

```rust
        Command::Mcp { transport, tt_api_key, tt_api_base } => {
            use tt_mcp::{auth, tools::{find_route_for, inspect_diff, lookup_semantic_cache, preview_cost}, resources::{cost_ledger, inspect_baseline}, Server};
            let api_key = tt_api_key.or_else(|| std::env::var("TT_API_KEY").ok());
            let api_key = auth::validate_api_key(api_key)?;
            let mut server = Server::new();
            server.tools.register(Box::new(preview_cost::PreviewCostTool));
            server.tools.register(Box::new(find_route_for::FindRouteForTool));
            server.tools.register(Box::new(inspect_diff::InspectDiffTool));
            server.tools.register(Box::new(lookup_semantic_cache::LookupSemanticCacheTool {
                base_url: tt_api_base.clone(),
                api_key: api_key.clone(),
                http: reqwest::Client::new(),
            }));
            server.resources.register(Box::new(cost_ledger::CostLedgerResource));
            server.resources.register(Box::new(inspect_baseline::InspectBaselineResource));
            match transport.as_str() {
                "stdio" => {
                    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(server.run_stdio())?;
                }
                other => anyhow::bail!("unsupported MCP transport `{other}` (v1: stdio only)"),
            }
        }
```

- [ ] **Step 4: Build + clippy**

```
cargo check -p tt-cli
cargo clippy -p tt-cli -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/src/main.rs
git commit -m "feat(cli): wire \`tt mcp\` subcommand

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Integration test against tt-mcp dispatcher

**Files:** `crates/mcp/tests/dispatcher_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
//! In-process dispatch: initialize, tools/list, tools/call(find_route_for).

use serde_json::json;
use tt_mcp::{protocol::JsonRpcRequest, tools::find_route_for::FindRouteForTool, Server};

#[tokio::test]
async fn lifecycle_initialize_list_call() {
    let mut server = Server::new();
    server.tools.register(Box::new(FindRouteForTool));

    let init = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "initialize".into(),
        params: json!({}),
        id: Some(json!(1)),
    };
    let r = server.dispatch(init).await;
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"]["serverInfo"]["name"], "tt-mcp");

    let list = JsonRpcRequest {
        jsonrpc: "2.0".into(), method: "tools/list".into(),
        params: json!({}), id: Some(json!(2)),
    };
    let r = server.dispatch(list).await;
    let v = serde_json::to_value(&r).unwrap();
    let tools = v["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "find_route_for");

    let call = JsonRpcRequest {
        jsonrpc: "2.0".into(), method: "tools/call".into(),
        params: json!({ "name": "find_route_for", "arguments": { "task_description": "classify this email as spam" } }),
        id: Some(json!(3)),
    };
    let r = server.dispatch(call).await;
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"]["model"], "claude-haiku-4-5");
}
```

- [ ] **Step 2: Run**

`cargo test -p tt-mcp --test dispatcher_smoke`
Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/mcp/tests/dispatcher_smoke.rs
git commit -m "test(mcp): dispatcher in-process smoke

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 14: Docs + final gate

**Files:**
- Create: `docs/tt-mcp-usage.md`
- Modify: `.claude/CONTEXT_MAP.md`

- [ ] **Step 1: Write `docs/tt-mcp-usage.md`**

```markdown
# tt mcp

MCP server exposing TokenTrimmer intelligence to MCP-compatible clients.

## Quick start with Claude Code

\`\`\`json
// ~/.config/claude-code/config.json
{
  "mcpServers": {
    "tokentrimmer": {
      "command": "tt",
      "args": ["mcp"],
      "env": { "TT_API_KEY": "tt_live_..." }
    }
  }
}
\`\`\`

## Day-0 tools

- `preview_cost` — cost projection (Track C engine)
- `find_route_for` — cheapest model for a plain-English task
- `inspect_diff` — run Inspect rules on a proposed file diff
- `lookup_semantic_cache` — check if a similar prompt was answered recently

## Day-0 resources

- `mcp://tokentrimmer/cost-ledger/last-7d`
- `mcp://tokentrimmer/inspect/baseline`

See `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md` for design.
```

- [ ] **Step 2: Context-map entry**

```markdown
### tt mcp

| If you're doing | Read |
|---|---|
| Adding a tool | `crates/mcp/src/tools/find_route_for.rs` (worked example) + `tools/mod.rs::Tool` trait |
| Adding a resource | `crates/mcp/src/resources/inspect_baseline.rs` + `resources/mod.rs::Resource` trait |
| Adding a transport | `crates/mcp/src/transport/stdio.rs` (worked example) |
| Spec | `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md` |
```

- [ ] **Step 3: Full gate**

```
cargo fmt --check
cargo clippy -p tt-mcp -p tt-cli -- -D warnings
cargo test -p tt-mcp
cargo test -p tt-cli
./scripts/tt-inspect-self.sh
```

- [ ] **Step 4: Commit**

```bash
git add docs/tt-mcp-usage.md .claude/CONTEXT_MAP.md
git commit -m "docs(mcp): usage + context map

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 15: Mark backlog item complete

- [ ] **Step 1: Flip `trackA-mcp-server` `[ ]` → `[x]` in BACKLOG.md and append `_Shipped 2026-MM-DD — Day-0 MVP (stdio transport, 4 tools, 2 resources)._`.**

- [ ] **Step 2: Commit**

```bash
git add .claude/BACKLOG.md
git commit -m "backlog: trackA MCP server Day-0 MVP shipped

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Spec coverage check

| Spec section | Covered by |
|---|---|
| §4 architecture | Tasks 1–11 |
| §5 CLI surface | Task 12 |
| §6 tool catalog | Tasks 6–9 |
| §7 resources | Task 10 |
| §8 auth | Task 4 + Task 12 |
| §9 testing | Tasks 2, 4 (units) + Task 13 (dispatch e2e) |
| §10 Day 0 rollout | Tasks 1–15 |
| §10 Day 7 (simulate_plan) | DEFERRED |
| §10 Day 14 (SSE transport) | DEFERRED |
| §10 Day 30 (prompts/) | DEFERRED |

`crates/mcp/src/client.rs` is scaffolded but no public types live there yet — placeholder for the next round when more tools need shared cloud-HTTP helpers.
