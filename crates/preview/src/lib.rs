//! `tt-preview` — pure cost preview engine.
//!
//! Given a chat-completion-shaped request, returns projected cost on the
//! current model, expected savings if served from cache, and cheaper-
//! equivalent route suggestions with quality risk bands. Performs no LLM
//! calls and no Postgres lookups in the hot path — all enrichment goes
//! through pluggable trait objects so callers can wire org-specific data.
//!
//! See `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md`.

pub mod cache_projection;
pub mod classifier;
pub mod error;
pub mod pricing;
pub mod route_suggestions;
pub mod token_estimator;
pub mod types;

pub use error::PreviewError;
pub use types::{
    CacheProjections, EstimationConfidence, PreviewRequest, PreviewResponse,
    RouteSuggestion, Suggestion,
};
