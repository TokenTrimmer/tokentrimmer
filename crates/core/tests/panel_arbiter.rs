//! Unit-level tests for the arbiter strategy layer of `routes::panel`.
//! Run with:
//!   cargo test -p tt-core --test panel_arbiter

use tt_core::{
    routes::panel::{strategy_for, ArbiterStrategyKind, ModelRef, PanelConfig, PanelDefaults},
    ApiError,
};

// ---------------------------------------------------------------------------
// Helper: build a minimal PanelConfig for a given strategy
// ---------------------------------------------------------------------------

fn cfg_for(strategy: ArbiterStrategyKind) -> PanelConfig {
    PanelConfig::resolve(
        strategy,
        None,
        &PanelDefaults {
            members: vec![ModelRef {
                model: "gpt-4o".to_string(),
                provider: None,
            }],
            arbiter_model: ModelRef {
                model: "gpt-4o".to_string(),
                provider: None,
            },
        },
    )
    .expect("resolve should not fail with non-empty members")
}

// ---------------------------------------------------------------------------
// strategy_for tests
// ---------------------------------------------------------------------------

#[test]
// Asserts the Ok path only; the trait object cannot be downcast to verify the
// concrete type at runtime.
fn strategy_for_synthesize_returns_synthesize() {
    let cfg = cfg_for(ArbiterStrategyKind::Synthesize);
    let strategy = strategy_for(&cfg);
    assert!(
        strategy.is_ok(),
        "expected Ok(Synthesize), got Err: {:?}",
        strategy.err()
    );
}

#[test]
fn strategy_for_best_of_n_is_unsupported() {
    let cfg = cfg_for(ArbiterStrategyKind::BestOfN);
    let result = strategy_for(&cfg);
    match result {
        Err(ApiError::PanelStrategyUnsupported { ref strategy }) if strategy == "best-of-n" => {}
        Err(e) => panic!("expected PanelStrategyUnsupported(best-of-n), got Err({e:?})"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

#[test]
fn strategy_for_majority_is_unsupported() {
    let cfg = cfg_for(ArbiterStrategyKind::Majority);
    let result = strategy_for(&cfg);
    match result {
        Err(ApiError::PanelStrategyUnsupported { ref strategy }) if strategy == "majority" => {}
        Err(e) => panic!("expected PanelStrategyUnsupported(majority), got Err({e:?})"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}
