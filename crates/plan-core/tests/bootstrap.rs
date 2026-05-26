//! Validation tests for the bootstrap module. The heavy one is a Monte
//! Carlo coverage check: with synthetic data whose true mean is known, the
//! 95% CI must cover that mean in ~95% of trials (target band 93–97%).

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use tt_plan_core::bootstrap::bootstrap_ci;

/// Box-Muller sample from N(mean, sd^2). Pure helper so the test doesn't
/// depend on `rand_distr`.
fn rand_normal(rng: &mut ChaCha8Rng, mean: f64, sd: f64) -> f64 {
    // Both u1 and u2 in (0, 1].
    let u1: f64 = 1.0 - rng.gen::<f64>();
    let u2: f64 = 1.0 - rng.gen::<f64>();
    let z = (-2.0_f64 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
    mean + sd * z
}

#[test]
fn determinism_same_seed_bit_identical() {
    let samples: Vec<f64> = (0..200).map(|i| (i as f64) * 0.123 + 0.5).collect();
    let a = bootstrap_ci(&samples, 9_999, 2_000, (0.025, 0.975));
    let b = bootstrap_ci(&samples, 9_999, 2_000, (0.025, 0.975));
    assert_eq!(a, b);
}

#[test]
fn ci_coverage_monte_carlo() {
    // 200 synthetic universes; in each draw 100 samples from N(1, 0.5^2);
    // compute the 95% bootstrap CI of the mean; count how many CIs cover
    // the true mean of 1.0. Coverage rate should be in [0.93, 0.97].
    let true_mean = 1.0_f64;
    let sd = 0.5_f64;
    let n_samples_per_trial = 100;
    let n_trials = 200;
    let bootstrap_iters = 500;

    // Outer RNG produces independent inner seeds per trial.
    let mut outer_rng = ChaCha8Rng::seed_from_u64(0xC0FFEE);

    let mut covered = 0_u32;
    for _trial in 0..n_trials {
        let trial_seed: u64 = outer_rng.gen();
        let mut sample_rng = ChaCha8Rng::seed_from_u64(trial_seed);
        let samples: Vec<f64> = (0..n_samples_per_trial)
            .map(|_| rand_normal(&mut sample_rng, true_mean, sd))
            .collect();
        let bootstrap_seed: u64 = outer_rng.gen();
        let (lo, hi) = bootstrap_ci(&samples, bootstrap_seed, bootstrap_iters, (0.025, 0.975));
        if lo <= true_mean && true_mean <= hi {
            covered += 1;
        }
    }
    let rate = f64::from(covered) / f64::from(n_trials);
    eprintln!("ci_coverage_monte_carlo: covered {covered}/{n_trials} = {rate}");
    assert!(
        (0.93..=0.97).contains(&rate),
        "coverage rate {rate} outside [0.93, 0.97] over {n_trials} trials",
    );
}

#[test]
fn ci_covers_truth_for_constant_samples() {
    // All samples equal to 5.0 — CI must contain 5.0 exactly.
    let samples = vec![5.0_f64; 50];
    let (lo, hi) = bootstrap_ci(&samples, 1, 500, (0.025, 0.975));
    assert!((lo - 5.0).abs() < 1e-9);
    assert!((hi - 5.0).abs() < 1e-9);
}

#[test]
fn ci_widens_as_n_shrinks() {
    // With more samples the CI should narrow. Compare CI widths for
    // n=20 vs n=500 sampled from the same N(0, 1).
    let mut rng = ChaCha8Rng::seed_from_u64(11);
    let small: Vec<f64> = (0..20).map(|_| rand_normal(&mut rng, 0.0, 1.0)).collect();
    let large: Vec<f64> = (0..500).map(|_| rand_normal(&mut rng, 0.0, 1.0)).collect();
    let (lo_s, hi_s) = bootstrap_ci(&small, 1, 1000, (0.025, 0.975));
    let (lo_l, hi_l) = bootstrap_ci(&large, 1, 1000, (0.025, 0.975));
    assert!(
        (hi_s - lo_s) > (hi_l - lo_l),
        "small-n CI width {} should exceed large-n CI width {}",
        hi_s - lo_s,
        hi_l - lo_l
    );
}
