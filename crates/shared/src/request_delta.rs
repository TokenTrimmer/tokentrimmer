//! Versioned, request-level catalog savings-estimate boundary.
//!
//! This module owns the pure arithmetic shared by the gateway core, Rust
//! client, CLI, and any corpus mirror. It does not select a cohort or price,
//! allocate judge/shadow tax, or reconcile a provider invoice.

use serde::{Deserialize, Serialize};

/// Stable identifier for the request-level formula and its public corpus.
pub const REQUEST_DELTA_ESTIMATE_V1: &str = "tt.request-delta-estimate.v1";

/// Closed provenance state for the persisted request-delta inputs.
///
/// A numeric zero is not evidence by itself: it can mean a genuinely free
/// local model or a catalog miss that the runtime intentionally flattened to
/// zero. Writers therefore persist the reason separately and reporting code
/// groups by this field instead of reverse-engineering provenance from money.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestDeltaEvidenceState {
    /// Every formula input is present and valid, with both served and baseline
    /// pricing known when pricing was required.
    Measured,
    /// At least one required model price was unavailable. The row remains
    /// billable telemetry, but its dollar delta must not enter measured sums.
    Unpriceable,
    /// Required non-pricing evidence was absent or invalid. This is also the
    /// conservative default for rows written before the provenance field.
    #[default]
    MissingEvidence,
}

impl RequestDeltaEvidenceState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Unpriceable => "unpriceable",
            Self::MissingEvidence => "missing_evidence",
        }
    }

    /// Parse the closed persisted/wire representation. Callers reading legacy
    /// or corrupt storage should use `unwrap_or_default()` so unknown values
    /// fail conservatively to `missing_evidence`.
    #[must_use]
    pub fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "measured" => Some(Self::Measured),
            "unpriceable" => Some(Self::Unpriceable),
            "missing_evidence" => Some(Self::MissingEvidence),
            _ => None,
        }
    }
}

/// Complete raw inputs for one gateway-recorded request delta.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RequestDeltaInput {
    pub baseline_cost_usd: Option<f64>,
    pub cost_usd: Option<f64>,
    pub provider_cache_saved_usd: Option<f64>,
    pub cache_bust_penalty_usd: Option<f64>,
    pub summarizer_tax_usd: Option<f64>,
}

/// A measured signed request delta. Absence from [`estimate_request_delta_v1`]
/// means at least one input was missing or invalid; it never means zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RequestDeltaEstimate {
    pub signed_request_delta_usd: f64,
    pub positive_request_delta_usd: f64,
    pub regression_request_delta_usd: f64,
}

/// Classify the evidence behind one request-delta row without inferring from
/// its numeric values.
///
/// Missing pricing takes precedence because it is actionable catalog evidence.
/// With complete pricing, malformed or absent formula inputs remain a distinct
/// missing-evidence bucket. Only a formula-valid tuple is measured.
#[must_use]
pub fn classify_request_delta_evidence_v1(
    served_pricing_known: bool,
    baseline_pricing_known: bool,
    input: RequestDeltaInput,
) -> RequestDeltaEvidenceState {
    if !served_pricing_known || !baseline_pricing_known {
        return RequestDeltaEvidenceState::Unpriceable;
    }
    if estimate_request_delta_v1(input).is_some() {
        RequestDeltaEvidenceState::Measured
    } else {
        RequestDeltaEvidenceState::MissingEvidence
    }
}

/// Formula and coverage fields signed by WFR v3/v4 and ARR v2 receipts.
///
/// Only complete, nonempty coverage is representable. Incomplete/empty cohorts
/// do not mint a run receipt, so they can never acquire a partial sum or zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestDeltaReceiptFields<'a> {
    pub cost_micros: i64,
    pub baseline_micros: i64,
    pub saved_micros: i64,
    pub signed_request_delta_micros: i64,
    pub formula_version: &'a str,
    pub eligible_requests: i64,
    pub measured_requests: i64,
}

impl RequestDeltaReceiptFields<'_> {
    /// Validate the signed receipt evidence and render its fixed-order fields.
    ///
    /// Field order:
    /// `<cost>|<baseline>|<saved>|<signed>|<formula>|<eligible>|<measured>`
    ///
    /// The surrounding WFR/ARR canonicalizer owns identifiers, status, and an
    /// optional quality verdict. Keeping this fragment here gives the public
    /// verifier and hosted signer one Rust source for the money/coverage state.
    pub fn canonical_fragment(&self) -> Result<String, RequestDeltaReceiptError> {
        self.validate()?;
        Ok(format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.cost_micros,
            self.baseline_micros,
            self.saved_micros,
            self.signed_request_delta_micros,
            self.formula_version,
            self.eligible_requests,
            self.measured_requests,
        ))
    }

    /// Enforce exact formula identity, sane coverage, and all-or-nothing money.
    pub fn validate(&self) -> Result<(), RequestDeltaReceiptError> {
        if self.cost_micros < 0 || self.baseline_micros < 0 {
            return Err(RequestDeltaReceiptError::NegativeCostOrBaseline);
        }
        if self.formula_version != REQUEST_DELTA_ESTIMATE_V1 {
            return Err(RequestDeltaReceiptError::UnknownFormulaVersion);
        }
        if self.eligible_requests < 0
            || self.measured_requests < 0
            || self.measured_requests > self.eligible_requests
        {
            return Err(RequestDeltaReceiptError::InvalidCoverage);
        }

        if self.eligible_requests == 0 || self.measured_requests != self.eligible_requests {
            return Err(RequestDeltaReceiptError::IncompleteCoverage);
        }
        if self.saved_micros != self.signed_request_delta_micros.max(0) {
            return Err(RequestDeltaReceiptError::InvalidPositiveProjection);
        }
        Ok(())
    }
}

/// Structural failures in versioned run-receipt request-delta evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDeltaReceiptError {
    NegativeCostOrBaseline,
    UnknownFormulaVersion,
    InvalidCoverage,
    IncompleteCoverage,
    InvalidPositiveProjection,
}

impl std::fmt::Display for RequestDeltaReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NegativeCostOrBaseline => {
                f.write_str("receipt cost and baseline must be non-negative")
            }
            Self::UnknownFormulaVersion => {
                f.write_str("receipt formula version is not tt.request-delta-estimate.v1")
            }
            Self::InvalidCoverage => {
                f.write_str("receipt coverage must satisfy 0 <= measured <= eligible")
            }
            Self::IncompleteCoverage => {
                f.write_str("receipt coverage must be nonempty and completely measured")
            }
            Self::InvalidPositiveProjection => {
                f.write_str("receipt saved_micros must equal max(signed_request_delta_micros, 0)")
            }
        }
    }
}

impl std::error::Error for RequestDeltaReceiptError {}

/// Apply `tt.request-delta-estimate.v1` to one complete request.
///
/// Every component must be present, finite, and non-negative. Rejecting the
/// whole row prevents a rolling deploy or malformed transport value from being
/// silently zero-filled into a customer-facing money claim.
#[must_use]
pub fn estimate_request_delta_v1(input: RequestDeltaInput) -> Option<RequestDeltaEstimate> {
    let components = [
        input.baseline_cost_usd?,
        input.cost_usd?,
        input.provider_cache_saved_usd?,
        input.cache_bust_penalty_usd?,
        input.summarizer_tax_usd?,
    ];
    if components
        .into_iter()
        .any(|value| !value.is_finite() || value < 0.0)
    {
        return None;
    }

    let signed_request_delta_usd =
        components[0] - components[1] - components[2] - components[3] - components[4];
    if !signed_request_delta_usd.is_finite() {
        return None;
    }

    Some(RequestDeltaEstimate {
        signed_request_delta_usd,
        positive_request_delta_usd: signed_request_delta_usd.max(0.0),
        regression_request_delta_usd: (-signed_request_delta_usd).max(0.0),
    })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Corpus {
        formula_version: String,
        vectors: Vec<Vector>,
    }

    #[derive(Deserialize)]
    struct Vector {
        id: String,
        input: CorpusInput,
        expected: Expected,
    }

    #[derive(Deserialize)]
    struct CorpusInput {
        baseline_cost_usd: Option<f64>,
        cost_usd: Option<f64>,
        provider_cache_saved_usd: Option<f64>,
        cache_bust_penalty_usd: Option<f64>,
        summarizer_tax_usd: Option<f64>,
    }

    #[derive(Deserialize)]
    struct Expected {
        state: String,
        signed_request_delta_usd: Option<f64>,
        positive_request_delta_usd: Option<f64>,
        regression_request_delta_usd: Option<f64>,
    }

    #[test]
    fn public_corpus_matches_the_runtime_formula() {
        let corpus: Corpus = serde_json::from_str(include_str!(
            "../../../docs/savings-estimate-contract/tokentrimmer.request-delta-estimate.v1.corpus.json"
        ))
        .expect("request-delta corpus must parse");
        assert_eq!(corpus.formula_version, REQUEST_DELTA_ESTIMATE_V1);

        for vector in corpus.vectors {
            let actual = estimate_request_delta_v1(RequestDeltaInput {
                baseline_cost_usd: vector.input.baseline_cost_usd,
                cost_usd: vector.input.cost_usd,
                provider_cache_saved_usd: vector.input.provider_cache_saved_usd,
                cache_bust_penalty_usd: vector.input.cache_bust_penalty_usd,
                summarizer_tax_usd: vector.input.summarizer_tax_usd,
            });
            match vector.expected.state.as_str() {
                "measured" => {
                    let actual = actual.unwrap_or_else(|| panic!("{} must be measured", vector.id));
                    assert_close(
                        actual.signed_request_delta_usd,
                        vector.expected.signed_request_delta_usd.unwrap(),
                        &vector.id,
                    );
                    assert_close(
                        actual.positive_request_delta_usd,
                        vector.expected.positive_request_delta_usd.unwrap(),
                        &vector.id,
                    );
                    assert_close(
                        actual.regression_request_delta_usd,
                        vector.expected.regression_request_delta_usd.unwrap(),
                        &vector.id,
                    );
                }
                "unmeasured" => assert!(actual.is_none(), "{} must be unmeasured", vector.id),
                state => panic!("unknown corpus state {state}"),
            }
        }
    }

    #[test]
    fn receipt_fragment_preserves_complete_regressions() {
        let regression = RequestDeltaReceiptFields {
            cost_micros: 200_000,
            baseline_micros: 180_000,
            saved_micros: 0,
            signed_request_delta_micros: -50_000,
            formula_version: REQUEST_DELTA_ESTIMATE_V1,
            eligible_requests: 2,
            measured_requests: 2,
        };
        assert_eq!(
            regression.canonical_fragment().unwrap(),
            "200000|180000|0|-50000|tt.request-delta-estimate.v1|2|2"
        );
    }

    #[test]
    fn evidence_state_preserves_zero_price_and_missing_price_distinction() {
        let free_but_priced = RequestDeltaInput {
            baseline_cost_usd: Some(0.0),
            cost_usd: Some(0.0),
            provider_cache_saved_usd: Some(0.0),
            cache_bust_penalty_usd: Some(0.0),
            summarizer_tax_usd: Some(0.0),
        };
        assert_eq!(
            classify_request_delta_evidence_v1(true, true, free_but_priced),
            RequestDeltaEvidenceState::Measured
        );
        assert_eq!(
            classify_request_delta_evidence_v1(false, true, free_but_priced),
            RequestDeltaEvidenceState::Unpriceable
        );

        let malformed = RequestDeltaInput {
            cost_usd: Some(f64::NAN),
            ..free_but_priced
        };
        assert_eq!(
            classify_request_delta_evidence_v1(true, true, malformed),
            RequestDeltaEvidenceState::MissingEvidence
        );

        for state in [
            RequestDeltaEvidenceState::Measured,
            RequestDeltaEvidenceState::Unpriceable,
            RequestDeltaEvidenceState::MissingEvidence,
        ] {
            assert_eq!(
                RequestDeltaEvidenceState::from_persisted(state.as_str()),
                Some(state)
            );
        }
        assert_eq!(RequestDeltaEvidenceState::from_persisted("future"), None);
    }

    #[test]
    fn receipt_fragment_rejects_partial_or_inconsistent_claims() {
        let valid = RequestDeltaReceiptFields {
            cost_micros: 70_000,
            baseline_micros: 180_000,
            saved_micros: 100_000,
            signed_request_delta_micros: 100_000,
            formula_version: REQUEST_DELTA_ESTIMATE_V1,
            eligible_requests: 1,
            measured_requests: 1,
        };
        for invalid in [
            RequestDeltaReceiptFields {
                saved_micros: 99_999,
                ..valid
            },
            RequestDeltaReceiptFields {
                measured_requests: 0,
                ..valid
            },
            RequestDeltaReceiptFields {
                eligible_requests: -1,
                measured_requests: 0,
                ..valid
            },
            RequestDeltaReceiptFields {
                formula_version: "tt.request-delta-estimate.v2",
                ..valid
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    fn assert_close(actual: f64, expected: f64, id: &str) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "{id}: {actual} != {expected}"
        );
    }
}
