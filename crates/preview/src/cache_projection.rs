//! Cache hit-rate projection.
//!
//! Given the current call's cost and per-org L1/L2 hit probabilities,
//! return the expected-value savings. Hit probabilities can be plugged in
//! per-org (from cloud-side telemetry) or fall back to global defaults.

use crate::types::CacheProjections;

/// Global defaults — used when per-org telemetry isn't available.
pub const DEFAULT_L1_HIT_PROBABILITY: f32 = 0.20;
pub const DEFAULT_L2_HIT_PROBABILITY: f32 = 0.10;

pub fn project(cost_usd: f64, l1_p: f32, l2_p: f32) -> CacheProjections {
    let l1_p = l1_p.clamp(0.0, 1.0);
    let l2_p = l2_p.clamp(0.0, 1.0);
    let weighted = cost_usd * (l1_p as f64 + (1.0 - l1_p as f64) * l2_p as f64);
    CacheProjections {
        l1_hit_savings_usd: cost_usd,
        l1_hit_probability: l1_p,
        l2_hit_savings_usd: cost_usd,
        l2_hit_probability: l2_p,
        weighted_savings_usd: weighted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_math_known_values() {
        // cost=$1, L1=0.5, L2=0.5 → weighted = 1 * (0.5 + 0.5*0.5) = 0.75
        let p = project(1.0, 0.5, 0.5);
        assert!((p.weighted_savings_usd - 0.75).abs() < 1e-9);
    }

    #[test]
    fn defaults_yield_modest_savings() {
        let p = project(1.0, DEFAULT_L1_HIT_PROBABILITY, DEFAULT_L2_HIT_PROBABILITY);
        // 0.20 + 0.80 * 0.10 = 0.28 (f32→f64 rounding; use 1e-6 tolerance) // fixup
        assert!((p.weighted_savings_usd - 0.28).abs() < 1e-6);
    }

    #[test]
    fn clamps_probabilities() {
        let p = project(1.0, 2.0, -1.0);
        assert_eq!(p.l1_hit_probability, 1.0);
        assert_eq!(p.l2_hit_probability, 0.0);
    }
}
