//! Integration tests for the Plan replay engine. The big invariants:
//! determinism (bit-identical JSON across re-runs), conservative cost math
//! (missing pricing → counted as unchanged), and per-route sums matching
//! aggregates.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use tt_plan_core::{
    cost, replay, ModelPricing, PlanInput, PricingTable, ProposedRoute, RequestLog, RouteAction,
    RouteConditions,
};
use uuid::Uuid;

fn ts(secs: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(secs)
}

/// Build a deterministic UUID from a small integer. Using `Uuid::from_u128`
/// rather than `Uuid::new_v4()` so the snapshot test produces stable IDs.
fn det_uuid(seed: u128) -> Uuid {
    Uuid::from_u128(seed)
}

fn make_req(
    id_seed: u128,
    secs: i64,
    model: &str,
    input_tokens: u32,
    output_tokens: u32,
    baseline_cost: f64,
    cached: bool,
) -> RequestLog {
    RequestLog {
        id: det_uuid(id_seed),
        org_id: det_uuid(0xfeed_face_cafe),
        ts: ts(secs),
        provider: "anthropic".into(),
        model: model.into(),
        requested_model: Some(model.into()),
        input_tokens,
        output_tokens,
        cached_tokens: 0,
        cost_usd: baseline_cost,
        baseline_cost_usd: baseline_cost,
        cached,
        cache_layer: if cached { Some("l1".into()) } else { None },
        matched_route_id: None,
        latency_ms: 100,
        upstream_latency_ms: Some(80),
        status: 200,
        tag: None,
        embedding: None,
        finish_reason: None,
        body: None,
        response_body: None,
        task_class: Default::default(),
        diff_saved_usd: None,
        minify_saved_est_usd: None,
    }
}

fn pricing_with(provider: &str, model: &str, input: f64, output: f64) -> (String, ModelPricing) {
    (
        format!("{provider}:{model}"),
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: Some(input * 0.1),
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        },
    )
}

fn input_with_routes(
    requests: Vec<RequestLog>,
    routes: Vec<ProposedRoute>,
    pricing: PricingTable,
    iterations: u32,
) -> PlanInput {
    PlanInput {
        plan_id: det_uuid(0xa11ce),
        org_id: det_uuid(0xfeed_face_cafe),
        window_start: ts(-1),
        window_end: ts(10_000),
        requests,
        proposed_routes: routes,
        pricing,
        config: Default::default(),
        seed: 42,
        bootstrap_iterations: iterations,
    }
}

#[test]
fn empty_input_yields_zero_aggregates() {
    let input = input_with_routes(vec![], vec![], HashMap::new(), 100);
    let result = replay(input).expect("replay must succeed with empty input");
    assert_eq!(result.sample_size, 0);
    assert_eq!(result.aggregates.total_baseline_cost_usd, 0.0);
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
    assert_eq!(result.aggregates.requests_rerouted, 0);
    assert_eq!(result.aggregates.requests_unchanged, 0);
    assert_eq!(result.aggregates.requests_unprice_able, 0);
    assert_eq!(result.confidence_intervals.savings_usd_95, (0.0, 0.0));
    assert!(result.per_route_breakdown.is_empty());
}

#[test]
fn invalid_window_errors() {
    let input = PlanInput {
        plan_id: det_uuid(1),
        org_id: det_uuid(2),
        window_start: ts(100),
        window_end: ts(50),
        requests: vec![],
        proposed_routes: vec![],
        pricing: HashMap::new(),
        config: Default::default(),
        seed: 1,
        bootstrap_iterations: 100,
    };
    assert!(matches!(
        replay(input),
        Err(tt_plan_core::PlanError::InvalidWindow { .. })
    ));
}

#[test]
fn zero_iterations_errors() {
    let input = PlanInput {
        plan_id: det_uuid(1),
        org_id: det_uuid(2),
        window_start: ts(0),
        window_end: ts(100),
        requests: vec![],
        proposed_routes: vec![],
        pricing: HashMap::new(),
        config: Default::default(),
        seed: 1,
        bootstrap_iterations: 0,
    };
    assert!(matches!(
        replay(input),
        Err(tt_plan_core::PlanError::ZeroBootstrapIterations)
    ));
}

#[test]
fn cache_only_diff_yields_savings() {
    // Two identical requests within the proposed L1 TTL: the second is a
    // projected cache hit. Adding caching with NO model change must therefore
    // yield projected savings > 0. Before cache hits were wired into the cost
    // loop this reported $0 (hits updated only the hit-rate, never the cost).
    let r1 = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let r2 = make_req(2, 30, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let input = PlanInput {
        plan_id: det_uuid(0xa11ce),
        org_id: det_uuid(0xfeed_face_cafe),
        window_start: ts(-1),
        window_end: ts(10_000),
        requests: vec![r1, r2],
        proposed_routes: vec![],
        pricing: HashMap::new(),
        config: tt_plan_core::PlanConfig {
            l1_ttl_seconds: Some(60),
            ..Default::default()
        },
        seed: 42,
        bootstrap_iterations: 100,
    };
    let result = replay(input).unwrap();
    assert!(
        result.aggregates.cache_hit_rate_projected > 0.0,
        "expected a projected cache hit"
    );
    assert!(
        result.aggregates.projected_savings_usd > 0.0,
        "cache-only diff should yield savings > 0, got {}",
        result.aggregates.projected_savings_usd
    );
}

#[test]
fn single_request_no_route_match_zero_savings() {
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.005, false);
    let input = input_with_routes(vec![req], vec![], HashMap::new(), 100);
    let result = replay(input).unwrap();
    assert_eq!(result.aggregates.requests_unchanged, 1);
    assert_eq!(result.aggregates.requests_rerouted, 0);
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
    assert_eq!(result.aggregates.projected_savings_pct, 0.0);
}

#[test]
fn single_request_route_match_cheaper_model_produces_savings() {
    // Sonnet pricing: $3/M input, $15/M output. Baseline cost for 1000 in /
    // 100 out = $0.003 + $0.0015 = $0.0045.
    // Haiku pricing: $0.25/M input, $1.25/M output. Projected = $0.00025 +
    // $0.000125 = $0.000375.
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "cheap-for-short".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["claude-3-5-sonnet".into()],
            ..Default::default()
        },
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("claude-3-5-haiku".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    pricing.insert(k, v);

    let input = input_with_routes(vec![req], vec![route], pricing, 100);
    let result = replay(input).unwrap();
    assert_eq!(result.aggregates.requests_rerouted, 1);
    assert_eq!(result.aggregates.requests_unchanged, 0);
    assert!(result.aggregates.total_projected_cost_usd < result.aggregates.total_baseline_cost_usd);
    assert!(result.aggregates.projected_savings_usd > 0.0);
}

#[test]
fn conservative_when_pricing_missing() {
    // Route to a target_model that has no pricing entry — request must be
    // counted as unchanged (no savings fabricated).
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "mystery-model".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions::default(),
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("nonexistent-model".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };
    let input = input_with_routes(vec![req], vec![route], HashMap::new(), 100);
    let result = replay(input).unwrap();
    assert_eq!(result.aggregates.requests_unprice_able, 1);
    assert_eq!(result.aggregates.requests_rerouted, 0);
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
    // Caveat surfaced to user.
    assert!(result
        .caveats
        .iter()
        .any(|c| c.contains("no pricing entry")));
}

#[test]
fn cross_provider_route_prices_target_by_its_own_provider() {
    // openai/gpt-4o request, routed to anthropic/claude-haiku-4-5. Both priced.
    // Must reroute and project savings (not count unprice_able).
    let mut req = make_req(1, 0, "gpt-4o", 1000, 100, 0.0045, false);
    req.provider = "openai".into();
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "x-provider".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        },
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("claude-haiku-4-5".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-haiku-4-5", 0.25, 1.25);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    assert_eq!(result.aggregates.requests_rerouted, 1);
    assert_eq!(result.aggregates.requests_unprice_able, 0);
    assert!(result.aggregates.projected_savings_usd > 0.0);
}

#[test]
fn cross_provider_target_absent_is_conservative() {
    // Same cross-provider route but the target pricing is missing entirely.
    let mut req = make_req(1, 0, "gpt-4o", 1000, 100, 0.0045, false);
    req.provider = "openai".into();
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "x-provider".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        },
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("claude-haiku-4-5".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };
    let result = replay(input_with_routes(
        vec![req],
        vec![route],
        HashMap::new(),
        100,
    ))
    .unwrap();
    assert_eq!(result.aggregates.requests_unprice_able, 1);
    assert_eq!(result.aggregates.requests_rerouted, 0);
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
}

#[test]
fn route_over_ceiling_is_blocked_not_saved() {
    // A request routed to a model whose projected cost exceeds max_cost_usd is
    // counted unchanged (no fabricated savings) and surfaced as a would-block.
    let req = make_req(1, 0, "claude-3-5-sonnet", 1_000_000, 1_000_000, 18.0, false);
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "capped".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions::default(),
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("claude-3-5-haiku".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: Some(0.01), // haiku on 1M/1M tokens still far exceeds $0.01
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    // Blocked → projected unchanged (no savings), and a caveat names it.
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
    assert!(result.caveats.iter().any(|c| c.contains("rejected")));
}

/// Build a `batch: true` route from `model_in` → `target` (no other actions).
fn batch_route(target: &str, model_in: &str) -> ProposedRoute {
    ProposedRoute {
        id: det_uuid(100),
        name: "batch-lane".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec![model_in.into()],
            ..Default::default()
        },
        then: RouteAction {
            format_switch: None,
            diff: false,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            target_model: Some(target.into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: true,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            ..Default::default()
        },
    }
}

#[test]
fn dispatch_actions_without_retained_runtime_evidence_fail_projection_closed() {
    let base = batch_route("cheap-model", "source-model");
    let cases = vec![
        (
            "traffic_pct",
            RouteAction {
                traffic_pct: Some(50),
                ..base.then.clone()
            },
        ),
        (
            "shadow_model",
            RouteAction {
                shadow_model: Some("shadow-model".into()),
                ..base.then.clone()
            },
        ),
        (
            "auto_pause",
            RouteAction {
                auto_pause: true,
                ..base.then.clone()
            },
        ),
        (
            "agentic_budget",
            RouteAction {
                agentic_budget: Some(tt_routing::AgenticBudget::default()),
                ..base.then.clone()
            },
        ),
        (
            "panel",
            RouteAction {
                panel: Some(tt_routing::RoutePanel {
                    strategy: "majority".into(),
                    ..Default::default()
                }),
                ..base.then.clone()
            },
        ),
        (
            "workflow",
            RouteAction {
                workflow: Some(tt_routing::RouteWorkflow {
                    workflow_id: "00000000-0000-0000-0000-000000000001".into(),
                    ..Default::default()
                }),
                ..base.then.clone()
            },
        ),
    ];

    for (field, action) in cases {
        let first = make_req(1, 0, "source-model", 1_000, 100, 0.0045, false);
        let second = make_req(2, 30, "source-model", 1_000, 100, 0.0045, false);
        let mut route = base.clone();
        route.then = action;
        let mut pricing = HashMap::new();
        let (key, value) = pricing_with("anthropic", "cheap-model", 0.25, 1.25);
        pricing.insert(key, value);
        let mut input = input_with_routes(vec![first, second], vec![route], pricing, 100);
        input.config.l1_ttl_seconds = Some(60);

        let result = replay(input)
            .expect("unsupported effective action must fail only its projection closed");
        assert_eq!(result.aggregates.requests_rerouted, 0, "{field}");
        assert_eq!(result.aggregates.requests_unchanged, 2, "{field}");
        assert_eq!(result.aggregates.projected_savings_usd, 0.0, "{field}");
        assert!(
            (result.aggregates.total_projected_cost_usd - 0.009).abs() < 1e-12,
            "{field}"
        );
        assert!(
            result.caveats.iter().any(|caveat| {
                caveat.contains(field) && caveat.contains("fail cost/latency projection closed")
            }),
            "missing {field} caveat: {:?}",
            result.caveats
        );
        assert!(
            result.caveats.iter().any(|caveat| {
                caveat.contains("2 request(s)") && caveat.contains("counted unchanged")
            }),
            "missing suppressed-request caveat for {field}: {:?}",
            result.caveats
        );
    }
}

#[test]
fn disable_cache_action_prevents_projected_route_cache_savings() {
    let first = make_req(1, 0, "source-model", 1_000, 100, 0.0045, false);
    let second = make_req(2, 30, "source-model", 1_000, 100, 0.0045, false);
    let mut route = batch_route("source-model", "source-model");
    route.then.batch = false;
    route.then.disable_cache = true;
    let mut pricing = HashMap::new();
    let (key, value) = pricing_with("anthropic", "source-model", 3.0, 15.0);
    pricing.insert(key, value);
    let mut input = input_with_routes(vec![first, second], vec![route], pricing, 100);
    input.config.l1_ttl_seconds = Some(60);

    let result = replay(input).expect("disable_cache is exactly projectable");
    assert_eq!(result.aggregates.requests_rerouted, 2);
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
    assert!((result.aggregates.total_projected_cost_usd - 0.009).abs() < 1e-12);
}

/// A `batch: true` route whose target carries catalog batch rates projects the
/// per-request cost at the BATCH rate (the discount the Batch Lane would
/// deliver), folds the delta into projected savings, and surfaces the advisory
/// caveat with the affected-request count — the EXACT honest wording.
#[test]
fn batch_route_projects_discount_and_caveats() {
    // 1000 in / 100 out on sonnet, baseline $0.0045. Target haiku:
    //   standard = 1000×$0.25/M + 100×$1.25/M = 0.00025 + 0.000125 = 0.000375
    //   batch    = 1000×$0.125/M + 100×$0.625/M = 0.000125 + 0.0000625 = 0.0001875
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let route = batch_route("claude-3-5-haiku", "claude-3-5-sonnet");
    let mut pricing = HashMap::new();
    let (k, mut v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    v.batch_input_per_million = Some(0.125);
    v.batch_output_per_million = Some(0.625);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    assert_eq!(result.aggregates.requests_rerouted, 1);
    let batch_cost = 1000.0 * 0.125 / 1e6 + 100.0 * 0.625 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - batch_cost).abs() < 1e-12,
        "projected ({}) must be the batch-rate cost ({batch_cost})",
        result.aggregates.total_projected_cost_usd
    );
    assert!(
        (result.aggregates.projected_savings_usd - (0.0045 - batch_cost)).abs() < 1e-12,
        "savings must include the batch delta"
    );
    let expected_caveat = "1 request(s) projected at the target's Batch API rate via a \
                           batch-eligibility route — advisory today: the synchronous gateway \
                           defers this discount until the async Batch Lane ships, and logs \
                           carry no streamed/interactive marker, so this count can include \
                           traffic the runtime gate would clear as batch-ineligible.";
    assert!(
        result.caveats.iter().any(|c| c == expected_caveat),
        "expected the advisory batch caveat, got: {:?}",
        result.caveats
    );
}

/// A `batch: true` route whose target has NO catalog batch rate projects the
/// STANDARD target cost (no fabricated 0.5×) and surfaces the unpriced caveat.
#[test]
fn batch_route_without_rate_projects_standard_with_unpriced_caveat() {
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let route = batch_route("claude-3-5-haiku", "claude-3-5-sonnet");
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    // pricing_with carries no batch rates — the target is batch-unpriced.
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    let standard_cost = 1000.0 * 0.25 / 1e6 + 100.0 * 1.25 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - standard_cost).abs() < 1e-12,
        "projected ({}) must be the STANDARD target cost ({standard_cost})",
        result.aggregates.total_projected_cost_usd
    );
    assert!(
        result.caveats.iter().any(|c| c
            == "1 request(s) matched a batch-eligibility route whose target has no catalog \
                batch rate — no batch discount projected."),
        "expected the unpriced batch caveat, got: {:?}",
        result.caveats
    );
}

/// When the cache-discounted STANDARD projection is already cheaper than the
/// full-prompt batch-rate cost (heavy provider-cache traffic: cached input is
/// priced ~0.1×, while the batch rate prices the FULL prompt), the standard
/// figure stands AND the batch-deferred counter does not increment — the
/// caveat must never claim a request was projected at the batch rate when it
/// was not.
#[test]
fn batch_route_with_cheaper_standard_projection_skips_counter() {
    // 1M input fully provider-cached, 0 output. Target haiku:
    //   standard = 1M × $0.025/M (cached rate)  = $0.025
    //   batch    = 1M × $0.125/M (full prompt)  = $0.125  → no discount
    let req = RequestLog {
        cached_tokens: 1_000_000,
        ..make_req(1, 0, "claude-3-5-sonnet", 1_000_000, 0, 3.0, false)
    };
    let route = batch_route("claude-3-5-haiku", "claude-3-5-sonnet");
    let mut pricing = HashMap::new();
    let (k, mut v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    v.batch_input_per_million = Some(0.125);
    v.batch_output_per_million = Some(0.625);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    assert_eq!(result.aggregates.requests_rerouted, 1);
    let standard_cached_cost = 1_000_000.0 * 0.025 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - standard_cached_cost).abs() < 1e-12,
        "projected ({}) must keep the cheaper cache-discounted standard cost ({standard_cached_cost})",
        result.aggregates.total_projected_cost_usd
    );
    assert!(
        !result.caveats.iter().any(|c| c.contains("Batch API rate")),
        "a request the batch rate did not discount must not be counted in the batch caveat: {:?}",
        result.caveats
    );
}

/// The `max_cost_usd` ceiling is evaluated on the STANDARD projected cost
/// (runtime dispatches synchronously — the unrealized batch discount can't
/// rescue an over-ceiling request): a blocked request is counted unchanged
/// with the would-block caveat and NO batch discount; a projected cache hit
/// stays 0-projected with no batch counter increment.
#[test]
fn batch_respects_max_cost_ceiling() {
    // Standard haiku cost on 1M/1M tokens = $1.50, over the $0.01 ceiling —
    // even though the batch-rate cost ($0.75) would also exceed it, the point
    // is the ceiling fires FIRST, on the standard figure.
    let req = make_req(1, 0, "claude-3-5-sonnet", 1_000_000, 1_000_000, 18.0, false);
    let mut route = batch_route("claude-3-5-haiku", "claude-3-5-sonnet");
    route.then.max_cost_usd = Some(0.01);
    let mut pricing = HashMap::new();
    let (k, mut v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    v.batch_input_per_million = Some(0.125);
    v.batch_output_per_million = Some(0.625);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    // Blocked → projected unchanged (no savings), would-block caveat, and NO
    // batch caveat (the discount must not be projected on a blocked request).
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
    assert!(result.caveats.iter().any(|c| c.contains("rejected")));
    assert!(
        !result.caveats.iter().any(|c| c.contains("Batch API rate")),
        "a blocked request must not project a batch discount: {:?}",
        result.caveats
    );
}

/// A projected L1 cache hit on a batch route serves for free (projected 0)
/// and does NOT increment the batch counter — only the live-dispatch request
/// is counted in the advisory caveat.
#[test]
fn batch_cache_hit_stays_zero_projected_without_counter() {
    // Two identical requests 10s apart with a 1h L1 TTL: the second projects
    // as a cache hit. Both match the batch route.
    let r1 = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let r2 = make_req(2, 10, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let route = batch_route("claude-3-5-haiku", "claude-3-5-sonnet");
    let mut pricing = HashMap::new();
    let (k, mut v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    v.batch_input_per_million = Some(0.125);
    v.batch_output_per_million = Some(0.625);
    pricing.insert(k, v);

    let mut input = input_with_routes(vec![r1, r2], vec![route], pricing, 100);
    input.config.l1_ttl_seconds = Some(3600);
    let result = replay(input).unwrap();

    // Projected total = ONE batch-rate dispatch + one free cache hit.
    let batch_cost = 1000.0 * 0.125 / 1e6 + 100.0 * 0.625 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - batch_cost).abs() < 1e-12,
        "cache hit must project 0; got total {}",
        result.aggregates.total_projected_cost_usd
    );
    // The caveat counts exactly 1 batch-deferred request — not the cache hit.
    assert!(
        result
            .caveats
            .iter()
            .any(|c| c.starts_with("1 request(s) projected at the target's Batch API rate")),
        "cache hits must not increment the batch counter: {:?}",
        result.caveats
    );
}

/// Build a `flex: true` route from `model_in` → `target` (no other actions).
fn flex_route(target: &str, model_in: &str) -> ProposedRoute {
    let mut r = batch_route(target, model_in);
    r.name = "flex-lane".into();
    r.then.batch = false;
    r.then.flex = true;
    r
}

/// A `flex: true` route whose served model carries catalog Flex rates projects
/// the per-request cost at the FLEX rate (a REALIZED discount), folds the delta
/// into projected savings, and surfaces the realized-flex caveat.
#[test]
fn flex_route_projects_discount_and_caveats() {
    // 1000 in / 100 out on sonnet, baseline $0.0045. Target haiku:
    //   standard = 1000×$0.25/M + 100×$1.25/M = 0.000375
    //   flex     = 1000×$0.125/M + 100×$0.625/M = 0.0001875
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let route = flex_route("claude-3-5-haiku", "claude-3-5-sonnet");
    let mut pricing = HashMap::new();
    let (k, mut v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    v.flex_input_per_million = Some(0.125);
    v.flex_output_per_million = Some(0.625);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    assert_eq!(result.aggregates.requests_rerouted, 1);
    let flex_cost = 1000.0 * 0.125 / 1e6 + 100.0 * 0.625 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - flex_cost).abs() < 1e-12,
        "projected ({}) must be the flex-rate cost ({flex_cost})",
        result.aggregates.total_projected_cost_usd
    );
    assert!(
        (result.aggregates.projected_savings_usd - (0.0045 - flex_cost)).abs() < 1e-12,
        "savings must include the flex delta"
    );
    assert!(
        result
            .caveats
            .iter()
            .any(|c| c.contains("Flex service-tier rate") && c.contains("REALIZED")),
        "expected the realized-flex caveat, got: {:?}",
        result.caveats
    );
}

/// A `flex: true` route whose served model has NO catalog Flex rate projects
/// the STANDARD target cost (no fabricated 0.5×) and surfaces the inapplicable
/// caveat.
#[test]
fn flex_route_without_rate_projects_standard_with_inapplicable_caveat() {
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    let route = flex_route("claude-3-5-haiku", "claude-3-5-sonnet");
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    // pricing_with carries no flex rates — the served model is flex-inapplicable.
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    let standard_cost = 1000.0 * 0.25 / 1e6 + 100.0 * 1.25 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - standard_cost).abs() < 1e-12,
        "projected ({}) must be the STANDARD target cost ({standard_cost})",
        result.aggregates.total_projected_cost_usd
    );
    assert!(
        result
            .caveats
            .iter()
            .any(|c| c.contains("no Flex service-tier rate")),
        "expected the flex-inapplicable caveat, got: {:?}",
        result.caveats
    );
}

/// A **modifier-only route** (`target_model: None`) keeps the caller's chosen
/// model — replay must price the request at its OWN model (model-rewrite delta
/// = 0) and apply ONLY the route's modifier levers. Here a `flex: true`
/// modifier-only route's served model (the original `claude-3-5-sonnet`) carries
/// catalog Flex rates, so the entire projected saving comes from the flex lever,
/// never a model swap. Also exercises that gateway JSON LACKING `target_model`
/// deserializes to `None` and projects the same way.
#[test]
fn modifier_only_route_keeps_model_and_projects_only_modifier_savings() {
    // sonnet @ 1000 in / 100 out, baseline $0.0045 (standard 3.0/15.0 per M).
    //   standard (== request's own model) = 1000×3.0/M + 100×15.0/M = 0.0045
    //   flex                              = 1000×1.5/M + 100×7.5/M  = 0.00225
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);

    // A modifier-only flex route: deserialize from gateway JSON that OMITS the
    // `target_model` key, proving it lands as `None` (not a defaulted "").
    let action: RouteAction =
        serde_json::from_str(r#"{"flex":true}"#).expect("modifier-only action must deserialize");
    assert_eq!(
        action.target_model, None,
        "omitted target_model must deserialize to None (modifier-only route)"
    );
    assert!(action.flex);
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "flex-modifier-only".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["claude-3-5-sonnet".into()],
            ..Default::default()
        },
        then: action,
    };

    let mut pricing = HashMap::new();
    let (k, mut v) = pricing_with("anthropic", "claude-3-5-sonnet", 3.0, 15.0);
    v.flex_input_per_million = Some(1.5);
    v.flex_output_per_million = Some(7.5);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    // The request still "reroutes" through the route (the modifier applies), but
    // the served model is unchanged, so the projection is the flex rate of the
    // ORIGINAL model — never a model-swap cost.
    assert_eq!(result.aggregates.requests_rerouted, 1);
    let flex_cost = 1000.0 * 1.5 / 1e6 + 100.0 * 7.5 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - flex_cost).abs() < 1e-12,
        "projected ({}) must be the served (original) model's flex cost ({flex_cost})",
        result.aggregates.total_projected_cost_usd
    );
    // The entire saving is the flex modifier delta — the model-rewrite delta is
    // exactly 0 (standard projection == baseline for the unchanged model).
    assert!(
        (result.aggregates.projected_savings_usd - (0.0045 - flex_cost)).abs() < 1e-12,
        "savings must be ONLY the flex-modifier delta, not a model swap; got {}",
        result.aggregates.projected_savings_usd
    );
}

/// A `diff: true` route surfaces the measured `diff_saved_usd` carried on its
/// matched requests as a per-lever caveat — WITHOUT folding it into the
/// projected cost (the realized saving is already in baseline−projected, so
/// re-folding would double-count).
#[test]
fn diff_route_attributes_realized_savings_without_double_counting() {
    let mut req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    req.diff_saved_usd = Some(0.0012);
    let mut route = batch_route("claude-3-5-haiku", "claude-3-5-sonnet");
    route.then.batch = false;
    route.then.diff = true;
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    // Projected cost is the plain model-swap cost — diff is NOT folded on top.
    let standard_cost = 1000.0 * 0.25 / 1e6 + 100.0 * 1.25 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - standard_cost).abs() < 1e-12,
        "diff savings must NOT be folded into the projected cost; got {}",
        result.aggregates.total_projected_cost_usd
    );
    assert!(
        result
            .caveats
            .iter()
            .any(|c| c.contains("measured output-diff savings") && c.contains("0.001200")),
        "expected the realized-diff attribution caveat, got: {:?}",
        result.caveats
    );
}

/// A `minify_json: true` route surfaces the estimated `minify_saved_est_usd` as
/// a labeled-estimate caveat, never folded into the projected headline.
#[test]
fn minify_route_attributes_estimate_without_folding() {
    let mut req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);
    req.minify_saved_est_usd = Some(0.0003);
    let mut route = batch_route("claude-3-5-haiku", "claude-3-5-sonnet");
    route.then.batch = false;
    route.then.minify_json = true;
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    let standard_cost = 1000.0 * 0.25 / 1e6 + 100.0 * 1.25 / 1e6;
    assert!(
        (result.aggregates.total_projected_cost_usd - standard_cost).abs() < 1e-12,
        "minify estimate must NOT be folded into the projected cost; got {}",
        result.aggregates.total_projected_cost_usd
    );
    assert!(
        result
            .caveats
            .iter()
            .any(|c| c.contains("ESTIMATED whitespace savings") && c.contains("0.000300")),
        "expected the estimated-minify caveat, got: {:?}",
        result.caveats
    );
}

#[test]
fn rerouted_latency_projected_from_target_model_history() {
    // 3 premium (900ms) + 3 cheap (100ms) requests; route premium -> cheap.
    // The cheap model HAS history in the window, so rerouted premium requests
    // should project the cheap model's latency (~100ms), not echo 900ms.
    let mut requests = Vec::new();
    for i in 0..3u128 {
        requests.push(RequestLog {
            latency_ms: 900,
            ..make_req(i * 2 + 1, i as i64, "premium-model", 1000, 100, 0.01, false)
        });
    }
    for i in 0..3u128 {
        requests.push(RequestLog {
            latency_ms: 100,
            ..make_req(
                i * 2 + 2,
                (i as i64) + 10,
                "cheap-model",
                1000,
                100,
                0.001,
                false,
            )
        });
    }
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "premium-to-cheap".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["premium-model".into()],
            ..Default::default()
        },
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("cheap-model".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };
    let mut pricing = HashMap::new();
    let (k1, v1) = pricing_with("anthropic", "premium-model", 10.0, 30.0);
    let (k2, v2) = pricing_with("anthropic", "cheap-model", 0.5, 1.5);
    pricing.insert(k1, v1);
    pricing.insert(k2, v2);

    let input = input_with_routes(requests, vec![route], pricing, 100);
    let result = replay(input).unwrap();
    assert_eq!(result.aggregates.requests_rerouted, 3);
    // Rerouted requests now carry the target model's (~100ms) latency, so the
    // projected p50 is ~100ms, not the premium model's 900ms.
    assert!(
        result.aggregates.p50_latency_ms_projected <= 150.0,
        "p50 projected latency should reflect the cheap target model (~100ms), got {}",
        result.aggregates.p50_latency_ms_projected
    );
}

fn deterministic_input(n: u32, iterations: u32) -> PlanInput {
    let mut requests = Vec::with_capacity(n as usize);
    for i in 0..n {
        let input_tokens = 100 + (i % 500);
        let output_tokens = 50 + (i % 100);
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        };
        let req_template = RequestLog {
            id: det_uuid(u128::from(i) + 1),
            org_id: det_uuid(0xfeed_face_cafe),
            ts: ts(i64::from(i) * 7),
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet".into(),
            requested_model: Some("claude-3-5-sonnet".into()),
            input_tokens,
            output_tokens,
            cached_tokens: 0,
            cost_usd: 0.0,
            baseline_cost_usd: 0.0,
            cached: i % 5 == 0,
            cache_layer: None,
            matched_route_id: None,
            latency_ms: 100 + (i % 200),
            upstream_latency_ms: Some(80),
            status: 200,
            tag: if i % 3 == 0 { Some("ux".into()) } else { None },
            embedding: None,
            finish_reason: None,
            body: None,
            response_body: None,
            task_class: Default::default(),
            diff_saved_usd: None,
            minify_saved_est_usd: None,
        };
        let baseline = cost::compute_baseline_cost(&req_template, &pricing);
        requests.push(RequestLog {
            cost_usd: baseline,
            baseline_cost_usd: baseline,
            ..req_template
        });
    }

    let route = ProposedRoute {
        id: det_uuid(0xb_eef),
        name: "cheap-for-short".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            input_tokens_lt: Some(300),
            ..Default::default()
        },
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("claude-3-5-haiku".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };

    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    pricing.insert(k, v);

    PlanInput {
        plan_id: det_uuid(0xa11ce),
        org_id: det_uuid(0xfeed_face_cafe),
        window_start: ts(-1),
        window_end: ts(1_000_000),
        requests,
        proposed_routes: vec![route],
        pricing,
        config: Default::default(),
        seed: 1337,
        bootstrap_iterations: iterations,
    }
}

#[test]
fn determinism_simple_input_bit_identical_json() {
    let a = replay(deterministic_input(100, 100)).unwrap();
    let b = replay(deterministic_input(100, 100)).unwrap();
    let ja = serde_json::to_string(&a).unwrap();
    let jb = serde_json::to_string(&b).unwrap();
    assert_eq!(ja, jb, "same input must yield bit-identical JSON");
}

#[test]
fn determinism_with_nontrivial_bootstrap_iterations() {
    let a = replay(deterministic_input(100, 1000)).unwrap();
    let b = replay(deterministic_input(100, 1000)).unwrap();
    let ja = serde_json::to_string(&a).unwrap();
    let jb = serde_json::to_string(&b).unwrap();
    assert_eq!(
        ja, jb,
        "bootstrap with iterations=1000 must be deterministic"
    );
}

#[test]
fn per_route_breakdown_sums_match_aggregates() {
    let result = replay(deterministic_input(200, 200)).unwrap();
    let sum_baseline: f64 = result
        .per_route_breakdown
        .iter()
        .map(|b| b.baseline_cost_usd)
        .sum();
    let sum_projected: f64 = result
        .per_route_breakdown
        .iter()
        .map(|b| b.projected_cost_usd)
        .sum();

    // Aggregates over ALL requests = per-route sums (over matched) +
    // unchanged cost. Easier to assert: per-route baseline+projected sums
    // equal the rerouted slice of the aggregates.
    let rerouted = u64::from(result.aggregates.requests_rerouted);
    let total_count: u64 = rerouted
        + u64::from(result.aggregates.requests_unchanged + result.aggregates.requests_unprice_able);
    assert_eq!(total_count, u64::from(result.sample_size));

    // Per-route baseline + projected must be subsets of the aggregates totals.
    // For requests not matched by any route, the projected cost equals the
    // baseline `cost_usd`, so per-route totals strictly bound the difference.
    assert!(sum_baseline <= result.aggregates.total_baseline_cost_usd + 1e-9);
    assert!(sum_projected <= result.aggregates.total_projected_cost_usd + 1e-9);

    // Per-route savings sum equals top-line savings when every matched
    // request's baseline cost was strictly greater than its projected cost.
    let per_route_savings: f64 = result
        .per_route_breakdown
        .iter()
        .map(|b| b.savings_usd)
        .sum();
    assert!((per_route_savings - result.aggregates.projected_savings_usd).abs() < 1e-9);
}

#[test]
fn snapshot_canned_replay() {
    // 50-request canned input with deterministic IDs + a fixed seed.
    // Everything in PlanResult is derived from the input — no fields need
    // redaction.
    let input = deterministic_input(50, 200);
    let result = replay(input).expect("snapshot replay must succeed");
    insta::assert_json_snapshot!(result);
}

#[test]
fn equal_priority_overlapping_routes_fail_without_live_store_order() {
    // Historical Plan inputs do not carry routes.created_at, while the live
    // store uses creation order for equal priorities. Both routes can match,
    // so UUID order would fabricate a winner and projected savings.
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);

    let route_a = ProposedRoute {
        id: det_uuid(100),
        name: "route-a".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["claude-3-5-sonnet".into()],
            ..Default::default()
        },
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("claude-3-5-haiku".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };
    let route_b = ProposedRoute {
        id: det_uuid(200),
        name: "route-b".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["claude-3-5-sonnet".into()],
            ..Default::default()
        },
        then: RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("claude-3-haiku-pricier".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };

    let input = input_with_routes(vec![req], vec![route_a, route_b], HashMap::new(), 100);

    assert!(matches!(
        replay(input),
        Err(tt_plan_core::PlanError::AmbiguousRoutePriority {
            priority: 100,
            first_route_id,
            second_route_id,
        }) if first_route_id == det_uuid(100) && second_route_id == det_uuid(200)
    ));
}

#[test]
fn equal_priority_disjoint_model_routes_remain_projectable() {
    let req = make_req(1, 0, "model-a", 1000, 100, 0.0045, false);
    let route_a = batch_route("cheap-a", "model-a");
    let route_b = batch_route("cheap-b", "model-b");
    let route_a_id = route_a.id;
    let mut pricing = HashMap::new();
    let (key, value) = pricing_with("anthropic", "cheap-a", 0.25, 1.25);
    pricing.insert(key, value);

    let result = replay(input_with_routes(
        vec![req],
        vec![route_b, route_a],
        pricing,
        100,
    ))
    .expect("disjoint equal-priority routes do not need a tie winner");

    assert_eq!(result.aggregates.requests_rerouted, 1);
    assert_eq!(result.per_route_breakdown[0].route_id, route_a_id);
}

#[test]
fn missing_requested_model_fails_closed_and_is_counted_in_caveats() {
    let mut req = make_req(1, 0, "served-model", 1000, 100, 0.0045, false);
    req.requested_model = None;
    let mut route = batch_route("cheap-model", "served-model");
    route.then.batch = false;
    let result = replay(input_with_routes(
        vec![req],
        vec![route],
        HashMap::new(),
        100,
    ))
    .expect("legacy model coverage must fail closed, not fail replay");

    assert_eq!(result.aggregates.requests_rerouted, 0);
    assert_eq!(result.aggregates.requests_unchanged, 1);
    assert!(result.caveats.iter().any(|caveat| {
        caveat.contains("1 request(s) lack the exact pre-routing requested_model")
            && caveat.contains("final served model is not substituted")
    }));
}

#[test]
fn historical_condition_proxies_and_unavailable_features_are_explicit() {
    let req = make_req(1, 0, "model-a", 1000, 100, 0.0045, false);
    let mut route = batch_route("cheap-model", "model-a");
    route.then.batch = false;
    route.when.input_tokens_lt = Some(2_000);
    route.when.estimated_cost_gt = Some(0.001);
    route.when.has_images = Some(false);
    route.when.prompt_contains_any_of = vec!["private".into()];

    let result = replay(input_with_routes(
        vec![req],
        vec![route],
        HashMap::new(),
        100,
    ))
    .expect("unavailable features conservatively prevent only the route match");

    assert_eq!(result.aggregates.requests_rerouted, 0);
    assert!(result
        .caveats
        .iter()
        .any(|caveat| caveat.contains("input_tokens as an APPROXIMATE proxy")));
    assert!(result
        .caveats
        .iter()
        .any(|caveat| caveat.contains("baseline_cost_usd as an APPROXIMATE proxy")));
    assert!(result.caveats.iter().any(|caveat| {
        caveat.contains("has_images, prompt_contains_any_of") && caveat.contains("fail closed")
    }));
}

#[test]
fn caveat_small_sample_present_under_1000() {
    let input = deterministic_input(50, 100);
    let result = replay(input).unwrap();
    assert!(result
        .caveats
        .iter()
        .any(|c| c.contains("Small sample size")));
}
