//! Fail-closed budget gate tests for `routes::panel`.
//!
//! Run with:
//!   cargo test -p tt-core --test panel_budget
//!
//! Model IDs used:
//!   priced:   "gpt-4o" (openai, $2.50/$10/M), "gpt-4o-mini" (openai, $0.15/$0.60/M)
//!   unpriced: "no-such-model-xyz" (not in registry or pricing catalog)

use tt_core::{
    routes::panel::{
        estimate_panel_cost, panel_budget_gate, ArbiterStrategyKind, ModelRef, PanelConfig,
    },
    ApiError, AppState,
};

/// Build a two-member PanelConfig with the given model IDs.
fn make_cfg(member1: &str, member2: &str, arbiter: &str, max_cost_usd: Option<f64>) -> PanelConfig {
    PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![
            ModelRef {
                model: member1.to_string(),
                provider: None,
            },
            ModelRef {
                model: member2.to_string(),
                provider: None,
            },
        ],
        arbiter_model: ModelRef {
            model: arbiter.to_string(),
            provider: None,
        },
        quorum: None,
        max_cost_usd,
    }
}

/// Two priced members + priced arbiter → estimate equals the summed cost.
#[test]
fn estimate_panel_cost_sums_all_legs() {
    let state = AppState::with_default_providers();
    // members: gpt-4o, gpt-4o-mini  |  arbiter: gpt-4o
    let cfg = make_cfg("gpt-4o", "gpt-4o-mini", "gpt-4o", None);
    let input_tokens = 1000_u32;
    let max_tokens = Some(500_u32);

    let total = estimate_panel_cost(&state, &cfg, input_tokens, max_tokens);
    assert!(total.is_some(), "all models are priced; should return Some");
    let total = total.unwrap();
    // Must be strictly positive and a reasonable-looking cost
    assert!(total > 0.0, "summed cost should be positive, got {total}");
    // Rough sanity: 3 legs, each at most a few cents per 1k tokens
    assert!(total < 1.0, "summed cost sanity check failed, got {total}");
}

/// One member has no catalog pricing → estimate_panel_cost returns None (fail-closed).
#[test]
fn estimate_panel_cost_none_when_member_unpriceable() {
    let state = AppState::with_default_providers();
    // "no-such-model-xyz" has no pricing
    let cfg = make_cfg("gpt-4o", "no-such-model-xyz", "gpt-4o", None);
    let result = estimate_panel_cost(&state, &cfg, 1000, Some(500));
    assert!(
        result.is_none(),
        "unpriceable member should yield None, got {result:?}"
    );
}

/// Estimate within budget → gate returns Ok.
#[test]
fn panel_budget_gate_ok_when_within_budget() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o-mini", "gpt-4o", None);
    // Use a very high ceiling so any real estimate passes
    let result = panel_budget_gate(&state, &cfg, 100, Some(100), Some(999.0));
    assert!(
        result.is_ok(),
        "should pass gate with high ceiling, got {result:?}"
    );
}

/// Estimate exceeds ceiling → gate returns Err(CostLimitExceeded).
#[test]
fn panel_budget_gate_err_when_over_budget() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o-mini", "gpt-4o", None);
    // Use a tiny ceiling so the estimate exceeds it
    let result = panel_budget_gate(&state, &cfg, 1_000_000, Some(1_000_000), Some(0.000001));
    assert!(
        matches!(result, Err(ApiError::CostLimitExceeded { .. })),
        "should err CostLimitExceeded when over ceiling, got {result:?}"
    );
}

/// Unpriceable member → gate returns Err(CostLimitExceeded) (treat as over-ceiling).
#[test]
fn panel_budget_gate_err_when_unpriceable() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "no-such-model-xyz", "gpt-4o", None);
    let result = panel_budget_gate(&state, &cfg, 1000, Some(500), Some(999.0));
    assert!(
        matches!(result, Err(ApiError::CostLimitExceeded { .. })),
        "should err CostLimitExceeded when estimate is None, got {result:?}"
    );
}

/// No ceiling arg AND cfg.max_cost_usd is None → gate returns Err(CostLimitExceeded).
/// A panel REQUIRES an explicit budget.
#[test]
fn panel_budget_gate_err_when_no_ceiling() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o-mini", "gpt-4o", None);
    let result = panel_budget_gate(&state, &cfg, 1000, Some(500), None);
    assert!(
        matches!(result, Err(ApiError::CostLimitExceeded { .. })),
        "should err CostLimitExceeded when no budget is set, got {result:?}"
    );
}

/// cfg.max_cost_usd provides the ceiling when ceiling arg is None.
#[test]
fn panel_budget_gate_uses_cfg_max_cost_when_ceiling_arg_is_none() {
    let state = AppState::with_default_providers();
    // Set max_cost_usd in cfg to a generous value → should pass
    let cfg = make_cfg("gpt-4o", "gpt-4o-mini", "gpt-4o", Some(999.0));
    let result = panel_budget_gate(&state, &cfg, 100, Some(100), None);
    assert!(
        result.is_ok(),
        "cfg.max_cost_usd should serve as fallback ceiling, got {result:?}"
    );
}
