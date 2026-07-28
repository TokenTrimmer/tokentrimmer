//! Golden compatibility corpus for the authenticated gateway capabilities
//! document. The authoritative bytes live beside the public contract docs so
//! hosted consumers can vendor them with the gateway revision they pin.

use std::collections::HashSet;

use chrono::{TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;
use tt_cli::capabilities::parse_snapshot;
use tt_core::{
    routes::capabilities::{build_document, CAPABILITIES_SCHEMA_VERSION},
    AppState, ProviderRegistry,
};
use tt_shared::CallerTier;

const CORPUS: &str = include_str!(
    "../../../docs/capability-contract/tokentrimmer.gateway-capabilities.v1.corpus.json"
);
const CORPUS_FORMAT_ID: &str = "tokentrimmer.gateway-capabilities-contract-corpus";
const CORPUS_FORMAT_VERSION: u32 = 1;
const CONTRACT_ID: &str = "tokentrimmer.gateway-capabilities.v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityContractCorpus {
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
    document: Value,
    expected: ExpectedConsumers,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedConsumers {
    cli: ExpectedOutcome,
    dashboard: ExpectedOutcome,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum ExpectedOutcome {
    Accepted { fusion: Value },
    Rejected {},
}

fn parse_corpus() -> CapabilityContractCorpus {
    serde_json::from_str(CORPUS).expect("capability contract corpus must remain valid JSON")
}

#[test]
fn v1_corpus_matches_cli_parser() {
    let corpus = parse_corpus();

    assert_eq!(corpus.corpus.id, CORPUS_FORMAT_ID);
    assert_eq!(corpus.corpus.version, CORPUS_FORMAT_VERSION);
    assert_eq!(corpus.contract.id, CONTRACT_ID);
    assert_eq!(corpus.contract.version, CAPABILITIES_SCHEMA_VERSION);
    assert!(
        !corpus.cases.is_empty(),
        "the capability corpus must carry representative cases"
    );

    let mut ids = HashSet::new();
    for case in &corpus.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate corpus case id: {}",
            case.id
        );
        assert_cli_outcome(case);
        assert_dashboard_projection_is_present(case);
    }
}

fn assert_cli_outcome(case: &CorpusCase) {
    let document = serde_json::to_vec(&case.document)
        .unwrap_or_else(|error| panic!("case {} document must serialize: {error}", case.id));
    let parsed = parse_snapshot(&document);

    match &case.expected.cli {
        ExpectedOutcome::Accepted { fusion } => {
            let snapshot = parsed.unwrap_or_else(|error| {
                panic!(
                    "case {} CLI projection should be accepted: {error}",
                    case.id
                )
            });
            let actual = serde_json::to_value(&snapshot.fusion)
                .unwrap_or_else(|error| panic!("case {} fusion must serialize: {error}", case.id));
            assert_eq!(actual, *fusion, "case {} CLI fusion projection", case.id);
        }
        ExpectedOutcome::Rejected {} => {
            assert!(
                parsed.is_err(),
                "case {} CLI projection should be rejected",
                case.id
            );
        }
    }
}

fn assert_dashboard_projection_is_present(case: &CorpusCase) {
    match &case.expected.dashboard {
        ExpectedOutcome::Accepted { fusion } => assert!(
            fusion.is_object(),
            "case {} accepted dashboard projection must carry Fusion evidence",
            case.id
        ),
        ExpectedOutcome::Rejected {} => {}
    }
}

#[test]
fn gateway_builder_documents_match_source_shaped_corpus_vectors() {
    let corpus = parse_corpus();
    let generated_at = Utc.with_ymd_and_hms(2026, 7, 17, 12, 0, 0).unwrap();
    let cases = [
        (
            "enabled_authenticated_tier_gate_passes",
            true,
            Some(CallerTier::Pro),
        ),
        (
            "disabled_switch_is_unavailable",
            false,
            Some(CallerTier::Pro),
        ),
        ("tier_below_minimum_is_unavailable", true, None),
    ];

    for (case_id, panel_enabled, authenticated_tier) in cases {
        let vector = corpus
            .cases
            .iter()
            .find(|vector| vector.id == case_id)
            .unwrap_or_else(|| panic!("missing source-shaped corpus vector: {case_id}"));
        let ExpectedOutcome::Accepted {
            fusion: expected_fusion,
        } = &vector.expected.cli
        else {
            panic!("source-shaped corpus vector {case_id} must be accepted by the CLI");
        };
        let state = AppState::new(ProviderRegistry::new())
            .with_panel_enabled(panel_enabled)
            .with_panel_min_tier(CallerTier::Pro);
        let document = build_document(&state, authenticated_tier, generated_at);
        let body = serde_json::to_vec(&document)
            .unwrap_or_else(|error| panic!("{case_id} document must serialize: {error}"));
        let snapshot = parse_snapshot(&body)
            .unwrap_or_else(|error| panic!("{case_id} gateway document must parse: {error}"));
        let actual_fusion = serde_json::to_value(&snapshot.fusion)
            .unwrap_or_else(|error| panic!("{case_id} fusion must serialize: {error}"));

        // The production cap is intentionally process-configurable. Every
        // other normalized Fusion field, including every emitted reason code,
        // must remain compatible with this source-shaped vector.
        let mut expected = expected_fusion.clone();
        expected["member_models_max"] = Value::from(snapshot.fusion.member_models_max);

        assert_eq!(
            actual_fusion, expected,
            "{case_id} source Fusion projection"
        );
        assert!(
            snapshot.fusion.member_models_max > 0,
            "{case_id} must preserve a positive gateway member cap"
        );
    }
}
