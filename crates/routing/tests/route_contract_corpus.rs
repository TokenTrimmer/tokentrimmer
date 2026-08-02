//! Golden compatibility corpus for the public route-definition contract.
//!
//! The authoritative JSON lives in `docs/route-contract` so hosted consumers
//! can vendor the exact artifact associated with their pinned gateway revision.

use std::collections::{BTreeSet, HashSet};

use serde::Deserialize;
use serde_json::Value;
use tt_routing::{
    canonicalize_route_parts, canonicalize_route_value, CanonicalRoute, RouteValidationIssue,
    ROUTE_SCHEMA_ID, ROUTE_SCHEMA_VERSION,
};

const CORPUS: &str = include_str!("../../../docs/route-contract/tokentrimmer.route.v1.corpus.json");
const SCHEMA: &str = include_str!("../../../docs/route-contract/route-write.schema.json");
const CORPUS_FORMAT_ID: &str = "tokentrimmer.route-contract-corpus";
const CORPUS_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteContractCorpus {
    corpus: CorpusFormat,
    contract: ContractVersion,
    cases: Vec<CorpusCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFormat {
    id: String,
    version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractVersion {
    id: String,
    version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusCase {
    id: String,
    #[serde(default)]
    gateway: Option<Value>,
    #[serde(default)]
    control_plane: Option<ControlPlaneInput>,
    expected: ExpectedOutcomes,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlPlaneInput {
    #[serde(default)]
    schema_version: Option<u32>,
    name: String,
    priority: i32,
    enabled: bool,
    conditions: Value,
    target: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedOutcomes {
    #[serde(default)]
    gateway: Option<ExpectedOutcome>,
    #[serde(default)]
    control_plane: Option<ExpectedOutcome>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum ExpectedOutcome {
    Accepted { canonical: CanonicalExpectation },
    Rejected { issues: Vec<IssueExpectation> },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalExpectation {
    schema_version: u32,
    name: String,
    priority: u32,
    enabled: bool,
    conditions: Value,
    target: Value,
    canonical_hash: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct IssueExpectation {
    field: String,
    code: String,
}

#[test]
fn v1_corpus_matches_gateway_and_control_plane_canonicalizers() {
    let corpus: RouteContractCorpus =
        serde_json::from_str(CORPUS).expect("route contract corpus must remain valid JSON");

    assert_eq!(corpus.corpus.id, CORPUS_FORMAT_ID);
    assert_eq!(corpus.corpus.version, CORPUS_FORMAT_VERSION);
    assert_eq!(corpus.contract.id, ROUTE_SCHEMA_ID);
    assert_eq!(corpus.contract.version, ROUTE_SCHEMA_VERSION);
    assert!(
        !corpus.cases.is_empty(),
        "the corpus must carry representative cases"
    );

    let mut ids = HashSet::new();
    for case in &corpus.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate corpus case id: {}",
            case.id
        );
        assert!(
            case.gateway.is_some() || case.control_plane.is_some(),
            "case {} must specify at least one input projection",
            case.id
        );
        assert_projection_pairing(
            case.gateway.is_some(),
            case.expected.gateway.is_some(),
            &case.id,
            "gateway",
        );
        assert_projection_pairing(
            case.control_plane.is_some(),
            case.expected.control_plane.is_some(),
            &case.id,
            "control_plane",
        );

        if let (Some(input), Some(expected)) = (&case.gateway, &case.expected.gateway) {
            assert_outcome(
                &case.id,
                "gateway",
                canonicalize_route_value(input.clone()),
                expected,
            );
        }
        if let (Some(input), Some(expected)) = (&case.control_plane, &case.expected.control_plane) {
            assert_outcome(
                &case.id,
                "control_plane",
                canonicalize_route_parts(
                    input.schema_version,
                    input.name.clone(),
                    input.priority,
                    input.enabled,
                    input.conditions.clone(),
                    input.target.clone(),
                ),
                expected,
            );
        }
    }
}

#[test]
fn accepted_corpus_covers_every_structural_route_field() {
    let corpus: RouteContractCorpus =
        serde_json::from_str(CORPUS).expect("route contract corpus must remain valid JSON");
    let schema: Value =
        serde_json::from_str(SCHEMA).expect("generated route schema must remain valid JSON");

    let mut gateway_fields = BTreeSet::new();
    let mut condition_fields = BTreeSet::new();
    let mut action_fields = BTreeSet::new();
    let mut agentic_budget_fields = BTreeSet::new();
    let mut panel_fields = BTreeSet::new();
    let mut workflow_fields = BTreeSet::new();

    for case in &corpus.cases {
        let Some(ExpectedOutcome::Accepted { canonical }) = case.expected.gateway.as_ref() else {
            continue;
        };
        let gateway = case
            .gateway
            .as_ref()
            .expect("accepted gateway expectation must have an input");
        extend_object_keys(&mut gateway_fields, gateway, &case.id, "gateway");
        extend_object_keys(
            &mut condition_fields,
            &canonical.conditions,
            &case.id,
            "canonical conditions",
        );
        extend_object_keys(
            &mut action_fields,
            &canonical.target,
            &case.id,
            "canonical target",
        );
        extend_nested_keys(
            &mut agentic_budget_fields,
            &canonical.target,
            "agentic_budget",
            &case.id,
        );
        extend_nested_keys(&mut panel_fields, &canonical.target, "panel", &case.id);
        extend_nested_keys(
            &mut workflow_fields,
            &canonical.target,
            "workflow",
            &case.id,
        );
    }

    assert_eq!(
        gateway_fields,
        schema_property_names(&schema, "/properties"),
        "accepted gateway cases must exercise every route-write envelope field"
    );
    assert_eq!(
        condition_fields,
        schema_property_names(&schema, "/$defs/RouteConditions/properties"),
        "accepted cases must exercise every canonical route-condition field"
    );
    assert_eq!(
        action_fields,
        schema_property_names(&schema, "/$defs/RouteAction/properties"),
        "accepted cases must exercise every canonical route-action field"
    );
    assert_eq!(
        agentic_budget_fields,
        schema_property_names(&schema, "/$defs/AgenticBudget/properties"),
        "accepted cases must exercise every agentic-budget field"
    );
    assert_eq!(
        panel_fields,
        schema_property_names(&schema, "/$defs/RoutePanel/properties"),
        "accepted cases must exercise every Fusion-panel field"
    );
    assert_eq!(
        workflow_fields,
        schema_property_names(&schema, "/$defs/RouteWorkflow/properties"),
        "accepted cases must exercise every governed-workflow field"
    );
}

fn extend_object_keys(
    destination: &mut BTreeSet<String>,
    value: &Value,
    case_id: &str,
    field: &str,
) {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("case {case_id} {field} must be an object"));
    destination.extend(object.keys().cloned());
}

fn extend_nested_keys(
    destination: &mut BTreeSet<String>,
    target: &Value,
    field: &str,
    case_id: &str,
) {
    let Some(value) = target.get(field) else {
        return;
    };
    extend_object_keys(destination, value, case_id, field);
}

fn schema_property_names(schema: &Value, pointer: &str) -> BTreeSet<String> {
    schema
        .pointer(pointer)
        .unwrap_or_else(|| panic!("generated schema is missing {pointer}"))
        .as_object()
        .unwrap_or_else(|| panic!("generated schema {pointer} must be an object"))
        .keys()
        .cloned()
        .collect()
}

fn assert_projection_pairing(has_input: bool, has_expected: bool, case_id: &str, surface: &str) {
    assert_eq!(
        has_input, has_expected,
        "case {case_id} must specify both {surface} input and expected outcome"
    );
}

fn assert_outcome(
    case_id: &str,
    surface: &str,
    actual: Result<CanonicalRoute, Vec<RouteValidationIssue>>,
    expected: &ExpectedOutcome,
) {
    match expected {
        ExpectedOutcome::Accepted { canonical } => {
            let actual = actual.unwrap_or_else(|issues| {
                panic!("case {case_id} {surface} should be accepted, got {issues:?}")
            });
            assert_eq!(
                actual.schema_version, canonical.schema_version,
                "case {case_id} {surface}"
            );
            assert_eq!(
                actual.route.name, canonical.name,
                "case {case_id} {surface}"
            );
            assert_eq!(
                actual.route.priority, canonical.priority,
                "case {case_id} {surface}"
            );
            assert_eq!(
                actual.route.enabled, canonical.enabled,
                "case {case_id} {surface}"
            );
            assert_eq!(
                actual.conditions, canonical.conditions,
                "case {case_id} {surface}"
            );
            assert_eq!(actual.target, canonical.target, "case {case_id} {surface}");
            assert_eq!(
                actual.canonical_hash, canonical.canonical_hash,
                "case {case_id} {surface}"
            );
        }
        ExpectedOutcome::Rejected { issues } => {
            let actual = actual.expect_err("corpus rejection must not canonicalize");
            let actual: Vec<IssueExpectation> = actual
                .into_iter()
                .map(|issue| IssueExpectation {
                    field: issue.field,
                    code: issue.code,
                })
                .collect();
            assert_eq!(
                actual.as_slice(),
                issues.as_slice(),
                "case {case_id} {surface}"
            );
        }
    }
}
