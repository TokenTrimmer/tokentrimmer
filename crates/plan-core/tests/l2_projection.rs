//! Integration tests for the L2 (semantic) cache projection module. The
//! module-level docs in `src/l2_projection.rs` describe the algorithm; this
//! file pins down its observable behaviour:
//!
//! - empty / no-embedding short-circuits,
//! - per-`(provider, model)` isolation,
//! - TTL eviction,
//! - threshold sensitivity (4 thresholds, hits monotonic non-increasing),
//! - cache-poisoning heuristic on divergent outcomes,
//! - byte-for-byte determinism.

use chrono::{DateTime, TimeZone, Utc};
use tt_plan_core::{l2_projection::project_l2_hits, PlanConfig, RequestLog};
use uuid::Uuid;

fn ts(secs: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(secs)
}

fn det_uuid(seed: u128) -> Uuid {
    Uuid::from_u128(seed)
}

/// Build a `RequestLog` carrying the minimum the L2 projection cares about
/// — `(provider, model, ts, embedding, finish_reason, output_tokens)` —
/// with everything else set to harmless defaults.
fn make_req(
    id_seed: u128,
    secs: i64,
    provider: &str,
    model: &str,
    embedding: Option<Vec<f32>>,
    finish_reason: Option<&str>,
    output_tokens: u32,
) -> RequestLog {
    RequestLog {
        id: det_uuid(id_seed),
        org_id: det_uuid(0xfeed_face_cafe),
        ts: ts(secs),
        provider: provider.into(),
        model: model.into(),
        input_tokens: 100,
        output_tokens,
        cached_tokens: 0,
        cost_usd: 0.0,
        baseline_cost_usd: 0.0,
        cached: false,
        cache_layer: None,
        matched_route_id: None,
        latency_ms: 0,
        upstream_latency_ms: None,
        status: 200,
        tag: None,
        embedding,
        finish_reason: finish_reason.map(String::from),
        body: None,
        response_body: None,
        task_class: Default::default(),
        diff_saved_usd: None,
        minify_saved_est_usd: None,
    }
}

/// Default config for the L2 tests: TTL 60s, full 4-threshold sweep.
fn cfg() -> PlanConfig {
    PlanConfig {
        l1_ttl_seconds: None,
        l2_threshold_sweep: vec![0.85, 0.90, 0.92, 0.95],
        l2_ttl_seconds: Some(60),
    }
}

// --------------------------------------------------------------------- (1)

#[test]
fn empty_input_yields_empty_sweep() {
    let result = project_l2_hits(&[], &cfg());
    assert!(result.per_threshold.is_empty());
    assert_eq!(result.poisoning_candidates, 0);
}

// --------------------------------------------------------------------- (2)

#[test]
fn no_embeddings_short_circuits_to_empty_sweep() {
    let reqs = vec![
        make_req(1, 0, "anthropic", "claude", None, None, 10),
        make_req(2, 1, "anthropic", "claude", None, None, 10),
    ];
    let result = project_l2_hits(&reqs, &cfg());
    assert!(
        result.per_threshold.is_empty(),
        "L2 must short-circuit when no request carries an embedding"
    );
    assert_eq!(result.poisoning_candidates, 0);
}

// --------------------------------------------------------------------- (3)

#[test]
fn single_request_no_candidates_zero_hits_all_thresholds() {
    let reqs = vec![make_req(
        1,
        0,
        "anthropic",
        "claude",
        Some(vec![1.0, 0.0, 0.0]),
        None,
        10,
    )];
    let result = project_l2_hits(&reqs, &cfg());
    assert_eq!(result.per_threshold.len(), 4);
    for proj in &result.per_threshold {
        assert_eq!(proj.total, 1);
        assert_eq!(proj.projected_l2_hits, 0);
        assert_eq!(proj.projected_l2_hit_rate, 0.0);
    }
    assert_eq!(result.poisoning_candidates, 0);
}

// --------------------------------------------------------------------- (4)

#[test]
fn two_identical_requests_within_ttl_hit_at_every_threshold() {
    let emb = vec![1.0_f32, 0.0, 0.0];
    let reqs = vec![
        make_req(
            1,
            0,
            "anthropic",
            "claude",
            Some(emb.clone()),
            Some("stop"),
            10,
        ),
        make_req(2, 30, "anthropic", "claude", Some(emb), Some("stop"), 10),
    ];
    let result = project_l2_hits(&reqs, &cfg());
    for proj in &result.per_threshold {
        assert_eq!(proj.total, 2);
        assert_eq!(
            proj.projected_l2_hits, 1,
            "identical embeddings (cos=1.0) must hit at threshold {}",
            proj.threshold
        );
        assert!((proj.projected_l2_hit_rate - 0.5).abs() < 1e-12);
    }
    // Identical outcomes (same finish_reason, same output_tokens) → no
    // poisoning candidate even though we hit at every threshold.
    assert_eq!(result.poisoning_candidates, 0);
}

// --------------------------------------------------------------------- (5)

#[test]
fn pair_with_cosine_about_0_93_hits_low_thresholds_only() {
    // Cherry-pick a pair whose cosine is roughly 0.93 — a 2D vector pair
    // separated by ~21.79 degrees: (1, 0) and (cos21.79, sin21.79).
    // cos(21.79deg) ≈ 0.9285. Sit comfortably between 0.92 and 0.95.
    let theta: f32 = 21.79_f32.to_radians();
    let a = vec![1.0_f32, 0.0];
    let b = vec![theta.cos(), theta.sin()];
    // sanity: confirm cosine sits in the 0.92..0.95 band.
    let dot: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    let sim = dot / (na * nb);
    assert!(
        (0.921..0.949).contains(&sim),
        "fixture pair must sit in (0.921, 0.949); got {sim}",
    );

    let reqs = vec![
        make_req(1, 0, "anthropic", "claude", Some(a), Some("stop"), 10),
        make_req(2, 30, "anthropic", "claude", Some(b), Some("stop"), 10),
    ];
    let result = project_l2_hits(&reqs, &cfg());

    let by_threshold = |t: f32| {
        result
            .per_threshold
            .iter()
            .find(|p| (p.threshold - t).abs() < 1e-6)
            .expect("threshold present")
    };
    assert_eq!(by_threshold(0.85).projected_l2_hits, 1);
    assert_eq!(by_threshold(0.90).projected_l2_hits, 1);
    assert_eq!(by_threshold(0.92).projected_l2_hits, 1);
    assert_eq!(by_threshold(0.95).projected_l2_hits, 0);
}

// --------------------------------------------------------------------- (6)

#[test]
fn requests_outside_ttl_window_do_not_hit() {
    let emb = vec![1.0_f32, 0.0];
    let reqs = vec![
        make_req(
            1,
            0,
            "anthropic",
            "claude",
            Some(emb.clone()),
            Some("stop"),
            10,
        ),
        // 120s gap, TTL is 60s → second request must miss.
        make_req(2, 120, "anthropic", "claude", Some(emb), Some("stop"), 10),
    ];
    let result = project_l2_hits(&reqs, &cfg());
    for proj in &result.per_threshold {
        assert_eq!(
            proj.projected_l2_hits, 0,
            "TTL-expired entry must not hit at threshold {}",
            proj.threshold
        );
    }
    assert_eq!(result.poisoning_candidates, 0);
}

// --------------------------------------------------------------------- (7)

#[test]
fn different_provider_model_are_isolated() {
    let emb = vec![1.0_f32, 0.0];
    let reqs = vec![
        make_req(
            1,
            0,
            "anthropic",
            "claude",
            Some(emb.clone()),
            Some("stop"),
            10,
        ),
        // Same embedding but different model → must NOT hit.
        make_req(
            2,
            5,
            "anthropic",
            "haiku",
            Some(emb.clone()),
            Some("stop"),
            10,
        ),
        // Same embedding but different provider → must NOT hit either.
        make_req(3, 10, "openai", "claude", Some(emb), Some("stop"), 10),
    ];
    let result = project_l2_hits(&reqs, &cfg());
    for proj in &result.per_threshold {
        assert_eq!(proj.total, 3);
        assert_eq!(
            proj.projected_l2_hits, 0,
            "per-(provider, model) isolation broken at threshold {}",
            proj.threshold
        );
    }
}

// --------------------------------------------------------------------- (8)

#[test]
fn divergent_finish_reason_flags_poisoning_candidate() {
    // Same embedding (cos=1.0 → hits at every threshold), divergent
    // finish_reason → every hit counts as a poisoning candidate.
    let emb = vec![1.0_f32, 0.0, 0.0];
    let reqs = vec![
        make_req(
            1,
            0,
            "anthropic",
            "claude",
            Some(emb.clone()),
            Some("stop"),
            10,
        ),
        make_req(2, 30, "anthropic", "claude", Some(emb), Some("length"), 10),
    ];
    let result = project_l2_hits(&reqs, &cfg());

    // 4 thresholds × 1 hit each = 4 projected hits across the sweep.
    let total_hits: u32 = result
        .per_threshold
        .iter()
        .map(|p| p.projected_l2_hits)
        .sum();
    assert_eq!(total_hits, 4);
    // Each threshold reports its OWN poisoning count (the single divergent
    // request poisons at every threshold it hits → 1 per threshold).
    for p in &result.per_threshold {
        assert_eq!(p.poisoning_candidates, 1);
    }
    // Aggregate dedups the single distinct candidate request → 1, NOT the
    // old cross-sweep sum of 4.
    assert_eq!(result.poisoning_candidates, 1);
}

#[test]
fn divergent_output_tokens_flags_poisoning_candidate() {
    // Same embedding, same finish_reason, but output_tokens diverge well
    // beyond max(20, 100/4) = 25 → poisoning candidate.
    let emb = vec![1.0_f32, 0.0, 0.0];
    let reqs = vec![
        make_req(
            1,
            0,
            "anthropic",
            "claude",
            Some(emb.clone()),
            Some("stop"),
            100,
        ),
        make_req(2, 30, "anthropic", "claude", Some(emb), Some("stop"), 200),
    ];
    let result = project_l2_hits(&reqs, &cfg());
    let total_hits: u32 = result
        .per_threshold
        .iter()
        .map(|p| p.projected_l2_hits)
        .sum();
    assert_eq!(total_hits, 4);
    // Per-threshold each row owns its count; aggregate dedups to the single
    // distinct candidate request (was 4 under the old summed behaviour).
    for p in &result.per_threshold {
        assert_eq!(p.poisoning_candidates, 1);
    }
    assert_eq!(result.poisoning_candidates, 1);
}

// --------------------------------------------------------------------- (9)

#[test]
fn threshold_monotonicity_over_synthetic_corpus() {
    // 50-request synthetic set: alternating "cluster A" and "cluster B"
    // unit vectors offset by small angles so hit rates differ across
    // thresholds. Same (provider, model) for all so the bucket fills up.
    let mut reqs = Vec::with_capacity(50);
    for i in 0..50_u32 {
        // Offset the vector by a small angle per request — about 1 deg.
        // Within a single cluster pairs stay close, across clusters they
        // drift further apart.
        let cluster_offset = if i % 2 == 0 { 0.0_f32 } else { 0.10_f32 };
        let angle: f32 = (cluster_offset + (i as f32) * 0.0001).to_radians();
        let emb = vec![angle.cos(), angle.sin()];
        reqs.push(make_req(
            u128::from(i) + 1,
            i64::from(i) * 5,
            "anthropic",
            "claude",
            Some(emb),
            Some("stop"),
            50,
        ));
    }
    // Wider TTL so no eviction.
    let mut config = cfg();
    config.l2_ttl_seconds = Some(1_000_000);
    let result = project_l2_hits(&reqs, &config);
    assert_eq!(result.per_threshold.len(), 4);

    // Pull hit counts in sweep order.
    let hits: Vec<u32> = result
        .per_threshold
        .iter()
        .map(|p| p.projected_l2_hits)
        .collect();
    assert!(
        hits[0] >= hits[1] && hits[1] >= hits[2] && hits[2] >= hits[3],
        "hit counts must be non-increasing as threshold rises: {hits:?}",
    );

    // Sanity: the lowest threshold should pick up at least some hits.
    assert!(
        hits[0] > 0,
        "synthetic corpus should produce some hits at threshold 0.85; got {hits:?}",
    );
}

// --------------------------------------------------------------------- determinism

#[test]
fn determinism_serialized_byte_identical() {
    // Mixed corpus: some hits, some misses, some without embeddings.
    let emb1 = vec![1.0_f32, 0.0];
    let emb2 = vec![0.0_f32, 1.0];
    let emb3 = vec![0.9_f32, 0.4358899]; // ~cos 0.9; sits at ~0.9
    let reqs = vec![
        make_req(
            1,
            0,
            "anthropic",
            "claude",
            Some(emb1.clone()),
            Some("stop"),
            10,
        ),
        make_req(
            2,
            5,
            "anthropic",
            "claude",
            Some(emb1.clone()),
            Some("stop"),
            10,
        ),
        make_req(3, 10, "anthropic", "haiku", Some(emb1), Some("stop"), 10),
        make_req(
            4,
            15,
            "anthropic",
            "claude",
            Some(emb2.clone()),
            Some("length"),
            50,
        ),
        make_req(
            5,
            20,
            "anthropic",
            "claude",
            Some(emb2),
            Some("length"),
            100,
        ),
        make_req(6, 25, "anthropic", "claude", Some(emb3), Some("stop"), 10),
        make_req(7, 30, "anthropic", "claude", None, None, 10),
    ];
    let config = cfg();
    let a = project_l2_hits(&reqs, &config);
    let b = project_l2_hits(&reqs, &config);
    // L2SweepResult is not Serialize itself (no need — we expose its parts
    // through Aggregates), so compare the public components directly.
    assert_eq!(a.poisoning_candidates, b.poisoning_candidates);
    let ja = serde_json::to_string(&a.per_threshold).unwrap();
    let jb = serde_json::to_string(&b.per_threshold).unwrap();
    assert_eq!(
        ja, jb,
        "L2 projection must produce byte-identical output across runs"
    );
}

#[test]
fn determinism_input_permutation_safe() {
    // Same set, presented in two different input orders — sort-by-(ts, id)
    // makes the projection invariant.
    let emb = vec![1.0_f32, 0.0, 0.0];
    let r1 = make_req(
        1,
        0,
        "anthropic",
        "claude",
        Some(emb.clone()),
        Some("stop"),
        10,
    );
    let r2 = make_req(
        2,
        5,
        "anthropic",
        "claude",
        Some(emb.clone()),
        Some("stop"),
        10,
    );
    let r3 = make_req(3, 10, "anthropic", "claude", Some(emb), Some("stop"), 10);

    let forward = vec![r1.clone(), r2.clone(), r3.clone()];
    let reverse = vec![r3, r2, r1];
    let config = cfg();
    let a = project_l2_hits(&forward, &config);
    let b = project_l2_hits(&reverse, &config);
    assert_eq!(a.poisoning_candidates, b.poisoning_candidates);
    assert_eq!(
        serde_json::to_string(&a.per_threshold).unwrap(),
        serde_json::to_string(&b.per_threshold).unwrap(),
    );
}
