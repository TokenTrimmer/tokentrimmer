//! Golden compatibility corpus for historical route-preview coverage.
//!
//! The authoritative JSON lives beside the public contract docs so hosted
//! consumers can vendor the exact artifact associated with their pinned gateway
//! revision. This test deliberately constructs every `RouteConditions` field:
//! adding a route predicate therefore requires an explicit preview-coverage
//! decision rather than silently falling through to a historical SQL query.

use std::collections::HashSet;

use serde::Deserialize;
use tt_routing::{RouteConditions, ROUTE_SCHEMA_ID, ROUTE_SCHEMA_VERSION};

const CORPUS: &str = include_str!(
    "../../../docs/route-preview-contract/tokentrimmer.route-preview-coverage.v1.corpus.json"
);
const V2_CORPUS: &str = include_str!(
    "../../../docs/route-preview-contract/tokentrimmer.route-preview-coverage.v2.corpus.json"
);
const CORPUS_FORMAT_ID: &str = "tokentrimmer.route-preview-coverage-corpus";
const CORPUS_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutePreviewCoverageCorpus {
    corpus: CorpusFormat,
    route_contract: RouteContractVersion,
    conditions: Vec<ConditionCoverage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFormat {
    id: String,
    version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteContractVersion {
    id: String,
    version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConditionCoverage {
    field: String,
    classification: CoverageClassification,
    reason_id: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CoverageClassification {
    Exact,
    Approximate,
    Unavailable,
}

const EXPECTED_CONDITIONS: [(&str, CoverageClassification, &str); 13] = [
    (
        "model_in",
        CoverageClassification::Unavailable,
        "served_model_not_request_model",
    ),
    (
        "input_tokens_lt",
        CoverageClassification::Approximate,
        "realized_input_tokens_not_gateway_estimate",
    ),
    (
        "input_tokens_gt",
        CoverageClassification::Approximate,
        "realized_input_tokens_not_gateway_estimate",
    ),
    ("tag_equals", CoverageClassification::Exact, "tag_retained"),
    (
        "has_images",
        CoverageClassification::Unavailable,
        "image_presence_not_retained",
    ),
    (
        "has_audio",
        CoverageClassification::Unavailable,
        "audio_presence_not_retained",
    ),
    (
        "has_documents",
        CoverageClassification::Unavailable,
        "document_presence_not_retained",
    ),
    (
        "content_type",
        CoverageClassification::Unavailable,
        "content_type_not_retained",
    ),
    (
        "prompt_contains_any_of",
        CoverageClassification::Unavailable,
        "prompt_content_not_retained",
    ),
    (
        "estimated_cost_gt",
        CoverageClassification::Unavailable,
        "gateway_cost_estimate_not_retained",
    ),
    (
        "estimated_cost_lt",
        CoverageClassification::Unavailable,
        "gateway_cost_estimate_not_retained",
    ),
    (
        "upstream_latency_ms_p95_gt",
        CoverageClassification::Unavailable,
        "live_latency_not_retained",
    ),
    (
        "not_reasoning_class",
        CoverageClassification::Unavailable,
        "reasoning_classification_not_retained",
    ),
];

const V2_EXPECTED_CONDITIONS: [(&str, CoverageClassification, &str); 13] = [
    (
        "model_in",
        CoverageClassification::Exact,
        "requested_model_snapshot_retained",
    ),
    (
        "input_tokens_lt",
        CoverageClassification::Approximate,
        "realized_input_tokens_not_gateway_estimate",
    ),
    (
        "input_tokens_gt",
        CoverageClassification::Approximate,
        "realized_input_tokens_not_gateway_estimate",
    ),
    ("tag_equals", CoverageClassification::Exact, "tag_retained"),
    (
        "has_images",
        CoverageClassification::Unavailable,
        "image_presence_not_retained",
    ),
    (
        "has_audio",
        CoverageClassification::Unavailable,
        "audio_presence_not_retained",
    ),
    (
        "has_documents",
        CoverageClassification::Unavailable,
        "document_presence_not_retained",
    ),
    (
        "content_type",
        CoverageClassification::Unavailable,
        "content_type_not_retained",
    ),
    (
        "prompt_contains_any_of",
        CoverageClassification::Unavailable,
        "prompt_content_not_retained",
    ),
    (
        "estimated_cost_gt",
        CoverageClassification::Unavailable,
        "gateway_cost_estimate_not_retained",
    ),
    (
        "estimated_cost_lt",
        CoverageClassification::Unavailable,
        "gateway_cost_estimate_not_retained",
    ),
    (
        "upstream_latency_ms_p95_gt",
        CoverageClassification::Unavailable,
        "live_latency_not_retained",
    ),
    (
        "not_reasoning_class",
        CoverageClassification::Unavailable,
        "reasoning_classification_not_retained",
    ),
];

#[test]
fn v1_corpus_covers_each_canonical_route_condition_once() {
    assert_corpus(CORPUS, CORPUS_FORMAT_VERSION, EXPECTED_CONDITIONS);
}

#[test]
fn v2_corpus_covers_each_canonical_route_condition_once() {
    assert_corpus(V2_CORPUS, 2, V2_EXPECTED_CONDITIONS);
}

fn assert_corpus(
    raw: &str,
    expected_version: u32,
    expected_conditions: [(&str, CoverageClassification, &str); 13],
) {
    let corpus: RoutePreviewCoverageCorpus =
        serde_json::from_str(raw).expect("route-preview coverage corpus must remain valid JSON");

    assert_eq!(corpus.corpus.id, CORPUS_FORMAT_ID);
    assert_eq!(corpus.corpus.version, expected_version);
    assert_eq!(corpus.route_contract.id, ROUTE_SCHEMA_ID);
    assert_eq!(corpus.route_contract.version, ROUTE_SCHEMA_VERSION);
    assert_eq!(
        corpus.conditions.len(),
        expected_conditions.len(),
        "the corpus must carry exactly one entry for every canonical condition"
    );

    let canonical_fields = canonical_route_condition_fields();
    let mut seen_fields = HashSet::new();
    for (actual, (field, classification, reason_id)) in
        corpus.conditions.iter().zip(expected_conditions)
    {
        assert!(
            seen_fields.insert(actual.field.as_str()),
            "duplicate route-preview coverage field: {}",
            actual.field
        );
        assert!(
            canonical_fields.contains(&actual.field),
            "coverage field {} is not a canonical RouteConditions field",
            actual.field
        );
        assert_eq!(
            actual.field, field,
            "corpus condition order must remain canonical"
        );
        assert_eq!(
            actual.classification, classification,
            "corpus classification for {field}"
        );
        assert_eq!(actual.reason_id, reason_id, "corpus reason id for {field}");
        assert!(
            is_reason_id(&actual.reason_id),
            "reason id for {field} must be lowercase snake case"
        );
    }

    let covered_fields: HashSet<String> = corpus
        .conditions
        .iter()
        .map(|condition| condition.field.clone())
        .collect();
    assert_eq!(
        covered_fields, canonical_fields,
        "every canonical RouteConditions field must have exactly one coverage entry"
    );
}

fn canonical_route_condition_fields() -> HashSet<String> {
    // Do not add `..Default::default()`: this is an intentional compile-time
    // lockstep guard. A new RouteConditions field must force a corpus decision.
    let conditions = RouteConditions {
        model_in: vec!["requested-model".to_owned()],
        input_tokens_lt: Some(1_000),
        input_tokens_gt: Some(10),
        tag_equals: Some("preview-tag".to_owned()),
        has_images: Some(true),
        has_audio: Some(false),
        has_documents: Some(true),
        content_type: Some("code".to_owned()),
        prompt_contains_any_of: vec!["preview".to_owned()],
        estimated_cost_gt: Some(0.01),
        estimated_cost_lt: Some(5.0),
        upstream_latency_ms_p95_gt: Some(1_500),
        not_reasoning_class: true,
    };
    let serialized =
        serde_json::to_value(conditions).expect("representative RouteConditions must serialize");
    let fields = serialized
        .as_object()
        .expect("RouteConditions must serialize as an object");

    fields.keys().cloned().collect::<HashSet<_>>()
}

fn is_reason_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}
