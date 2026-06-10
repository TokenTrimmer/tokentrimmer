//! `tt-mcp` — Model Context Protocol server.
//!
//! See `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md`.

pub mod auth;
pub mod client;
pub mod cost;
pub mod error;
pub mod protocol;
pub mod resources;
pub mod server;
pub mod tools;
pub mod transport;

pub use error::McpError;
pub use server::Server;

impl Server {
    pub async fn run_stdio(self) -> Result<(), McpError> {
        crate::transport::stdio::run(self).await
    }

    /// Boot the MCP server over the current **Streamable HTTP** transport
    /// (MCP spec 2025-03-26).
    ///
    /// Binds to `addr` and serves a single MCP endpoint at `/mcp` that handles
    /// `POST` (client→server JSON-RPC, with optional SSE-upgraded responses),
    /// `GET` (server→client SSE stream), and `DELETE` (session termination).
    /// Session state is tracked via the `Mcp-Session-Id` header. Runs until a
    /// shutdown signal is received.
    pub async fn run_http(
        self,
        addr: std::net::SocketAddr,
        auth_token: String,
    ) -> Result<(), McpError> {
        crate::transport::http::run(self, addr, auth_token).await
    }

    /// Boot the MCP server over the **deprecated** HTTP+SSE transport
    /// (MCP spec 2024-11-05).
    ///
    /// Binds to `addr` and serves `GET /sse` + `POST /messages?sessionId=…`
    /// until a shutdown signal is received. Prefer [`Server::run_http`] — this
    /// transport is retained only for backward compatibility with older clients.
    pub async fn run_sse(
        self,
        addr: std::net::SocketAddr,
        auth_token: String,
    ) -> Result<(), McpError> {
        crate::transport::sse::run(self, addr, auth_token).await
    }
}
