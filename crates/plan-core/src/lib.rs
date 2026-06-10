//! Plan replay engine. See `docs/03-plan-replay-design.md`.
//!
//! Public surface:
//! - [`replay::replay`] — the deterministic entry point.
//! - Types in [`types`] — `PlanInput`, `PlanResult`, etc.
//! - [`error::PlanError`] — every error this crate can return.
//!
//! Hard invariants enforced by `tests/replay.rs` and `tests/bootstrap.rs`:
//! 1. **Determinism**: same `(historical_rows, proposed_config, seed)` →
//!    bit-identical JSON output.
//! 2. **Bootstrap CI coverage**: synthetic universes with known truth land
//!    inside the 95% CI ~95% of the time.
//! 3. **Conservative cost math**: missing pricing → request counted as
//!    unchanged (never fabricate savings).

#![deny(missing_docs)]

pub mod apply;
pub mod bootstrap;
pub mod cache_projection;
pub mod cost;
pub mod error;
pub mod l2_projection;
pub mod pricing;
pub mod quality;
pub mod replay;
pub mod routing;
pub mod types;

pub use apply::{apply_plan, ApplyError, InMemoryPlanStore, PlanStore};
pub use error::PlanError;
pub use pricing::{catalog_pricing_table, catalog_pricing_table_at};
pub use quality::{
    quality_preserved_summary, quality_score_for_verdict, score_quality, stratified_sample,
    JudgeProvider, JudgeVerdict, MockJudge, QualityConfig, QualityError, QualityPreservedSummary,
    QualityResult, RiskBand, SampleScore,
};
pub use replay::{replay, replay_with_quality};
pub use types::{
    Aggregates, CacheProjection, ConfidenceIntervals, L2Projection, L2SweepResult, ModelPricing,
    PerRouteBreakdown, PlanConfig, PlanInput, PlanResult, PricingTable, ProposedRoute, RequestLog,
    RouteAction, RouteConditions,
};
