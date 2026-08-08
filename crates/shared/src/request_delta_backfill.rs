//! Idempotent, pure historical backfill for `tt.request-delta-estimate.v1`
//! evidence over a snapshot of retained request rows.
//!
//! This module owns no I/O: it maps an immutable snapshot of retained request
//! rows to the exact [`RequestDeltaEvidenceState`] each row must record, so a
//! caller persists the produced states in one pass. Replaying the function
//! over the same rows always reproduces identical states and identical
//! coverage counts, so a backfill run is idempotent and never double-counts
//! an aggregate. Provider usage, judge/shadow tax allocation, component
//! signing/replay, and invoice reconciliation are deliberately out of scope
//! and never fabricated here.

use crate::request_delta::{
    classify_request_delta_evidence_v1, RequestDeltaEvidenceState, RequestDeltaInput,
};

/// Stable identifier for the historical request-delta backfill semantics.
///
/// Bump only when the replay semantics change; an immutable corpus must not
/// silently reinterpret retained rows under a newer rule.
pub const REQUEST_DELTA_BACKFILL_V1: &str = "tt.request-delta-backfill.v1";

/// Whether the retained row can positively assert the pricing that applied at
/// write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingProvenance {
    /// Closed write-time provenance: the runtime recorded whether the served
    /// and baseline model prices were known, so a missing price is a real
    /// catalog miss (`unpriceable`) rather than an unknown.
    Known {
        served_pricing_known: bool,
        baseline_pricing_known: bool,
    },
    /// The row predates the evidence column (or a foreign writer produced it).
    /// Its numeric tuple is never used to infer measurement: the row stays
    /// `missing_evidence`, because a stored zero cannot prove pricing was
    /// genuinely known when the row was written.
    Unknown,
}

/// One retained request row eligible to be replayed by the backfill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetainedRequestRow<'a> {
    /// Stable caller-facing identifier (e.g. request id / row key) echoed into
    /// the output so the caller can apply each recorded state to the right row.
    pub row_ref: &'a str,
    pub pricing_provenance: PricingProvenance,
    /// Raw formula components captured on the retained row.
    pub input: RequestDeltaInput,
}

/// The exact evidence state one retained row must record after backfill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackfilledRow<'a> {
    pub row_ref: &'a str,
    pub state: RequestDeltaEvidenceState,
}

/// Aggregate coverage of one backfill run.
///
/// `eligible_rows` counts rows whose pricing provenance is positively known
/// (and therefore could have been measured); `measured_rows` is the strict
/// subset of those that classified as measured. Rows with unknown provenance
/// are counted in `backfilled_rows` but are never eligible. Consumers must
/// report measured/eligible coverage and withhold complete dollar values when
/// `eligible_rows > measured_rows`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillCoverage {
    /// Rows replayed by this run, regardless of provenance.
    pub backfilled_rows: u64,
    /// Rows whose pricing provenance is positively known. Unknown-provenance
    /// rows are never eligible.
    pub eligible_rows: u64,
    /// The strict subset of eligible rows that classified as measured.
    pub measured_rows: u64,
}

/// One backfill replay: the per-row state assignments plus the run coverage.
#[derive(Debug, Clone, PartialEq)]
pub struct BackfillRun<'a> {
    pub rows: Vec<BackfilledRow<'a>>,
    pub coverage: BackfillCoverage,
}

/// Reproduce the exact evidence state a single retained row must record.
///
/// Row order never matters: a lone replay and a batch replay assign the same
/// state for the same row, which is what makes [`backfill_request_delta_evidence_v1`]
/// idempotent. Unknown pricing provenance conservatively stays
/// `missing_evidence` and is never upgraded from the numeric tuple.
#[must_use]
pub fn row_evidence_v1(
    pricing_provenance: PricingProvenance,
    input: RequestDeltaInput,
) -> RequestDeltaEvidenceState {
    match pricing_provenance {
        PricingProvenance::Known {
            served_pricing_known,
            baseline_pricing_known,
        } => {
            classify_request_delta_evidence_v1(served_pricing_known, baseline_pricing_known, input)
        }
        PricingProvenance::Unknown => RequestDeltaEvidenceState::MissingEvidence,
    }
}

/// Replay `tt.request-delta-estimate.v1` evidence over a snapshot of retained
/// request rows, recording the exact state each row must persist.
///
/// Pure and deterministic: the same rows always produce the same states and
/// the same coverage, so the run is idempotent and summable without
/// double-counting. Rows with unknown pricing provenance are recorded as
/// `missing_evidence` regardless of their numeric tuple, mirroring the
/// migration rule that historical rows are never inferred from numeric zero.
#[must_use]
pub fn backfill_request_delta_evidence_v1<'a>(
    rows: impl IntoIterator<Item = RetainedRequestRow<'a>>,
) -> BackfillRun<'a> {
    let mut out = Vec::new();
    let mut coverage = BackfillCoverage::default();
    for row in rows {
        let state = row_evidence_v1(row.pricing_provenance, row.input);
        coverage.backfilled_rows = coverage.backfilled_rows.saturating_add(1);
        if row.pricing_provenance == PricingProvenance::Unknown {
            // Not eligible: provenance is unknown, never inferred.
            out.push(BackfilledRow {
                row_ref: row.row_ref,
                state,
            });
            continue;
        }
        coverage.eligible_rows = coverage.eligible_rows.saturating_add(1);
        if state == RequestDeltaEvidenceState::Measured {
            coverage.measured_rows = coverage.measured_rows.saturating_add(1);
        }
        out.push(BackfilledRow {
            row_ref: row.row_ref,
            state,
        });
    }
    BackfillRun {
        rows: out,
        coverage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        baseline_cost_usd: f64,
        cost_usd: f64,
        provider_cache_saved_usd: f64,
        cache_bust_penalty_usd: f64,
        summarizer_tax_usd: f64,
    ) -> RequestDeltaInput {
        RequestDeltaInput {
            baseline_cost_usd: Some(baseline_cost_usd),
            cost_usd: Some(cost_usd),
            provider_cache_saved_usd: Some(provider_cache_saved_usd),
            cache_bust_penalty_usd: Some(cache_bust_penalty_usd),
            summarizer_tax_usd: Some(summarizer_tax_usd),
        }
    }

    /// `baseline 1.0 - cost 0.4 - cache 0.1 - bust 0.05 - tax 0.05 = +0.40`.
    const POSITIVE: RequestDeltaInput = RequestDeltaInput {
        baseline_cost_usd: Some(1.0),
        cost_usd: Some(0.4),
        provider_cache_saved_usd: Some(0.1),
        cache_bust_penalty_usd: Some(0.05),
        summarizer_tax_usd: Some(0.05),
    };

    /// `baseline 0.8 - cost 0.7 - cache 0.05 - bust 0.1 - tax 0.2 = -0.25`.
    const REGRESSION: RequestDeltaInput = RequestDeltaInput {
        baseline_cost_usd: Some(0.8),
        cost_usd: Some(0.7),
        provider_cache_saved_usd: Some(0.05),
        cache_bust_penalty_usd: Some(0.1),
        summarizer_tax_usd: Some(0.2),
    };

    #[test]
    fn backfill_replays_formula_into_measured_states() {
        let run = backfill_request_delta_evidence_v1([
            RetainedRequestRow {
                row_ref: "r-positive",
                pricing_provenance: PricingProvenance::Known {
                    served_pricing_known: true,
                    baseline_pricing_known: true,
                },
                input: POSITIVE,
            },
            RetainedRequestRow {
                row_ref: "r-regression",
                pricing_provenance: PricingProvenance::Known {
                    served_pricing_known: true,
                    baseline_pricing_known: true,
                },
                input: REGRESSION,
            },
            RetainedRequestRow {
                row_ref: "r-zero",
                pricing_provenance: PricingProvenance::Known {
                    served_pricing_known: true,
                    baseline_pricing_known: true,
                },
                input: input(1.0, 0.7, 0.1, 0.1, 0.1),
            },
        ]);
        assert_eq!(
            run.rows,
            vec![
                BackfilledRow {
                    row_ref: "r-positive",
                    state: RequestDeltaEvidenceState::Measured,
                },
                BackfilledRow {
                    row_ref: "r-regression",
                    state: RequestDeltaEvidenceState::Measured,
                },
                BackfilledRow {
                    row_ref: "r-zero",
                    state: RequestDeltaEvidenceState::Measured,
                },
            ]
        );
        assert_eq!(
            run.coverage,
            BackfillCoverage {
                backfilled_rows: 3,
                eligible_rows: 3,
                measured_rows: 3,
            }
        );
    }

    #[test]
    fn backfill_never_infers_unknown_provenance_from_numeric_tuple() {
        // The same tuple that classifies measured under known provenance must
        // stay missing_evidence when provenance is unknown, mirroring the rule
        // that historical rows are never inferred from numeric zero.
        let run = backfill_request_delta_evidence_v1([
            RetainedRequestRow {
                row_ref: "old-positive",
                pricing_provenance: PricingProvenance::Unknown,
                input: POSITIVE,
            },
            RetainedRequestRow {
                row_ref: "old-zero",
                pricing_provenance: PricingProvenance::Unknown,
                input: input(0.0, 0.0, 0.0, 0.0, 0.0),
            },
        ]);
        assert_eq!(
            run.rows,
            vec![
                BackfilledRow {
                    row_ref: "old-positive",
                    state: RequestDeltaEvidenceState::MissingEvidence,
                },
                BackfilledRow {
                    row_ref: "old-zero",
                    state: RequestDeltaEvidenceState::MissingEvidence,
                },
            ]
        );
        assert_eq!(
            run.coverage,
            BackfillCoverage {
                backfilled_rows: 2,
                eligible_rows: 0,
                measured_rows: 0,
            }
        );
    }

    #[test]
    fn backfill_is_idempotent_and_never_double_counts() {
        let snapshot = [
            RetainedRequestRow {
                row_ref: "a",
                pricing_provenance: PricingProvenance::Known {
                    served_pricing_known: true,
                    baseline_pricing_known: true,
                },
                input: POSITIVE,
            },
            RetainedRequestRow {
                row_ref: "b",
                pricing_provenance: PricingProvenance::Unknown,
                input: REGRESSION,
            },
            RetainedRequestRow {
                row_ref: "c",
                pricing_provenance: PricingProvenance::Known {
                    served_pricing_known: false,
                    baseline_pricing_known: true,
                },
                input: POSITIVE,
            },
        ];

        // The exact same rows replayed a second time produce the exact same
        // states and coverage: the recorded column update is idempotent.
        let first = backfill_request_delta_evidence_v1(snapshot);
        let second = backfill_request_delta_evidence_v1(snapshot);
        assert_eq!(first.rows, second.rows);
        assert_eq!(first.coverage, second.coverage);

        // Re-recording each row's produced state through the per-row entry
        // point also reproduces the same state for the same inputs.
        for row in &first.rows {
            let source = snapshot
                .iter()
                .find(|r| r.row_ref == row.row_ref)
                .expect("every produced row must come from the snapshot");
            assert_eq!(
                row_evidence_v1(source.pricing_provenance, source.input),
                row.state,
                "per-row replay must match the batch assignment"
            );
        }
    }

    #[test]
    fn backfill_distinguishes_catalog_miss_from_missing_and_reports_coverage() {
        let run = backfill_request_delta_evidence_v1([
            RetainedRequestRow {
                row_ref: "catalog-miss",
                pricing_provenance: PricingProvenance::Known {
                    served_pricing_known: false,
                    baseline_pricing_known: true,
                },
                input: POSITIVE,
            },
            RetainedRequestRow {
                row_ref: "malformed",
                pricing_provenance: PricingProvenance::Known {
                    served_pricing_known: true,
                    baseline_pricing_known: true,
                },
                input: RequestDeltaInput {
                    cost_usd: Some(f64::NAN),
                    ..POSITIVE
                },
            },
            RetainedRequestRow {
                row_ref: "measured",
                pricing_provenance: PricingProvenance::Known {
                    served_pricing_known: true,
                    baseline_pricing_known: true,
                },
                input: POSITIVE,
            },
        ]);
        assert_eq!(
            run.rows,
            vec![
                BackfilledRow {
                    row_ref: "catalog-miss",
                    state: RequestDeltaEvidenceState::Unpriceable,
                },
                BackfilledRow {
                    row_ref: "malformed",
                    state: RequestDeltaEvidenceState::MissingEvidence,
                },
                BackfilledRow {
                    row_ref: "measured",
                    state: RequestDeltaEvidenceState::Measured,
                },
            ]
        );
        assert_eq!(
            run.coverage,
            BackfillCoverage {
                backfilled_rows: 3,
                eligible_rows: 3,
                measured_rows: 1,
            }
        );
        // Measured != eligible signals an incomplete dollar claim; a consumer
        // must report coverage rather than a full measured net.
        assert_ne!(run.coverage.measured_rows, run.coverage.eligible_rows);
    }

    #[test]
    fn empty_backfill_has_zero_coverage() {
        let run = backfill_request_delta_evidence_v1([]);
        assert!(run.rows.is_empty());
        assert_eq!(run.coverage, BackfillCoverage::default());
    }
}
