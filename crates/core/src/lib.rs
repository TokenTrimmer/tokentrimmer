//! TokenTrimmer Gateway core: HTTP server, routing, middleware, provider registry.
//!
//! See `docs/04-gateway-api-reference.md` for the public API contract.

pub mod db;
pub mod error;
pub mod middleware;
pub mod registry;
pub mod routes;
pub mod server;
pub mod state;

pub use db::{connect, migrate, MIGRATOR};
pub use error::{ApiError, ApiResult};
pub use registry::ProviderRegistry;
pub use server::build_router;
pub use state::AppState;
