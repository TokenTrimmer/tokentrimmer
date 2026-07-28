//! Fail-closed budget gate tests for `routes::panel`.
//!
//! Run with:
//!   cargo test -p tt-core --test panel_budget
//!
//! Model IDs used:
//!   priced:   "gpt-4o" (openai, $2.50/$10/M), "gpt-4o-mini" (openai, $0.15/$0.60/M)
//!   unpriced: "no-such-model-xyz" (not in registry or pricing catalog)

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tt_core::{
    routes::panel::{
        estimate_panel_cost, panel_budget_gate, ArbiterStrategyKind, ModelRef,
        PanelAdmissionEstimate, PanelConfig,
    },
    ApiError, AppState, ProviderRegistry,
};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
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

fn estimate(
    input_tokens: u32,
    max_tokens: Option<u32>,
    max_completion_tokens: Option<u32>,
    n: Option<u32>,
) -> PanelAdmissionEstimate {
    PanelAdmissionEstimate {
        input_tokens,
        max_tokens,
        max_completion_tokens,
        n,
    }
}

/// A provider whose dynamic catalog or surcharge is malformed. Admission must
/// reject before any of its dispatch methods could be called.
struct NonFiniteCostProvider {
    input_per_million: f64,
    fee_multiplier: f64,
}

#[async_trait]
impl Provider for NonFiniteCostProvider {
    fn id(&self) -> &'static str {
        "non-finite"
    }

    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "non-finite-model".to_string(),
            provider: self.id().to_string(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8_192,
            max_output_tokens: 8_192,
        }]
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        (model == "non-finite-model").then(|| ModelPricing {
            input_per_million: self.input_per_million,
            output_per_million: 1.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        })
    }

    fn fee_multiplier(&self) -> f64 {
        self.fee_multiplier
    }

    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported(
            "admission must reject first".to_string(),
        ))
    }

    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Err(ProviderError::Unsupported(
            "admission must reject first".to_string(),
        ))
    }
}

fn state_with_non_finite_cost(input_per_million: f64, fee_multiplier: f64) -> AppState {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(NonFiniteCostProvider {
        input_per_million,
        fee_multiplier,
    }));
    AppState::new(registry)
}

/// Two priced members + priced arbiter → estimate equals the summed cost.
#[test]
fn estimate_panel_cost_sums_all_legs() {
    let state = AppState::with_default_providers();
    // members: gpt-4o, gpt-4o-mini  |  arbiter: gpt-4o
    let cfg = make_cfg("gpt-4o", "gpt-4o-mini", "gpt-4o", None);
    let input_tokens = 1000_u32;
    let admission = estimate(input_tokens, Some(500), None, None);

    let total = estimate_panel_cost(&state, &cfg, admission);
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
    let result = estimate_panel_cost(&state, &cfg, estimate(1000, Some(500), None, None));
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
    let result = panel_budget_gate(
        &state,
        &cfg,
        estimate(100, Some(100), None, None),
        Some(999.0),
    );
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
    let result = panel_budget_gate(
        &state,
        &cfg,
        estimate(1_000_000, Some(1_000_000), None, None),
        Some(0.000001),
    );
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
    let result = panel_budget_gate(
        &state,
        &cfg,
        estimate(1000, Some(500), None, None),
        Some(999.0),
    );
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
    let result = panel_budget_gate(&state, &cfg, estimate(1000, Some(500), None, None), None);
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
    let result = panel_budget_gate(&state, &cfg, estimate(100, Some(100), None, None), None);
    assert!(
        result.is_ok(),
        "cfg.max_cost_usd should serve as fallback ceiling, got {result:?}"
    );
}

/// Even though normal header/config parsers reject these values, the admission
/// helper is also callable by internal code. A non-finite or non-positive raw
/// ceiling must fail closed rather than making `estimate > ceiling` false.
#[test]
fn panel_budget_gate_rejects_invalid_direct_ceiling() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o-mini", "gpt-4o", None);
    let admission = estimate(100, Some(100), None, None);

    for ceiling in [0.0, -0.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(
            panel_budget_gate(&state, &cfg, admission, Some(ceiling)),
            Err(ApiError::CostLimitExceeded { .. })
        ));
    }
}

/// The same defensive validation applies when a direct caller places the bad
/// value on PanelConfig rather than passing a header-style override.
#[test]
fn panel_budget_gate_rejects_invalid_direct_config_budget() {
    let state = AppState::with_default_providers();
    let admission = estimate(100, Some(100), None, None);

    for ceiling in [0.0, -0.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let cfg = make_cfg("gpt-4o", "gpt-4o-mini", "gpt-4o", Some(ceiling));
        assert!(matches!(
            panel_budget_gate(&state, &cfg, admission, None),
            Err(ApiError::CostLimitExceeded { .. })
        ));
    }
}

/// A malformed dynamic pricing record or fee multiplier cannot make the
/// preflight comparison quietly pass through NaN/∞ arithmetic.
#[test]
fn panel_preflight_fails_closed_for_non_finite_cost_inputs() {
    let cfg = make_cfg(
        "non-finite-model",
        "non-finite-model",
        "non-finite-model",
        None,
    );
    let admission = estimate(100, Some(100), None, None);

    for (input_per_million, fee_multiplier) in [(f64::NAN, 1.0), (1.0, f64::INFINITY)] {
        let state = state_with_non_finite_cost(input_per_million, fee_multiplier);
        assert!(
            estimate_panel_cost(&state, &cfg, admission).is_none(),
            "non-finite pricing inputs must not yield an estimate"
        );
        assert!(matches!(
            panel_budget_gate(&state, &cfg, admission, Some(999.0)),
            Err(ApiError::CostLimitExceeded { .. })
        ));
    }
}

/// Synthesize's known runtime shape is not three copies of the original
/// request: it fans every capped member answer into the arbiter prompt and
/// always asks that arbiter for 4,096 output tokens.
#[test]
fn synthesize_preflight_includes_candidate_fan_in_and_fixed_arbiter_output() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o", "gpt-4o", None);
    let admission = estimate(100, Some(100), None, None);

    let planned = estimate_panel_cost(&state, &cfg, admission).expect("all legs are priced");
    // Legacy preflight priced each of the two members plus arbiter as only
    // (100 input + 100 output) at $2.50/$10 per million = $0.00375 total.
    let legacy_flat_estimate = 3.0 * ((100.0 * 2.5 + 100.0 * 10.0) / 1_000_000.0);
    assert!(
        planned > legacy_flat_estimate * 10.0,
        "known Synthesize fan-in/4,096 output must materially exceed the old flat estimate: \
         planned={planned}, legacy={legacy_flat_estimate}"
    );

    let ceiling = legacy_flat_estimate * 2.0;
    assert!(matches!(
        panel_budget_gate(&state, &cfg, admission, Some(ceiling)),
        Err(ApiError::CostLimitExceeded { .. })
    ));
}

/// `max_completion_tokens` is the effective output cap when both OpenAI-shaped
/// fields are present, so a small legacy cap cannot make the panel admission
/// estimate undercount a materially larger dispatched allowance.
#[test]
fn panel_preflight_prefers_max_completion_tokens() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o", "gpt-4o", None);
    let legacy_only = estimate(100, Some(1), None, None);
    let completion_cap = estimate(100, Some(1), Some(1_000), None);

    let legacy = estimate_panel_cost(&state, &cfg, legacy_only).expect("priced legacy plan");
    let planned =
        estimate_panel_cost(&state, &cfg, completion_cap).expect("priced completion-cap plan");
    assert!(
        planned > legacy,
        "max_completion_tokens must raise the admission estimate: {planned} <= {legacy}"
    );
    let ceiling = (legacy + planned) / 2.0;
    assert!(matches!(
        panel_budget_gate(&state, &cfg, completion_cap, Some(ceiling)),
        Err(ApiError::CostLimitExceeded { .. })
    ));
}

/// A multi-choice member call bills every requested completion even though the
/// arbiter consumes only its first candidate. The static plan must include that
/// known output multiplication instead of admitting it as a one-choice panel.
#[test]
fn panel_preflight_accounts_for_multiple_member_choices() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o", "gpt-4o", None);
    let one_choice = estimate(100, Some(100), None, Some(1));
    let two_choices = estimate(100, Some(100), None, Some(2));

    let one = estimate_panel_cost(&state, &cfg, one_choice).expect("priced one-choice plan");
    let two = estimate_panel_cost(&state, &cfg, two_choices).expect("priced two-choice plan");
    assert!(
        two > one,
        "two choices must cost more than one: {two} <= {one}"
    );
    let ceiling = (one + two) / 2.0;
    assert!(matches!(
        panel_budget_gate(&state, &cfg, two_choices, Some(ceiling)),
        Err(ApiError::CostLimitExceeded { .. })
    ));
}

/// `n=0` has no valid dispatch semantics, so the static plan fails closed
/// before it can turn a malformed provider request into a zero-cost admission.
#[test]
fn panel_preflight_fails_closed_for_zero_choices() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o", "gpt-4o", None);
    let zero_choices = estimate(100, Some(100), None, Some(0));

    assert!(estimate_panel_cost(&state, &cfg, zero_choices).is_none());
    assert!(matches!(
        panel_budget_gate(&state, &cfg, zero_choices, Some(999.0)),
        Err(ApiError::CostLimitExceeded { .. })
    ));
}

/// A provider-selected output limit has no static upper bound, so a budgeted
/// panel must supply either OpenAI-shaped output cap before admission can pass.
#[test]
fn panel_preflight_requires_an_explicit_effective_output_cap() {
    let state = AppState::with_default_providers();
    let cfg = make_cfg("gpt-4o", "gpt-4o", "gpt-4o", None);
    let missing_cap = estimate(100, None, None, None);

    assert!(estimate_panel_cost(&state, &cfg, missing_cap).is_none());
    assert!(matches!(
        panel_budget_gate(&state, &cfg, missing_cap, Some(999.0)),
        Err(ApiError::CostLimitExceeded { .. })
    ));
}

/// Majority includes an embedding pass, but PanelConfig has no embedding
/// pricing contract. It therefore cannot be honestly admitted against a hard
/// static budget merely by pricing the LLM fields.
#[test]
fn majority_preflight_fails_closed_without_embedding_pricing() {
    let state = AppState::with_default_providers();
    let mut cfg = make_cfg("gpt-4o", "gpt-4o", "gpt-4o", None);
    cfg.strategy = ArbiterStrategyKind::Majority;
    let admission = estimate(100, Some(100), None, None);

    assert!(estimate_panel_cost(&state, &cfg, admission).is_none());
    assert!(matches!(
        panel_budget_gate(&state, &cfg, admission, Some(999.0)),
        Err(ApiError::CostLimitExceeded { .. })
    ));
}
