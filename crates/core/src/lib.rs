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
pub use middleware::retrieval::RetrievalState;
pub use registry::ProviderRegistry;
pub use server::{build_router, build_router_with_retrieval};
pub use state::AppState;

/// Fixed org id used in dogfood mode (`TT_DOGFOOD_GROQ_ROUTING=1` with no DB
/// pool). Unauthenticated requests are assigned this identity so the routing
/// engine can match them against the pre-seeded dogfood route.
pub const DOGFOOD_ORG_ID: uuid::Uuid = uuid::uuid!("00000000-0000-0000-0000-00000d0660fd");
