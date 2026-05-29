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
    }
}

fn pricing_with(provider: &str, model: &str, input: f64, output: f64) -> (String, ModelPricing) {
    (
        format!("{provider}:{model}"),
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: Some(input * 0.1),
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
            target_model: "claude-3-5-haiku".into(),
            force_cache_layer: None,
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
            target_model: "nonexistent-model".into(),
            force_cache_layer: None,
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

fn deterministic_input(n: u32, iterations: u32) -> PlanInput {
    let mut requests = Vec::with_capacity(n as usize);
    for i in 0..n {
        let input_tokens = 100 + (i % 500);
        let output_tokens = 50 + (i % 100);
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
        };
        let req_template = RequestLog {
            id: det_uuid(u128::from(i) + 1),
            org_id: det_uuid(0xfeed_face_cafe),
            ts: ts(i64::from(i) * 7),
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet".into(),
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
            target_model: "claude-3-5-haiku".into(),
            force_cache_layer: None,
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
fn caveat_small_sample_present_under_1000() {
    let input = deterministic_input(50, 100);
    let result = replay(input).unwrap();
    assert!(result
        .caveats
        .iter()
        .any(|c| c.contains("Small sample size")));
}
