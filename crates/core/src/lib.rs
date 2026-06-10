//! TokenTrimmer Gateway core: HTTP server, routing, middleware, provider registry.
//!
//! See `docs/04-gateway-api-reference.md` for the public API contract.

pub mod budget;
pub mod db;
pub mod error;
pub mod failover;
pub mod metrics;
pub mod middleware;
pub mod quality_sample;
pub mod registry;
pub mod retry;
pub mod routes;
pub mod server;
pub mod single_flight;
pub mod state;
pub mod tier_resolver;

pub use budget::{
    tier_budget_limits, BudgetDecision, BudgetEnforcer, BudgetLimits, DynamicBudgetEnforcer,
    InMemoryBudgetEnforcer, SpendSink,
};
pub use db::{connect, migrate, migrate_only, MIGRATOR};
pub use error::{ApiError, ApiResult};
pub use failover::{dispatch_with_failover, CircuitBreaker};
pub use middleware::retrieval::RetrievalState;
pub use quality_sample::{
    risk_band_to_preview, spawn_quality_judge, GatewayLlmJudge, JudgeConfig, JudgeOutcome,
    JudgeSink, JudgeTaskClass, QualityJudgeJob, ReferenceSource,
};
pub use registry::{register_providers, ProviderRegistry, ProvidersConfig};
pub use retry::{with_retry, RetryPolicy};
pub use server::{build_router, build_router_with_retrieval};
pub use state::AppState;

/// Fixed org id used in dogfood mode (`TT_DOGFOOD_GROQ_ROUTING=1` with no DB
/// pool). Unauthenticated requests are assigned this identity so the routing
/// engine can match them against the pre-seeded dogfood route.
pub const DOGFOOD_ORG_ID: uuid::Uuid = uuid::uuid!("00000000-0000-0000-0000-00000d0660fd");
