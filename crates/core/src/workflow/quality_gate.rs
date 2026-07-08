//! The flow-level end-to-end quality gate — an OPTIONAL, sample-rate-cost-
//! bounded judge that compares a workflow run's final synthesized answer
//! against a baseline (a reference-model re-dispatch of the trigger input),
//! producing a `QualityVerdict` a per-flow attestation can carry.
//!
//! BACKLOG item #5 (flow-level end-to-end quality gate), Slice 1 (public): the
//! pure gate logic + the fail-open detached spawn. The canonical-payload bump
//! (`wfr:v2`), the ledger column, + the mint endpoint land in Slice 2 (cloud).
//!
//! # What this is
//! The workflow receipt (`wfr:v1|...|cost|baseline|saved|status`) attests cost
//! + savings but not quality. A down-routed workflow that saves money could
//! degrade the answer. This gate makes "saved $X AND the answer was judged
//! equivalent to the baseline" a single claim — the verdict folds into the
//! receipt attestation in Slice 2.
//!
//! # Reuse, not rebuild
//! The primitives already exist in [`crate::quality_sample`]:
//! - [`quality_sample::should_sample`] — deterministic-but-uniform sampling.
//! - the ABS-style paired judge [`quality_sample::judge_paired`] compares an
//!   optimized answer to a baseline (counters position bias). It IS a
//!   final-answer-vs-baseline judge.
//! This module is the per-RUN invocation of that primitive (vs the per-tool-call
//! summary-judge the agent_run loop already does), with the workflow's `Output`
//! node's final content as `optimized_answer` + a reference-model re-dispatch
//! of the trigger input as `baseline_answer`.
//!
//! # Fail-open (the production posture)
//! The gate is **opt-in + fail-open**: it's off by default
//! (`JudgeConfig::from_env()` with `sample_rate == 0` unless set). A gate that
//! errors/times out / is disabled records `QualityVerdict::NotSampled` — it
//! NEVER blocks the run, never fails the workflow. Mirrors the VCR /
//! attestation fail-open posture.
//!
//! # Cost bounding
//! Two-stage, mirroring the agent-run per-turn judge:
//! 1. `PerOrgDayJudgeCap` — bounds per-org-day judge spend (bites BEFORE sampling).
//! 2. `should_sample` — the deterministic sample rate (the trace_id is the key).
//! Plus `both_orders: false` — one paired judge per sampled run (not both AB
//! orders). A sampled run costs exactly one reference-model completion.

use uuid::Uuid;

use crate::quality_sample;

/// The stable string codes a [`QualityVerdict`] serializes to in a signed
/// canonical payload (part of the SIGNED bytes — change the version, never the
/// codes). Mirrors the VCR/L2-receipt verdict-code discipline.
pub const VERDICT_EQUIVALENT: &str = "equivalent";
pub const VERDICT_DEGRADED: &str = "degraded";
pub const VERDICT_INCONCLUSIVE: &str = "inconclusive";
pub const VERDICT_NOT_SAMPLED: &str = "not_sampled";

/// The flow-level quality gate's verdict for a workflow run. Folded into the
/// per-flow attestation (the `wfr:v2` receipt, Slice 2) as the stable code.
///
/// `NotSampled` is the default (gate off, sample-rate 0, run not sampled, or
/// the run failed before producing a final answer) — a pre-gate / un-sampled
/// run carries no verdict, so the receipt stays `wfr:v1` (backward-compat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityVerdict {
    /// The run was sampled + `judge_paired` judged the final answer's recall of
    /// the baseline at or above the config threshold.
    Equivalent,
    /// The run was sampled + the judge found a meaningful quality regression.
    Degraded,
    /// The run was sampled but the judge was inconclusive (tied / errored mid-
    /// compare). Not a PASS, not a FAIL — surfaced honestly as "could not judge".
    Inconclusive,
    /// The gate was off / the run was not sampled / the run produced no final
    /// answer (failed, budget-exhausted, or no `Output` node). The receipt
    /// carries NO quality verdict (stays `wfr:v1`).
    NotSampled,
}

impl QualityVerdict {
    /// The stable string code for the signed canonical payload.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Equivalent => VERDICT_EQUIVALENT,
            Self::Degraded => VERDICT_DEGRADED,
            Self::Inconclusive => VERDICT_INCONCLUSIVE,
            Self::NotSampled => VERDICT_NOT_SAMPLED,
        }
    }

    /// Parse a code back to the verdict (the verify path). Unknown codes →
    /// `NotSampled` (fail-safe — a receipt with a garbage verdict is treated as
    /// un-sampled, not as a false PASS).
    #[must_use]
    pub fn from_code(code: &str) -> Self {
        match code {
            VERDICT_EQUIVALENT => Self::Equivalent,
            VERDICT_DEGRADED => Self::Degraded,
            VERDICT_INCONCLUSIVE => Self::Inconclusive,
            _ => Self::NotSampled,
        }
    }

    /// Whether this verdict should be carried on a `wfr:v2` receipt (vs the
    /// v1 shape, which carries no quality verdict). `NotSampled` → v1 (the
    /// backward-compat default); any sampled verdict → v2.
    #[must_use]
    pub fn carries_on_receipt(self) -> bool {
        !matches!(self, Self::NotSampled)
    }
}

/// The two-stage bound for the per-run gate — mirrors the agent-run per-turn
/// judge's cap-then-sample. The cap (per-org-day) bites BEFORE the sample rate;
/// together they bound the gate's judge spend. `should_quality_gate` is the
/// single entry point.
///
/// # Arguments
/// * `key` — the sampling key (the run_id — deterministic per-run).
/// * `sample_rate` — the JudgeConfig sample rate (0.0 = off).
/// * `cap` — the per-org-day cap (None = uncapped; the cap is the caller's to
///   acquire so it can record the outcome).
#[must_use]
pub fn should_quality_gate(
    key: Uuid,
    sample_rate: f64,
    cap_acquired: bool,
) -> bool {
    // The cap bites BEFORE sampling (mirrors the agent-run judge: the cap is
    // the hard bound on judge spend; the sample rate is the within-cap rate).
    if !cap_acquired {
        return false;
    }
    quality_sample::should_sample(key, sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_codes_are_round_trippable() {
        for v in [
            QualityVerdict::Equivalent,
            QualityVerdict::Degraded,
            QualityVerdict::Inconclusive,
            QualityVerdict::NotSampled,
        ] {
            assert_eq!(QualityVerdict::from_code(v.code()), v);
        }
    }

    #[test]
    fn unknown_code_fails_safe_to_not_sampled() {
        // A receipt with a garbage verdict must NOT be treated as a PASS.
        assert_eq!(QualityVerdict::from_code("pass"), QualityVerdict::NotSampled);
        assert_eq!(QualityVerdict::from_code(""), QualityVerdict::NotSampled);
        assert_eq!(
            QualityVerdict::from_code("EQUIVALENT"), // case-sensitive
            QualityVerdict::NotSampled,
        );
    }

    #[test]
    fn only_sampled_verdicts_carry_on_a_receipt() {
        assert!(QualityVerdict::Equivalent.carries_on_receipt());
        assert!(QualityVerdict::Degraded.carries_on_receipt());
        assert!(QualityVerdict::Inconclusive.carries_on_receipt());
        assert!(!QualityVerdict::NotSampled.carries_on_receipt());
    }

    #[test]
    fn should_quality_gate_requires_cap_then_sample() {
        let key = Uuid::from_u128(42);
        // cap not acquired → never gate, regardless of rate.
        assert!(!should_quality_gate(key, 1.0, false));
        // rate 0 → never gate even with the cap.
        assert!(!should_quality_gate(key, 0.0, true));
        // rate 1 + cap → always gate.
        assert!(should_quality_gate(key, 1.0, true));
    }

    #[test]
    fn should_quality_gate_is_deterministic_for_a_fixed_key() {
        // should_sample is a pure function of (key, rate) — the same run_id +
        // rate always gate-or-not identically (no Math.random-class flakiness).
        let key = Uuid::from_u128(99);
        let a = should_quality_gate(key, 0.5, true);
        let b = should_quality_gate(key, 0.5, true);
        assert_eq!(a, b);
    }
}
