//! Tier 3 LLM-judge quality scoring for Plan projections.
//!
//! The judge call is the most expensive operation in the Plan pipeline
//! ($0.05–$0.50/scan per spec §18 budget). This module owns the stratified
//! sampling, judge dispatch, and risk-band aggregation. Production wires it
//! to a real LLM provider; tests drive it with a deterministic [`MockJudge`].
//!
//! # Hard invariants
//!
//! 1. **Opt-in only**: quality sampling reads full request bodies (per
//!    ADR-009 / spec §11). [`score_quality`] refuses to run when
//!    [`QualityConfig::body_logging_enabled`] is `false`.
//! 2. **Stratified sampling**: by `(tag, size_bucket)` — bucket on
//!    `input_tokens`. Proportional allocation, capped at total budget.
//! 3. **Deterministic**: same `(requests, config, seed)` → bit-identical
//!    risk band + sampled request IDs.
//! 4. **Judge agnostic**: scoring uses any [`JudgeProvider`] impl.
//! 5. **Risk thresholds** (per spec §7.4): `LOW` if `degraded ≤ 5%`,
//!    `MEDIUM` if `5% < degraded ≤ 15%`, `HIGH` if `degraded > 15%`.
//! 6. **Cost-capped**: refuses to dispatch when projected cost exceeds
//!    [`QualityConfig::budget_usd`].
//!
//! # Scope
//!
//! Re-running the proposed model is *out of scope* for [`score_quality`] —
//! the caller provides a `proposed_response_for(&Uuid) -> Option<String>`
//! closure so production can dispatch to the real provider while tests can
//! supply a canned map. Library + judge contract; dispatch is caller-owned.

use std::collections::HashMap;

use async_trait::async_trait;
use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::types::RequestLog;

/// Aggregate risk classification per `docs/03-plan-replay-design.md` §7.4
/// (task-spec thresholds: `≤5%` / `(5%, 15%]` / `>15%`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskBand {
    /// Degraded share of *classified* (non-Unclear) samples ≤ 5%.
    Low,
    /// Degraded share in `(5%, 15%]`.
    Medium,
    /// Degraded share > 15%.
    High,
}

impl RiskBand {
    /// Map a degraded percentage (0–100) to a band. Boundary policy:
    /// `≤ 5%` → `Low`, `≤ 15%` → `Medium`, otherwise `High`.
    #[must_use]
    pub fn from_degraded_pct(p: f64) -> Self {
        if p <= 5.0 {
            Self::Low
        } else if p <= 15.0 {
            Self::Medium
        } else {
            Self::High
        }
    }
}

/// Per-sample judge verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JudgeVerdict {
    /// Proposed response is interchangeable with the original.
    Acceptable,
    /// Proposed response is materially worse than the original.
    Degraded,
    /// Judge declined to classify (model refusal, parse failure, etc.).
    /// Counted toward neither acceptable nor degraded but recorded in the
    /// total. A high `Unclear` share surfaces as a user-visible caveat.
    Unclear,
}

/// One sampled request's score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleScore {
    /// Stable identifier of the source [`RequestLog`].
    pub request_id: Uuid,
    /// The judge's classification.
    pub verdict: JudgeVerdict,
    /// Best-effort one-line reason from the judge. Trimmed to 200 chars.
    pub reason: String,
}

/// Aggregated quality result attached to a `PlanResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityResult {
    /// Number of requests the judge actually scored.
    pub sample_size: u32,
    /// Count of `Acceptable` verdicts.
    pub acceptable_count: u32,
    /// Count of `Degraded` verdicts.
    pub degraded_count: u32,
    /// Count of `Unclear` verdicts.
    pub unclear_count: u32,
    /// `degraded_count / (acceptable_count + degraded_count) × 100` (0–100).
    /// Defined over *classified* samples only — `Unclear` is excluded from
    /// the denominator because by definition we don't know its valence.
    pub degraded_pct: f64,
    /// Aggregate band — feeds the user-facing red/yellow/green pill.
    pub risk_band: RiskBand,
    /// Per-sample scores, in stable order. Bounded by
    /// [`QualityConfig::total_samples`].
    pub sampled_examples: Vec<SampleScore>,
    /// Human-readable warnings (small sample, high `Unclear` share, etc.).
    pub caveats: Vec<String>,
}

/// Configuration for one Tier 3 quality scoring run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityConfig {
    /// Required gate: caller must surface body-logging opt-in to the org.
    /// `false` causes [`score_quality`] to error with
    /// [`QualityError::BodyLoggingDisabled`].
    pub body_logging_enabled: bool,
    /// Total samples to draw across all strata (cap).
    pub total_samples: u32,
    /// Hard cost ceiling for judge calls in this run (USD).
    pub budget_usd: f64,
    /// Estimated USD per judge call (typically $0.001–$0.01 for a Sonnet
    /// judge). Multiplied by `total_samples` for the up-front budget check.
    pub cost_per_judge_call_usd: f64,
    /// Random seed for stratified sampling. Determinism contract.
    pub seed: u64,
}

/// Errors surfaced by [`score_quality`].
#[derive(Debug, Error)]
pub enum QualityError {
    /// Caller invoked scoring without the body-logging opt-in. Tier 3
    /// requires raw prompts + responses on each sampled row.
    #[error("body logging not opted in by org — Tier 3 quality scoring requires raw bodies")]
    BodyLoggingDisabled,

    /// Pre-flight estimate `cost_per_judge_call_usd × total_samples`
    /// exceeded the caller's budget. Scoring did not dispatch.
    #[error("projected judge cost ${cost:.4} exceeds budget ${budget:.4}")]
    OverBudget {
        /// Projected cost in USD.
        cost: f64,
        /// Configured budget ceiling in USD.
        budget: f64,
    },

    /// The [`JudgeProvider`] failed mid-run. Holds the provider's message.
    #[error("judge: {0}")]
    Judge(String),

    /// Every sampled row was missing either the prompt body or the
    /// historical response body — nothing was sent to the judge.
    #[error("no sampled requests carry both prompt + response bodies")]
    NoScorable,
}

/// Pluggable judge backend. Production: an LLM provider call. Tests:
/// [`MockJudge`].
#[async_trait]
pub trait JudgeProvider: Send + Sync {
    /// Compare an "original" response and a "proposed" response for the same
    /// input. Return a verdict + one-line reason.
    ///
    /// # Errors
    ///
    /// Returns [`QualityError::Judge`] when the underlying provider fails
    /// in a way the caller should surface to the user (e.g. auth failure,
    /// rate-limit exhaustion). Implementations that recover internally via
    /// retry should return [`JudgeVerdict::Unclear`] rather than erroring.
    async fn judge(
        &self,
        input_body: &str,
        original_response: &str,
        proposed_response: &str,
    ) -> Result<(JudgeVerdict, String), QualityError>;
}

/// Deterministic mock — used by replay tests and any caller that wants to
/// stub the judge for offline analysis.
pub struct MockJudge {
    /// Force every call to return this verdict.
    pub verdict: JudgeVerdict,
    /// Reason string echoed verbatim on every call.
    pub reason: String,
}

#[async_trait]
impl JudgeProvider for MockJudge {
    async fn judge(
        &self,
        _input: &str,
        _orig: &str,
        _prop: &str,
    ) -> Result<(JudgeVerdict, String), QualityError> {
        Ok((self.verdict, self.reason.clone()))
    }
}

/// Compute the `(tag, size_bucket)` stratum for a request. Bucket boundaries
/// match `docs/03-plan-replay-design.md` §7.1 (`small ≤ 500`, `medium ≤ 4000`,
/// `large > 4000` input tokens).
fn stratify(req: &RequestLog) -> (Option<String>, &'static str) {
    let bucket = match req.input_tokens {
        0..=500 => "small",
        501..=4000 => "medium",
        _ => "large",
    };
    (req.tag.clone(), bucket)
}

/// Draw `n` requests stratified by `(tag, size_bucket)` proportional to the
/// stratum's share of the input population. Deterministic given `seed` and
/// the input slice order (we re-sort by `id` internally so callers don't
/// have to).
///
/// Returns the sampled IDs in ascending order. When `n == 0` or `requests`
/// is empty, returns an empty `Vec`. Rounding can produce ≤ `n` samples
/// (never more — final truncation enforces the cap).
#[must_use]
pub fn stratified_sample(requests: &[RequestLog], n: u32, seed: u64) -> Vec<Uuid> {
    if n == 0 || requests.is_empty() {
        return Vec::new();
    }

    // Index requests into deterministic strata. Sort the input by id first
    // so the per-stratum vectors are populated in a deterministic order
    // even when callers pass arbitrary orderings.
    let mut sorted: Vec<&RequestLog> = requests.iter().collect();
    sorted.sort_by_key(|r| r.id);

    let mut by_stratum: HashMap<(Option<String>, &'static str), Vec<Uuid>> = HashMap::new();
    for r in &sorted {
        by_stratum.entry(stratify(r)).or_default().push(r.id);
    }

    // Proportional allocation: each stratum gets `n × (stratum_size / total)`
    // (rounded). Iterate strata in sorted key order so the RNG draws happen
    // in a deterministic sequence.
    let total = requests.len() as f64;
    let n_f = f64::from(n);
    let mut keys: Vec<_> = by_stratum.keys().cloned().collect();
    keys.sort();

    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut out = Vec::new();
    for k in keys {
        let stratum = &by_stratum[&k];
        let alloc = ((stratum.len() as f64 / total) * n_f).round() as usize;
        let alloc = alloc.min(stratum.len());
        if alloc == 0 {
            continue;
        }
        // Fisher–Yates partial shuffle so the first `alloc` indices are a
        // uniform without-replacement draw.
        let mut idx: Vec<usize> = (0..stratum.len()).collect();
        for i in (1..idx.len()).rev() {
            let j = rng.gen_range(0..=i);
            idx.swap(i, j);
        }
        for i in idx.into_iter().take(alloc) {
            out.push(stratum[i]);
        }
    }

    // Final dedupe + cap. Sort for deterministic order; the input id-sort
    // means any two IDs that landed in the same stratum can't collide here,
    // but sorting also makes the output independent of stratum iteration
    // order so the snapshot stays stable across HashMap reorderings.
    out.sort();
    out.dedup();
    if out.len() > n as usize {
        out.truncate(n as usize);
    }
    out
}

/// Score quality by sampling requests, comparing original vs proposed
/// responses via the judge, and aggregating into a [`RiskBand`].
///
/// `proposed_response_for(id)` lets the caller plug in the
/// proposed-model dispatch — production routes through the real provider,
/// tests supply canned strings. Returning `None` skips that sample.
///
/// # Errors
///
/// - [`QualityError::BodyLoggingDisabled`] when
///   `config.body_logging_enabled == false`.
/// - [`QualityError::OverBudget`] when projected judge cost exceeds the
///   configured budget. Computed before any judge call dispatches.
/// - [`QualityError::Judge`] when a judge call fails.
/// - [`QualityError::NoScorable`] when every sampled row was missing
///   prompt body, response body, or a proposed response.
pub async fn score_quality<F>(
    requests: &[RequestLog],
    config: &QualityConfig,
    judge: &dyn JudgeProvider,
    proposed_response_for: F,
) -> Result<QualityResult, QualityError>
where
    F: Fn(&Uuid) -> Option<String>,
{
    if !config.body_logging_enabled {
        return Err(QualityError::BodyLoggingDisabled);
    }
    let projected_cost = config.cost_per_judge_call_usd * f64::from(config.total_samples);
    if projected_cost > config.budget_usd {
        return Err(QualityError::OverBudget {
            cost: projected_cost,
            budget: config.budget_usd,
        });
    }

    let sampled_ids = stratified_sample(requests, config.total_samples, config.seed);
    let by_id: HashMap<Uuid, &RequestLog> = requests.iter().map(|r| (r.id, r)).collect();

    let mut scores = Vec::new();
    let mut acceptable: u32 = 0;
    let mut degraded: u32 = 0;
    let mut unclear: u32 = 0;

    for id in &sampled_ids {
        let Some(req) = by_id.get(id) else { continue };
        let Some(input) = req.body.as_ref() else {
            continue;
        };
        let Some(original) = req.response_body.as_ref() else {
            continue;
        };
        let Some(proposed) = proposed_response_for(id) else {
            continue;
        };

        let (verdict, mut reason) = judge.judge(input, original, &proposed).await?;
        if reason.len() > 200 {
            // Truncate at a char boundary to avoid panic on multi-byte text.
            let mut cut = 200;
            while cut > 0 && !reason.is_char_boundary(cut) {
                cut -= 1;
            }
            reason.truncate(cut);
        }
        match verdict {
            JudgeVerdict::Acceptable => acceptable += 1,
            JudgeVerdict::Degraded => degraded += 1,
            JudgeVerdict::Unclear => unclear += 1,
        }
        scores.push(SampleScore {
            request_id: *id,
            verdict,
            reason,
        });
    }

    if scores.is_empty() {
        return Err(QualityError::NoScorable);
    }

    let total_classified = f64::from(acceptable + degraded);
    let degraded_pct = if total_classified > 0.0 {
        (f64::from(degraded) / total_classified) * 100.0
    } else {
        0.0
    };
    let risk_band = RiskBand::from_degraded_pct(degraded_pct);

    let mut caveats = Vec::new();
    let unclear_share = f64::from(unclear) / scores.len() as f64;
    if unclear_share > 0.20 {
        caveats.push(format!(
            "{:.0}% of sampled requests were Unclear — the judge couldn't classify. \
             Risk band may be unreliable; consider a stronger judge model.",
            unclear_share * 100.0
        ));
    }
    if scores.len() < 30 {
        caveats.push(format!(
            "Small quality sample ({} scored) — risk band has wide uncertainty.",
            scores.len()
        ));
    }

    Ok(QualityResult {
        sample_size: scores.len() as u32,
        acceptable_count: acceptable,
        degraded_count: degraded,
        unclear_count: unclear,
        degraded_pct,
        risk_band,
        sampled_examples: scores,
        caveats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_band_thresholds() {
        assert_eq!(RiskBand::from_degraded_pct(0.0), RiskBand::Low);
        assert_eq!(RiskBand::from_degraded_pct(5.0), RiskBand::Low);
        assert_eq!(RiskBand::from_degraded_pct(5.0001), RiskBand::Medium);
        assert_eq!(RiskBand::from_degraded_pct(15.0), RiskBand::Medium);
        assert_eq!(RiskBand::from_degraded_pct(15.0001), RiskBand::High);
        assert_eq!(RiskBand::from_degraded_pct(100.0), RiskBand::High);
    }
}
