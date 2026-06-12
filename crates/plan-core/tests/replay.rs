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
        task_class: Default::default(),
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
            target_model: "claude-3-5-haiku".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
            target_model: "claude-haiku-4-5".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
            target_model: "claude-haiku-4-5".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
            target_model: "claude-3-5-haiku".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: Some(0.01), // haiku on 1M/1M tokens still far exceeds $0.01
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
            target_model: target.into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: true,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
        },
    }
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
            target_model: "cheap-model".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
            task_class: Default::default(),
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
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
fn equal_priority_routes_resolve_deterministically_regardless_of_array_order() {
    // Two routes share priority 100 and BOTH match the same request (the
    // request's model is in each route's `model_in`), but they reroute to
    // DIFFERENT target models with different pricing — so which one wins is
    // observable in the projected savings. Build two configs that are
    // identical except for the order of these two routes in the input array.
    // With a deterministic id-tiebreak the winner (and thus the result) must
    // be identical across both orderings.
    let req = make_req(1, 0, "claude-3-5-sonnet", 1000, 100, 0.0045, false);

    // route_a (lower id) -> haiku; route_b (higher id) -> a pricier target.
    // The id-tiebreak picks the lower id, so route_a must always win.
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
            target_model: "claude-3-5-haiku".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
            target_model: "claude-3-haiku-pricier".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
        },
    };

    let mut pricing = HashMap::new();
    let (k1, v1) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    let (k2, v2) = pricing_with("anthropic", "claude-3-haiku-pricier", 1.0, 5.0);
    pricing.insert(k1, v1);
    pricing.insert(k2, v2);

    // Order 1: [route_a, route_b]
    let input_ab = input_with_routes(
        vec![req.clone()],
        vec![route_a.clone(), route_b.clone()],
        pricing.clone(),
        100,
    );
    // Order 2: [route_b, route_a] — only the array order differs.
    let input_ba = input_with_routes(vec![req], vec![route_b, route_a], pricing, 100);

    let res_ab = replay(input_ab).expect("replay ab must succeed");
    let res_ba = replay(input_ba).expect("replay ba must succeed");

    // The determinism the review wants (§4.12): same config modulo array
    // order -> bit-identical PROJECTION. We compare the projection outputs
    // (aggregates + CIs + per-route breakdown) rather than the whole
    // PlanResult because `proposed_routes` is deliberately echoed in the
    // caller's authored (unsorted) order for apply-path round-trip fidelity —
    // that field is the input document, not the projection. Before the
    // tiebreak, the winner (and so these projection numbers) flipped with the
    // array order; that is the bug being fixed.
    let proj_ab = serde_json::to_string(&(
        &res_ab.aggregates,
        &res_ab.confidence_intervals,
        &res_ab.per_route_breakdown,
    ))
    .expect("serialize ab projection");
    let proj_ba = serde_json::to_string(&(
        &res_ba.aggregates,
        &res_ba.confidence_intervals,
        &res_ba.per_route_breakdown,
    ))
    .expect("serialize ba projection");
    assert_eq!(
        proj_ab, proj_ba,
        "equal-priority route configs that differ only in array order must \
         produce a bit-identical projection"
    );

    // And specifically: the lower-id route (route_a -> haiku) wins in both,
    // independent of which order it was authored in.
    for res in [&res_ab, &res_ba] {
        assert_eq!(res.aggregates.requests_rerouted, 1);
        assert_eq!(res.per_route_breakdown.len(), 1);
        assert_eq!(res.per_route_breakdown[0].route_id, det_uuid(100));
    }
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
