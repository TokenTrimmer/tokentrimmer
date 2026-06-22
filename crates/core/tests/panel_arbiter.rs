//! Unit-level tests for the arbiter strategy layer of `routes::panel`.
//! Run with:
//!   cargo test -p tt-core --test panel_arbiter

use tt_core::{
    routes::panel::{
        cosine, strategy_for, surviving_answers, ArbiterDetail, ArbiterStrategyKind, LegResult,
        LegRole, LegStatus, ModelRef, PanelConfig, PanelDefaults,
    },
    ApiError,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    ChatCompletionResponse, Usage,
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

// ---------------------------------------------------------------------------
// cosine helper tests
// ---------------------------------------------------------------------------

#[test]
fn cosine_identical_is_one_orthogonal_zero_zeronorm_zero() {
    assert!(
        (cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6,
        "identical unit vectors should have cosine 1.0"
    );
    assert!(
        cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6,
        "orthogonal vectors should have cosine ~0.0"
    );
    assert_eq!(
        cosine(&[0.0, 0.0], &[1.0, 1.0]),
        0.0,
        "zero-norm vector should return 0.0"
    );
}

// ---------------------------------------------------------------------------
// ArbiterDetail default tests
// ---------------------------------------------------------------------------

#[test]
fn arbiter_detail_default_is_empty() {
    let d = ArbiterDetail::default();
    assert!(d.chosen_leg.is_none() && !d.fell_back && !d.no_majority);
}

// ---------------------------------------------------------------------------
// surviving_answers helper tests
// ---------------------------------------------------------------------------

fn make_leg(pos: usize, role: LegRole, status: LegStatus, text: Option<&str>) -> LegResult {
    let response = text.map(|t| ChatCompletionResponse {
        id: "test".to_string(),
        object: "chat.completion".to_string(),
        created: 0,
        model: "test-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text(t.to_string())),
                name: None,
                tool_calls: vec![],
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: Usage::default(),
    });
    LegResult {
        leg_index: pos,
        role,
        model: "test-model".to_string(),
        provider: "test-provider".to_string(),
        status,
        response,
        cost_usd: None,
        usage: None,
        latency_ms: 0,
    }
}

#[test]
fn surviving_answers_filters_non_ok_and_arbiter_legs() {
    let legs = vec![
        make_leg(0, LegRole::Leg, LegStatus::Ok, Some("answer A")),
        make_leg(
            1,
            LegRole::Leg,
            LegStatus::Error,
            Some("should be excluded"),
        ),
        make_leg(2, LegRole::Arbiter, LegStatus::Ok, Some("arbiter excluded")),
        make_leg(3, LegRole::Leg, LegStatus::Ok, Some("answer B")),
    ];
    let answers = surviving_answers(&legs);
    assert_eq!(answers.len(), 2, "only 2 ok member legs should survive");
    assert_eq!(answers[0].0, 0, "first surviving answer is at position 0");
    assert_eq!(answers[0].1, "answer A");
    assert_eq!(answers[1].0, 3, "second surviving answer is at position 3");
    assert_eq!(answers[1].1, "answer B");
}
