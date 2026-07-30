//! Deterministic bootstrap confidence intervals. Uses ChaCha8 (seeded)
//! so the same `(samples, seed, iterations)` triple always produces the
//! same `(lo, hi)`. ChaCha8 is the same RNG family `rand` itself uses for
//! `StdRng` on most platforms; the explicit choice here is so we don't
//! inherit a future `rand` default-change as a determinism break.

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Resample `samples` with replacement `iterations` times. For each
/// resample, compute the mean. Return the percentiles of the resampled
/// means — the canonical non-parametric bootstrap CI of the mean.
///
/// Returns `(0.0, 0.0)` when `samples` is empty or `iterations == 0`
/// rather than panicking — callers in `replay.rs` rely on this for the
/// "no requests in window" fast path.
#[must_use]
pub fn bootstrap_ci(
    samples: &[f64],
    seed: u64,
    iterations: u32,
    percentiles: (f64, f64),
) -> (f64, f64) {
    let n = samples.len();
    if n == 0 || iterations == 0 {
        return (0.0, 0.0);
    }
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let n_f = n as f64;
    let mut means: Vec<f64> = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let mut sum = 0.0;
        for _ in 0..n {
            let idx = rng.random_range(0..n);
            sum += samples[idx];
        }
        means.push(sum / n_f);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let iter_f = iterations as f64;
    let lo_idx = ((percentiles.0 * iter_f) as usize).min(means.len() - 1);
    let hi_idx = ((percentiles.1 * iter_f) as usize).min(means.len() - 1);
    (means[lo_idx], means[hi_idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_zero() {
        assert_eq!(bootstrap_ci(&[], 1, 1000, (0.025, 0.975)), (0.0, 0.0));
    }

    #[test]
    fn zero_iterations_returns_zero() {
        assert_eq!(
            bootstrap_ci(&[1.0, 2.0, 3.0], 1, 0, (0.025, 0.975)),
            (0.0, 0.0)
        );
    }

    #[test]
    fn determinism_same_seed() {
        let samples: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let a = bootstrap_ci(&samples, 42, 1000, (0.025, 0.975));
        let b = bootstrap_ci(&samples, 42, 1000, (0.025, 0.975));
        assert_eq!(a, b);
    }

    #[test]
    fn different_seed_different_output() {
        let samples: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let a = bootstrap_ci(&samples, 42, 1000, (0.025, 0.975));
        let b = bootstrap_ci(&samples, 43, 1000, (0.025, 0.975));
        assert_ne!(a, b);
    }

    #[test]
    fn constant_samples_zero_width_ci() {
        let samples = vec![5.0; 50];
        let (lo, hi) = bootstrap_ci(&samples, 7, 500, (0.025, 0.975));
        // Every resample's mean is exactly 5.0 — CI collapses.
        assert!((lo - 5.0).abs() < 1e-12);
        assert!((hi - 5.0).abs() < 1e-12);
    }
}
