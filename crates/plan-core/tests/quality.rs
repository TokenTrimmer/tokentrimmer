//! Integration tests for Tier 3 LLM-judge quality scoring.
//!
//! Covers the hard invariants from the task spec:
//! 1. Opt-in gate (body logging disabled → error).
//! 2. Budget cap (projected cost > budget → error).
//! 3. Risk band thresholds (5% / 15% boundaries).
//! 4. Deterministic stratified sampling (same seed → same IDs).
//! 5. Proportional stratification (stratum share preserved).
//! 6. NoScorable when all bodies missing.
//! 7. High-Unclear caveat.
//! 8. Reason truncation at 200 chars.

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use tt_plan_core::{
    score_quality, stratified_sample, JudgeProvider, JudgeVerdict, MockJudge, QualityConfig,
    QualityError, RequestLog, RiskBand,
};
use uuid::Uuid;

fn ts(secs: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(secs)
}

fn det_uuid(seed: u128) -> Uuid {
    Uuid::from_u128(seed)
}

/// Build a request with optional body/response so most tests can flip
/// individual rows into / out of the scorable population.
fn req(
    id_seed: u128,
    input_tokens: u32,
    tag: Option<&str>,
    body: Option<&str>,
    response_body: Option<&str>,
) -> RequestLog {
    RequestLog {
        id: det_uuid(id_seed),
        org_id: det_uuid(0xfeed_face_cafe),
        ts: ts(id_seed as i64),
        provider: "anthropic".into(),
        model: "claude-3-5-sonnet".into(),
        input_tokens,
        output_tokens: 100,
        cached_tokens: 0,
        cost_usd: 0.001,
        baseline_cost_usd: 0.001,
        cached: false,
        cache_layer: None,
        matched_route_id: None,
        latency_ms: 100,
        upstream_latency_ms: Some(80),
        status: 200,
        tag: tag.map(String::from),
        embedding: None,
        finish_reason: None,
        body: body.map(String::from),
        response_body: response_body.map(String::from),
    }
}

fn base_config(seed: u64, total_samples: u32) -> QualityConfig {
    QualityConfig {
        body_logging_enabled: true,
        total_samples,
        // Generous budget so OverBudget doesn't fire unless we explicitly opt in.
        budget_usd: 100.0,
        cost_per_judge_call_usd: 0.001,
        seed,
    }
}

/// Build N requests with bodies + responses, evenly tagged, varying sizes.
/// Every request is scorable.
fn scorable_pop(n: u32) -> Vec<RequestLog> {
    (0..n)
        .map(|i| {
            let tag = if i % 2 == 0 { Some("ux") } else { Some("api") };
            let tokens = match i % 3 {
                0 => 200,
                1 => 1500,
                _ => 8000,
            };
            req(
                u128::from(i) + 1,
                tokens,
                tag,
                Some("prompt"),
                Some("original response"),
            )
        })
        .collect()
}

/// Always-acceptable judge.
fn judge_ok() -> MockJudge {
    MockJudge {
        verdict: JudgeVerdict::Acceptable,
        reason: "matches".into(),
    }
}

/// Judge that returns Degraded on the first `degraded_n` calls then
/// Acceptable. Used to dial the degraded share for risk-band tests.
struct SequencedJudge {
    sequence: Mutex<Vec<JudgeVerdict>>,
    cursor: AtomicUsize,
}

impl SequencedJudge {
    fn new(verdicts: Vec<JudgeVerdict>) -> Self {
        Self {
            sequence: Mutex::new(verdicts),
            cursor: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl JudgeProvider for SequencedJudge {
    async fn judge(
        &self,
        _input: &str,
        _orig: &str,
        _prop: &str,
    ) -> Result<(JudgeVerdict, String), QualityError> {
        let idx = self.cursor.fetch_add(1, Ordering::SeqCst);
        let seq = self.sequence.lock().expect("mutex poisoned");
        let v = seq.get(idx).copied().unwrap_or(JudgeVerdict::Acceptable);
        Ok((v, format!("verdict #{idx}")))
    }
}

fn proposed_constant(_id: &Uuid) -> Option<String> {
    Some("proposed response".into())
}

#[tokio::test]
async fn body_logging_disabled_errors() {
    let reqs = scorable_pop(10);
    let mut cfg = base_config(42, 5);
    cfg.body_logging_enabled = false;
    let err = score_quality(&reqs, &cfg, &judge_ok(), proposed_constant)
        .await
        .expect_err("must error when body logging is off");
    assert!(
        matches!(err, QualityError::BodyLoggingDisabled),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn over_budget_errors() {
    let reqs = scorable_pop(10);
    let cfg = QualityConfig {
        body_logging_enabled: true,
        total_samples: 2,
        budget_usd: 0.01,
        cost_per_judge_call_usd: 0.10, // 2 * 0.10 = $0.20 > $0.01
        seed: 1,
    };
    let err = score_quality(&reqs, &cfg, &judge_ok(), proposed_constant)
        .await
        .expect_err("must error when projected cost exceeds budget");
    let QualityError::OverBudget { cost, budget } = err else {
        panic!("expected OverBudget, got {err:?}");
    };
    assert!((cost - 0.20).abs() < 1e-9);
    assert!((budget - 0.01).abs() < 1e-9);
}

#[tokio::test]
async fn all_acceptable_is_low_risk() {
    let reqs = scorable_pop(40);
    let cfg = base_config(7, 30);
    let result = score_quality(&reqs, &cfg, &judge_ok(), proposed_constant)
        .await
        .expect("score must succeed");
    assert_eq!(result.degraded_count, 0);
    assert_eq!(result.risk_band, RiskBand::Low);
    assert!((result.degraded_pct - 0.0).abs() < 1e-9);
}

#[tokio::test]
async fn mostly_degraded_is_high_risk() {
    // 20 samples, 6 Degraded (= 30%) → degraded_pct = 30 → HIGH.
    let reqs = scorable_pop(40);
    let mut verdicts = vec![JudgeVerdict::Degraded; 6];
    verdicts.extend(std::iter::repeat_n(JudgeVerdict::Acceptable, 14));
    let judge = SequencedJudge::new(verdicts);
    let cfg = base_config(7, 20);
    let result = score_quality(&reqs, &cfg, &judge, proposed_constant)
        .await
        .expect("score must succeed");
    assert_eq!(result.sample_size, 20);
    assert_eq!(result.degraded_count, 6);
    assert_eq!(result.acceptable_count, 14);
    assert!((result.degraded_pct - 30.0).abs() < 1e-9);
    assert_eq!(result.risk_band, RiskBand::High);
}

#[tokio::test]
async fn exactly_at_5pct_is_low() {
    // 20 samples, 1 Degraded → 5% → LOW (boundary inclusive).
    let reqs = scorable_pop(40);
    let mut verdicts = vec![JudgeVerdict::Degraded];
    verdicts.extend(std::iter::repeat_n(JudgeVerdict::Acceptable, 19));
    let judge = SequencedJudge::new(verdicts);
    let cfg = base_config(7, 20);
    let result = score_quality(&reqs, &cfg, &judge, proposed_constant)
        .await
        .expect("score must succeed");
    assert_eq!(result.sample_size, 20);
    assert_eq!(result.degraded_count, 1);
    assert!((result.degraded_pct - 5.0).abs() < 1e-9);
    assert_eq!(result.risk_band, RiskBand::Low);
}

#[tokio::test]
async fn exactly_at_15pct_is_medium() {
    // 20 samples, 3 Degraded → 15% → MEDIUM (boundary inclusive).
    let reqs = scorable_pop(40);
    let mut verdicts = vec![JudgeVerdict::Degraded; 3];
    verdicts.extend(std::iter::repeat_n(JudgeVerdict::Acceptable, 17));
    let judge = SequencedJudge::new(verdicts);
    let cfg = base_config(7, 20);
    let result = score_quality(&reqs, &cfg, &judge, proposed_constant)
        .await
        .expect("score must succeed");
    assert_eq!(result.sample_size, 20);
    assert_eq!(result.degraded_count, 3);
    assert!((result.degraded_pct - 15.0).abs() < 1e-9);
    assert_eq!(result.risk_band, RiskBand::Medium);
}

#[test]
fn stratified_sample_is_deterministic_same_seed() {
    let reqs = scorable_pop(200);
    let a = stratified_sample(&reqs, 50, 1234);
    let b = stratified_sample(&reqs, 50, 1234);
    assert_eq!(a, b, "same seed must produce identical sampled IDs");

    // Different seed → different sample (high-probability assertion; with
    // 200 requests and n=50 the chance of collision is astronomical).
    let c = stratified_sample(&reqs, 50, 9999);
    assert_ne!(a, c, "different seed should yield a different draw");
}

#[test]
fn stratified_sample_is_proportional() {
    // 200 small (size 200) and 100 large (size 8000) requests, all tagged
    // "x". With n=30, the two strata get round(200/300 × 30) = 20 small +
    // round(100/300 × 30) = 10 large = 30 total.
    let mut reqs: Vec<RequestLog> = Vec::new();
    for i in 0u32..200 {
        reqs.push(req(u128::from(i) + 1, 200, Some("x"), Some("p"), Some("r")));
    }
    for i in 0u32..100 {
        reqs.push(req(
            u128::from(i) + 10_000,
            8000,
            Some("x"),
            Some("p"),
            Some("r"),
        ));
    }
    let sampled = stratified_sample(&reqs, 30, 42);
    // Group sampled IDs back into strata.
    let by_id: HashMap<Uuid, &RequestLog> = reqs.iter().map(|r| (r.id, r)).collect();
    let mut small = 0u32;
    let mut large = 0u32;
    for id in &sampled {
        let r = by_id.get(id).expect("sampled id must exist");
        if r.input_tokens <= 500 {
            small += 1;
        } else if r.input_tokens > 4000 {
            large += 1;
        }
    }
    assert_eq!(sampled.len(), 30, "should draw exactly 30 (proportional)");
    assert_eq!(small, 20, "small stratum allocation");
    assert_eq!(large, 10, "large stratum allocation");
}

#[tokio::test]
async fn no_scorable_when_bodies_missing() {
    // Every request lacks body + response, so the scorer sees 0 scorable rows.
    let reqs: Vec<RequestLog> = (0u32..10)
        .map(|i| req(u128::from(i) + 1, 200, Some("x"), None, None))
        .collect();
    let cfg = base_config(7, 5);
    let err = score_quality(&reqs, &cfg, &judge_ok(), proposed_constant)
        .await
        .expect_err("must error when no row is scorable");
    assert!(matches!(err, QualityError::NoScorable), "got: {err:?}");
}

#[tokio::test]
async fn high_unclear_share_surfaces_caveat() {
    // 10 samples, all Unclear → 100% Unclear → caveat surfaced.
    let reqs = scorable_pop(20);
    let judge = MockJudge {
        verdict: JudgeVerdict::Unclear,
        reason: "couldn't tell".into(),
    };
    let cfg = base_config(7, 10);
    let result = score_quality(&reqs, &cfg, &judge, proposed_constant)
        .await
        .expect("score must succeed even with all-unclear");
    assert_eq!(result.unclear_count, 10);
    assert_eq!(result.acceptable_count, 0);
    assert_eq!(result.degraded_count, 0);
    // degraded_pct is 0 (no classified samples) → Low band.
    assert_eq!(result.risk_band, RiskBand::Low);
    assert!(
        result.caveats.iter().any(|c| c.contains("Unclear")),
        "expected Unclear caveat, got {:?}",
        result.caveats
    );
}

#[tokio::test]
async fn reason_truncated_to_200_chars() {
    let long_reason = "x".repeat(500);
    let judge = MockJudge {
        verdict: JudgeVerdict::Acceptable,
        reason: long_reason,
    };
    let reqs = scorable_pop(20);
    let cfg = base_config(7, 10);
    let result = score_quality(&reqs, &cfg, &judge, proposed_constant)
        .await
        .expect("score must succeed");
    for s in &result.sampled_examples {
        assert!(
            s.reason.len() <= 200,
            "reason should be ≤200 chars, got {}",
            s.reason.len()
        );
    }
    // And exactly 200 when input was longer.
    let lens: Vec<usize> = result
        .sampled_examples
        .iter()
        .map(|s| s.reason.len())
        .collect();
    assert!(
        lens.iter().all(|&l| l == 200),
        "all reasons should be truncated to 200, got {lens:?}"
    );
}

#[tokio::test]
async fn small_sample_caveat_surfaces_under_30() {
    let reqs = scorable_pop(10);
    let cfg = base_config(7, 5);
    let result = score_quality(&reqs, &cfg, &judge_ok(), proposed_constant)
        .await
        .expect("score must succeed");
    assert!(result.sample_size < 30, "precondition for the caveat test");
    assert!(
        result
            .caveats
            .iter()
            .any(|c| c.contains("Small quality sample")),
        "expected small-sample caveat, got {:?}",
        result.caveats
    );
}

#[tokio::test]
async fn replay_with_quality_attaches_quality_field() {
    use chrono::TimeZone;
    use tt_plan_core::{
        replay_with_quality, PlanInput, ProposedRoute, RouteAction, RouteConditions,
    };

    let requests = scorable_pop(40);
    let route = ProposedRoute {
        id: det_uuid(0xb_eef),
        name: "any".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions::default(),
        then: RouteAction {
            target_model: "claude-3-5-haiku".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
        },
    };
    let mut pricing = HashMap::new();
    pricing.insert(
        "anthropic:claude-3-5-haiku".into(),
        tt_plan_core::ModelPricing {
            input_per_million: 0.25,
            output_per_million: 1.25,
            cached_input_per_million: Some(0.025),
        },
    );
    let input = PlanInput {
        plan_id: det_uuid(0xa11ce),
        org_id: det_uuid(0xfeed_face_cafe),
        window_start: Utc.with_ymd_and_hms(2026, 4, 30, 0, 0, 0).unwrap(),
        window_end: Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
        requests,
        proposed_routes: vec![route],
        pricing,
        config: Default::default(),
        seed: 42,
        bootstrap_iterations: 100,
    };
    let cfg = base_config(7, 20);
    let result = replay_with_quality(input, &judge_ok(), &cfg, proposed_constant)
        .await
        .expect("replay_with_quality must succeed");
    assert!(result.quality.is_some(), "quality field must be populated");
    let q = result.quality.unwrap();
    assert_eq!(q.sample_size, 20);
    assert_eq!(q.risk_band, RiskBand::Low);
}
