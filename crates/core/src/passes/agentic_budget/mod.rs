//! Agentic context budget — the route-grained mode that brings the CLI's
//! loop-aware levers server-side (plan `2026-06-13-agentic-cost-context-budget`,
//! spec `2026-06-13-agentic-cost-reduction-research`).
//!
//! Loop traffic burns tokens re-sending a growing transcript every turn. This
//! module nets four sub-levers per request — they do NOT stack at face value
//! (caching and routing fight; elision busts the prefix it trims from), so the
//! planner reconciles them and books the interactions honestly:
//!
//! - **Sub-lever 1 — [`cache_prefix`]** (lossless): annotate the
//!   `cache_control` breakpoints the caller forgot so the provider's prompt
//!   cache fires on the static + history prefix. Books NO TT savings and NO
//!   bust — pure provider-cache enablement, reported on the separate
//!   `provider_cache_saved_usd` axis (spec §4.3). **Shipped here (Task 4).**
//! - **Sub-lever 2** — field-drop (lossless, token-true-gated) + summarize
//!   (lossy, judge-gated) stale tool results. *(Later tasks.)*
//! - **Sub-lever 3** — down-route mechanical sub-steps in a cache-isolated
//!   lane. *(Later tasks.)*
//! - **Sub-lever 4** — semantic sub-step cache for read-only sub-steps.
//!   *(Later tasks.)*
//!
//! # Off by default (load-bearing)
//!
//! The mode is opt-in via `RouteAction::agentic_budget` (`None` by default,
//! serde-omitted). The default request path — no agentic budget configured —
//! is byte-identical: the planner is never constructed, so it adds no tokens,
//! no headers, and changes no behavior.

pub mod cache_prefix;

pub use cache_prefix::{annotate_cache_prefix, CachePrefixPlan};

/// Orchestrates the agentic-budget sub-levers for a request, netting their
/// interactions before dispatch.
///
/// The planner runs **before** the [`PassPipeline`](crate::passes::PassPipeline):
/// Sub-lever 1 confirms/annotates the cache-prefix breakpoint (it touches the
/// prefix's framing, not content, so it is not a tail-mutating
/// [`RequestPass`](crate::passes::RequestPass)), and the later sub-levers feed
/// the pass stage. It is constructed only for routes that opted into the mode;
/// the default (un-opted) path never builds one, keeping that path
/// byte-identical.
///
/// Task 4 wires Sub-lever 1 only; subsequent tasks attach the remaining
/// levers.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgenticBudgetPlanner;

impl AgenticBudgetPlanner {
    /// A planner with the mode's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}
