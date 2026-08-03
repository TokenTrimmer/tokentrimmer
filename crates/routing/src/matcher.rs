//! Canonical route-condition feature snapshot and decision evaluation.
//!
//! The snapshot deliberately cannot be serialized or formatted with `Debug`:
//! it may contain request text and tags. The exported decision records contain
//! only condition field names and outcomes, so callers can retain a bounded
//! explanation without retaining the underlying customer data.

use serde::Serialize;
use tt_shared::{ChatCompletionRequest, RequestContext};

use crate::{Route, RouteConditions};

/// Canonical route-condition field identifiers, in wire declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteConditionField {
    /// Requested model membership.
    ModelIn,
    /// Input-token estimate is strictly below a threshold.
    InputTokensLt,
    /// Input-token estimate is strictly above a threshold.
    InputTokensGt,
    /// Exact request tag.
    TagEquals,
    /// Image-input presence.
    HasImages,
    /// Audio-input presence.
    HasAudio,
    /// Document-input presence.
    HasDocuments,
    /// Dominant classified content kind.
    ContentType,
    /// Case-insensitive input-text keyword membership.
    PromptContainsAnyOf,
    /// Pre-dispatch estimated cost is strictly above a threshold.
    EstimatedCostGt,
    /// Pre-dispatch estimated cost is strictly below a threshold.
    EstimatedCostLt,
    /// Live upstream p95 latency is strictly above a threshold.
    UpstreamLatencyMsP95Gt,
    /// Request is not classified as reasoning-is-the-work.
    NotReasoningClass,
}

impl RouteConditionField {
    /// Every canonical condition field, in `RouteConditions` wire order.
    pub const ALL: [Self; 13] = [
        Self::ModelIn,
        Self::InputTokensLt,
        Self::InputTokensGt,
        Self::TagEquals,
        Self::HasImages,
        Self::HasAudio,
        Self::HasDocuments,
        Self::ContentType,
        Self::PromptContainsAnyOf,
        Self::EstimatedCostGt,
        Self::EstimatedCostLt,
        Self::UpstreamLatencyMsP95Gt,
        Self::NotReasoningClass,
    ];

    /// Stable snake-case field name used by the route wire contract.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelIn => "model_in",
            Self::InputTokensLt => "input_tokens_lt",
            Self::InputTokensGt => "input_tokens_gt",
            Self::TagEquals => "tag_equals",
            Self::HasImages => "has_images",
            Self::HasAudio => "has_audio",
            Self::HasDocuments => "has_documents",
            Self::ContentType => "content_type",
            Self::PromptContainsAnyOf => "prompt_contains_any_of",
            Self::EstimatedCostGt => "estimated_cost_gt",
            Self::EstimatedCostLt => "estimated_cost_lt",
            Self::UpstreamLatencyMsP95Gt => "upstream_latency_ms_p95_gt",
            Self::NotReasoningClass => "not_reasoning_class",
        }
    }
}

/// Result of one canonical condition evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteConditionOutcome {
    /// The condition is a wire-valid no-op and does not constrain the route.
    Inactive,
    /// The condition is active and its observed feature matched.
    Matched,
    /// The condition is active and its observed feature did not match.
    NotMatched,
    /// The condition is active but the required feature was not observed.
    Unavailable,
}

impl RouteConditionOutcome {
    const fn permits_route(self) -> bool {
        matches!(self, Self::Inactive | Self::Matched)
    }
}

/// Evidence quality behind an active condition decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteFeatureEvidence {
    /// No feature was needed because the condition is inactive.
    NotRequired,
    /// The snapshot carries the exact feature used by live routing.
    Exact,
    /// The snapshot carries a conservative proxy rather than the live feature.
    Approximate,
    /// The snapshot does not carry the feature required for this condition.
    Unavailable,
}

/// Value-free decision for one condition field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteConditionDecision {
    /// Canonical condition field.
    pub field: RouteConditionField,
    /// Whether it was inactive, matched, did not match, or lacked a feature.
    pub outcome: RouteConditionOutcome,
    /// Exact, approximate, unavailable, or not required evidence.
    pub evidence: RouteFeatureEvidence,
}

/// Canonical evaluation of every condition field for one route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteConditionEvaluation {
    /// Exactly one value-free decision per canonical condition field.
    pub decisions: Vec<RouteConditionDecision>,
}

impl RouteConditionEvaluation {
    /// Whether every active condition matched an observed feature.
    #[must_use]
    pub fn matches(&self) -> bool {
        self.decisions
            .iter()
            .all(|decision| decision.outcome.permits_route())
    }
}

#[derive(Clone)]
enum Observation<T> {
    Observed {
        value: T,
        evidence: RouteFeatureEvidence,
    },
    Unavailable,
}

impl<T> Observation<T> {
    fn exact(value: T) -> Self {
        Self::Observed {
            value,
            evidence: RouteFeatureEvidence::Exact,
        }
    }

    fn approximate(value: T) -> Self {
        Self::Observed {
            value,
            evidence: RouteFeatureEvidence::Approximate,
        }
    }

    fn evaluate(&self, predicate: impl FnOnce(&T) -> bool) -> ConditionResult {
        match self {
            Self::Observed { value, evidence } => ConditionResult {
                outcome: if predicate(value) {
                    RouteConditionOutcome::Matched
                } else {
                    RouteConditionOutcome::NotMatched
                },
                evidence: *evidence,
            },
            Self::Unavailable => ConditionResult::unavailable(),
        }
    }
}

#[derive(Clone, Copy)]
struct ConditionResult {
    outcome: RouteConditionOutcome,
    evidence: RouteFeatureEvidence,
}

impl ConditionResult {
    const fn inactive() -> Self {
        Self {
            outcome: RouteConditionOutcome::Inactive,
            evidence: RouteFeatureEvidence::NotRequired,
        }
    }

    const fn unavailable() -> Self {
        Self {
            outcome: RouteConditionOutcome::Unavailable,
            evidence: RouteFeatureEvidence::Unavailable,
        }
    }
}

/// Exact or conservatively partial feature inputs for canonical route matching.
///
/// This type intentionally implements neither `Debug` nor `Serialize`: it may
/// hold lowercased request text and a tenant tag. Use
/// [`RouteConditionEvaluation`] for a safe, value-free explanation.
#[derive(Clone)]
pub struct RouteFeatureSnapshot {
    model: Observation<String>,
    input_tokens: Observation<u32>,
    estimated_cost_usd: Observation<f64>,
    observed_p95_ms: Observation<u32>,
    is_reasoning_class: Observation<bool>,
    tag: Observation<Option<String>>,
    has_images: Observation<bool>,
    has_audio: Observation<bool>,
    has_documents: Observation<bool>,
    content_type: Observation<Option<String>>,
    input_text_lowercase: Observation<String>,
}

#[derive(Debug, Clone, Copy)]
struct FeatureRequirements {
    has_images: bool,
    has_audio: bool,
    has_documents: bool,
    content_type: bool,
    input_text: bool,
}

impl FeatureRequirements {
    const ALL: Self = Self {
        has_images: true,
        has_audio: true,
        has_documents: true,
        content_type: true,
        input_text: true,
    };

    fn for_routes(routes: &[Route]) -> Self {
        Self {
            has_images: routes
                .iter()
                .any(|route| route.enabled && route.when.has_images.is_some()),
            has_audio: routes
                .iter()
                .any(|route| route.enabled && route.when.has_audio.is_some()),
            has_documents: routes
                .iter()
                .any(|route| route.enabled && route.when.has_documents.is_some()),
            content_type: routes
                .iter()
                .any(|route| route.enabled && route.when.content_type.is_some()),
            input_text: routes
                .iter()
                .any(|route| route.enabled && !route.when.prompt_contains_any_of.is_empty()),
        }
    }
}

impl RouteFeatureSnapshot {
    /// Capture every live request feature used by the canonical matcher.
    #[must_use]
    pub fn from_request(
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens: u32,
        estimated_cost_usd: Option<f64>,
        observed_p95_ms: Option<u32>,
        is_reasoning_class: bool,
    ) -> Self {
        Self::from_request_with_requirements(
            req,
            ctx,
            input_tokens,
            estimated_cost_usd,
            observed_p95_ms,
            is_reasoning_class,
            FeatureRequirements::ALL,
        )
    }

    pub(crate) fn for_engine(
        routes: &[Route],
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens: u32,
        estimated_cost_usd: Option<f64>,
        observed_p95_ms: Option<u32>,
        is_reasoning_class: bool,
    ) -> Self {
        Self::from_request_with_requirements(
            req,
            ctx,
            input_tokens,
            estimated_cost_usd,
            observed_p95_ms,
            is_reasoning_class,
            FeatureRequirements::for_routes(routes),
        )
    }

    fn from_request_with_requirements(
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens: u32,
        estimated_cost_usd: Option<f64>,
        observed_p95_ms: Option<u32>,
        is_reasoning_class: bool,
        requirements: FeatureRequirements,
    ) -> Self {
        Self {
            model: Observation::exact(req.model.clone()),
            input_tokens: Observation::exact(input_tokens),
            estimated_cost_usd: estimated_cost_usd
                .map_or(Observation::Unavailable, Observation::exact),
            observed_p95_ms: observed_p95_ms.map_or(Observation::Unavailable, Observation::exact),
            is_reasoning_class: Observation::exact(is_reasoning_class),
            tag: Observation::exact(ctx.tag.clone()),
            has_images: requirements
                .has_images
                .then(|| tt_shared::capability_check::request_has_images(req))
                .map_or(Observation::Unavailable, Observation::exact),
            has_audio: requirements
                .has_audio
                .then(|| tt_shared::capability_check::request_has_audio(req))
                .map_or(Observation::Unavailable, Observation::exact),
            has_documents: requirements
                .has_documents
                .then(|| tt_shared::capability_check::request_has_documents(req))
                .map_or(Observation::Unavailable, Observation::exact),
            content_type: requirements
                .content_type
                .then(|| {
                    tt_shared::capability_check::request_dominant_content_kind(req)
                        .map(|kind| kind.as_str().to_owned())
                })
                .map_or(Observation::Unavailable, Observation::exact),
            input_text_lowercase: requirements
                .input_text
                .then(|| tt_shared::capability_check::request_input_text(req).to_lowercase())
                .map_or(Observation::Unavailable, Observation::exact),
        }
    }

    /// Build a conservative snapshot from facts commonly retained in request
    /// logs. Modality, content classification, exact prompt text, live latency,
    /// and reasoning classification remain unavailable until explicitly added.
    #[must_use]
    pub fn from_retained_features(model: String, input_tokens: u32, tag: Option<String>) -> Self {
        Self::from_partial_retained_features(Some(model), input_tokens, tag)
    }

    /// Build a conservative snapshot from a partially covered historical row.
    ///
    /// `requested_model` must be the exact pre-routing caller model snapshot;
    /// pass `None` for legacy rows instead of substituting a served model.
    /// Realized provider input tokens remain an approximate proxy for the
    /// gateway's pre-dispatch estimate. Features not named here are unavailable.
    #[must_use]
    pub fn from_partial_retained_features(
        requested_model: Option<String>,
        input_tokens: u32,
        tag: Option<String>,
    ) -> Self {
        Self {
            model: requested_model.map_or(Observation::Unavailable, Observation::exact),
            // Request logs retain realized provider tokens, while live routing
            // compares its pre-dispatch tokenizer estimate.
            input_tokens: Observation::approximate(input_tokens),
            estimated_cost_usd: Observation::Unavailable,
            observed_p95_ms: Observation::Unavailable,
            is_reasoning_class: Observation::Unavailable,
            tag: Observation::exact(tag),
            has_images: Observation::Unavailable,
            has_audio: Observation::Unavailable,
            has_documents: Observation::Unavailable,
            content_type: Observation::Unavailable,
            input_text_lowercase: Observation::Unavailable,
        }
    }

    /// Add an approximate proxy for the gateway's pre-dispatch cost estimate.
    #[must_use]
    pub fn with_approximate_estimated_cost_usd(mut self, estimated_cost_usd: f64) -> Self {
        self.estimated_cost_usd = Observation::approximate(estimated_cost_usd);
        self
    }

    /// Add the exact pre-dispatch cost estimate used by live routing.
    #[must_use]
    pub fn with_exact_estimated_cost_usd(mut self, estimated_cost_usd: f64) -> Self {
        self.estimated_cost_usd = Observation::exact(estimated_cost_usd);
        self
    }

    /// Add exact modality observations to a partial snapshot.
    #[must_use]
    pub fn with_modalities(
        mut self,
        has_images: bool,
        has_audio: bool,
        has_documents: bool,
    ) -> Self {
        self.has_images = Observation::exact(has_images);
        self.has_audio = Observation::exact(has_audio);
        self.has_documents = Observation::exact(has_documents);
        self
    }

    /// Add an exact dominant-content observation; `None` means observed but
    /// unclassifiable, which is distinct from an unavailable feature.
    #[must_use]
    pub fn with_content_type(mut self, content_type: Option<String>) -> Self {
        self.content_type = Observation::exact(content_type);
        self
    }

    /// Add the exact user/system input text used by the gateway matcher.
    #[must_use]
    pub fn with_input_text(mut self, input_text: &str) -> Self {
        self.input_text_lowercase = Observation::exact(input_text.to_lowercase());
        self
    }

    /// Add a live observed upstream p95 latency.
    #[must_use]
    pub fn with_observed_p95_ms(mut self, observed_p95_ms: u32) -> Self {
        self.observed_p95_ms = Observation::exact(observed_p95_ms);
        self
    }

    /// Add the exact gateway reasoning-class decision.
    #[must_use]
    pub fn with_reasoning_class(mut self, is_reasoning_class: bool) -> Self {
        self.is_reasoning_class = Observation::exact(is_reasoning_class);
        self
    }
}

/// Evaluate every canonical condition field without exposing feature values.
#[must_use]
pub fn evaluate_route_conditions(
    conditions: &RouteConditions,
    features: &RouteFeatureSnapshot,
) -> RouteConditionEvaluation {
    RouteConditionEvaluation {
        decisions: RouteConditionField::ALL
            .into_iter()
            .map(|field| {
                let result = condition_result(field, conditions, features);
                RouteConditionDecision {
                    field,
                    outcome: result.outcome,
                    evidence: result.evidence,
                }
            })
            .collect(),
    }
}

/// Match one route condition set against the canonical feature snapshot.
///
/// This is the same allocation-free condition routine used by
/// [`crate::RoutingEngine`]. Active conditions with unavailable features fail
/// closed rather than being ignored or inferred from another retained field.
#[must_use]
pub fn route_conditions_match(
    conditions: &RouteConditions,
    features: &RouteFeatureSnapshot,
) -> bool {
    RouteConditionField::ALL.into_iter().all(|field| {
        condition_result(field, conditions, features)
            .outcome
            .permits_route()
    })
}

fn condition_result(
    field: RouteConditionField,
    c: &RouteConditions,
    f: &RouteFeatureSnapshot,
) -> ConditionResult {
    match field {
        RouteConditionField::ModelIn => {
            if c.model_in.is_empty() {
                ConditionResult::inactive()
            } else {
                f.model
                    .evaluate(|model| c.model_in.iter().any(|candidate| candidate == model))
            }
        }
        RouteConditionField::InputTokensLt => c
            .input_tokens_lt
            .map_or(ConditionResult::inactive(), |threshold| {
                f.input_tokens.evaluate(|tokens| *tokens < threshold)
            }),
        RouteConditionField::InputTokensGt => c
            .input_tokens_gt
            .map_or(ConditionResult::inactive(), |threshold| {
                f.input_tokens.evaluate(|tokens| *tokens > threshold)
            }),
        RouteConditionField::TagEquals => c
            .tag_equals
            .as_ref()
            .map_or(ConditionResult::inactive(), |tag| {
                f.tag.evaluate(|observed| observed.as_ref() == Some(tag))
            }),
        RouteConditionField::HasImages => observed_optional_bool(c.has_images, &f.has_images),
        RouteConditionField::HasAudio => observed_optional_bool(c.has_audio, &f.has_audio),
        RouteConditionField::HasDocuments => {
            observed_optional_bool(c.has_documents, &f.has_documents)
        }
        RouteConditionField::ContentType => {
            c.content_type
                .as_ref()
                .map_or(ConditionResult::inactive(), |wanted| {
                    f.content_type
                        .evaluate(|observed| observed.as_ref() == Some(wanted))
                })
        }
        RouteConditionField::PromptContainsAnyOf => {
            if c.prompt_contains_any_of.is_empty() {
                ConditionResult::inactive()
            } else {
                f.input_text_lowercase.evaluate(|text| {
                    c.prompt_contains_any_of
                        .iter()
                        .any(|keyword| text.contains(&keyword.to_lowercase()))
                })
            }
        }
        RouteConditionField::EstimatedCostGt => c
            .estimated_cost_gt
            .map_or(ConditionResult::inactive(), |threshold| {
                f.estimated_cost_usd.evaluate(|cost| *cost > threshold)
            }),
        RouteConditionField::EstimatedCostLt => c
            .estimated_cost_lt
            .map_or(ConditionResult::inactive(), |threshold| {
                f.estimated_cost_usd.evaluate(|cost| *cost < threshold)
            }),
        RouteConditionField::UpstreamLatencyMsP95Gt => c
            .upstream_latency_ms_p95_gt
            .map_or(ConditionResult::inactive(), |threshold| {
                f.observed_p95_ms.evaluate(|p95| *p95 > threshold)
            }),
        RouteConditionField::NotReasoningClass => {
            if c.not_reasoning_class {
                f.is_reasoning_class.evaluate(|reasoning| !reasoning)
            } else {
                ConditionResult::inactive()
            }
        }
    }
}

fn observed_optional_bool(wanted: Option<bool>, observed: &Observation<bool>) -> ConditionResult {
    wanted.map_or(ConditionResult::inactive(), |wanted| {
        observed.evaluate(|actual| *actual == wanted)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_snapshot_names_unavailable_features_without_values() {
        let conditions = RouteConditions {
            model_in: vec!["gpt-4o".into()],
            input_tokens_lt: Some(500),
            tag_equals: Some("production".into()),
            has_images: Some(false),
            content_type: Some("code".into()),
            prompt_contains_any_of: vec!["secret".into()],
            estimated_cost_gt: Some(0.01),
            upstream_latency_ms_p95_gt: Some(250),
            not_reasoning_class: true,
            ..Default::default()
        };
        let snapshot = RouteFeatureSnapshot::from_retained_features(
            "gpt-4o".into(),
            100,
            Some("production".into()),
        )
        .with_approximate_estimated_cost_usd(0.02);
        let evaluation = evaluate_route_conditions(&conditions, &snapshot);

        assert!(!evaluation.matches());
        assert_eq!(evaluation.decisions.len(), RouteConditionField::ALL.len());
        for field in [
            RouteConditionField::HasImages,
            RouteConditionField::ContentType,
            RouteConditionField::PromptContainsAnyOf,
            RouteConditionField::UpstreamLatencyMsP95Gt,
            RouteConditionField::NotReasoningClass,
        ] {
            assert_eq!(
                evaluation
                    .decisions
                    .iter()
                    .find(|decision| decision.field == field)
                    .map(|decision| decision.outcome),
                Some(RouteConditionOutcome::Unavailable)
            );
        }

        let serialized = serde_json::to_string(&evaluation).unwrap();
        assert!(!serialized.contains("gpt-4o"));
        assert!(!serialized.contains("production"));
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn adding_observed_features_produces_one_canonical_full_match() {
        let conditions = RouteConditions {
            model_in: vec!["gpt-4o".into()],
            input_tokens_lt: Some(500),
            input_tokens_gt: Some(10),
            tag_equals: Some("production".into()),
            has_images: Some(false),
            has_audio: Some(false),
            has_documents: Some(false),
            content_type: Some("code".into()),
            prompt_contains_any_of: vec!["refactor".into()],
            estimated_cost_gt: Some(0.01),
            estimated_cost_lt: Some(1.0),
            upstream_latency_ms_p95_gt: Some(250),
            not_reasoning_class: true,
        };
        let snapshot = RouteFeatureSnapshot::from_retained_features(
            "gpt-4o".into(),
            100,
            Some("production".into()),
        )
        .with_exact_estimated_cost_usd(0.02)
        .with_modalities(false, false, false)
        .with_content_type(Some("code".into()))
        .with_input_text("Please REFACTOR this")
        .with_observed_p95_ms(300)
        .with_reasoning_class(false);
        let evaluation = evaluate_route_conditions(&conditions, &snapshot);

        assert!(evaluation.matches());
        assert!(evaluation
            .decisions
            .iter()
            .all(|decision| decision.outcome == RouteConditionOutcome::Matched));
        let input_decision = evaluation
            .decisions
            .iter()
            .find(|decision| decision.field == RouteConditionField::InputTokensLt)
            .unwrap();
        assert_eq!(input_decision.evidence, RouteFeatureEvidence::Approximate);
    }

    #[test]
    fn prompt_matching_uses_runtime_unicode_lowercase() {
        let conditions = RouteConditions {
            prompt_contains_any_of: vec!["CAFÉ".into()],
            ..Default::default()
        };
        let snapshot = RouteFeatureSnapshot::from_retained_features("gpt-4o".into(), 10, None)
            .with_input_text("Please review the Café invoice");

        let evaluation = evaluate_route_conditions(&conditions, &snapshot);

        assert!(evaluation.matches());
        assert_eq!(
            evaluation
                .decisions
                .iter()
                .find(|decision| decision.field == RouteConditionField::PromptContainsAnyOf)
                .map(|decision| decision.outcome),
            Some(RouteConditionOutcome::Matched)
        );
    }

    #[test]
    fn missing_requested_model_is_unavailable_not_served_model_inference() {
        let conditions = RouteConditions {
            model_in: vec!["served-model".into()],
            ..Default::default()
        };
        let snapshot = RouteFeatureSnapshot::from_partial_retained_features(None, 100, None);
        let evaluation = evaluate_route_conditions(&conditions, &snapshot);

        assert!(!evaluation.matches());
        let model = evaluation
            .decisions
            .iter()
            .find(|decision| decision.field == RouteConditionField::ModelIn)
            .unwrap();
        assert_eq!(model.outcome, RouteConditionOutcome::Unavailable);
        assert_eq!(model.evidence, RouteFeatureEvidence::Unavailable);
    }
}
