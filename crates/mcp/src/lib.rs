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

impl Server {
    pub async fn run_stdio(self) -> Result<(), McpError> {
        crate::transport::stdio::run(self).await
    }
}
