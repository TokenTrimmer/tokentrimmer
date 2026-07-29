//! Generated public contract artifacts for TokenTrimmer wire and proof surfaces.
//!
//! The actual Rust wire types and canonicalizers remain authoritative. This
//! crate derives JSON Schema from those types, derives TypeScript from those
//! schemas, and mints deterministic fixtures with the production signing and
//! replay functions. `check` compares every byte with the checked-in output.

mod schema;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tt_plan_core::{
    ModelPricing, PlanInput, ProposedRoute, RequestLog, RouteAction, RouteConditions, SavingsBundle,
};
use tt_telemetry::arr_receipt::AgentRunReceipt;
use tt_telemetry::wfr_receipt::WfrReceipt;
use uuid::Uuid;

const GENERATED_TS_PATH: &str = "bindings/receipt-contracts.generated.ts";
const MANIFEST_PATH: &str = "docs/receipt-spec/receipt-contracts.manifest.json";
const PRODUCT_TS_PATH: &str = "bindings/product-contracts.generated.ts";
const PRODUCT_MANIFEST_PATH: &str = "docs/contracts/product-contracts.manifest.json";
const ROUTE_PREVIEW_V2_PATH: &str =
    "docs/route-preview-contract/tokentrimmer.route-preview-coverage.v2.corpus.json";
const FIXED_KEY_BYTES: [u8; 32] = [7; 32];
const FIXED_KEY_HEX: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";

#[derive(Debug)]
pub struct GeneratedArtifact {
    pub relative_path: String,
    pub bytes: Vec<u8>,
}

/// Repository root inferred from this crate's stable workspace location.
#[must_use]
pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Regenerate every published proof contract in memory.
pub fn generate_artifacts() -> Result<Vec<GeneratedArtifact>> {
    let receipt_schemas = schema::generate_schemas()?;
    let mut receipt_artifacts = Vec::new();
    for contract in &receipt_schemas {
        receipt_artifacts.push(json_artifact(contract.relative_path, &contract.value)?);
    }
    receipt_artifacts.push(GeneratedArtifact {
        relative_path: GENERATED_TS_PATH.into(),
        bytes: schema::render_typescript(&receipt_schemas)?.into_bytes(),
    });
    receipt_artifacts.extend(generate_vectors()?);
    receipt_artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let receipt_manifest = manifest_artifact(&receipt_artifacts)?;

    let product_schemas = schema::generate_product_schemas()?;
    let mut product_artifacts = Vec::new();
    for contract in &product_schemas {
        product_artifacts.push(json_artifact(contract.relative_path, &contract.value)?);
    }
    product_artifacts.push(GeneratedArtifact {
        relative_path: PRODUCT_TS_PATH.into(),
        bytes: schema::render_product_typescript(&product_schemas)?.into_bytes(),
    });
    product_artifacts.push(typed_json_artifact(
        "docs/workflow-contract/workflow-definition-v1.golden.json",
        &workflow_definition_vector(),
    )?);
    // Preview coverage is a public-owned compatibility decision paired with
    // the canonical route field inventory. Its Rust corpus test enforces the
    // semantics; including the exact bytes here makes pin consumers verify the
    // same artifact through the product manifest instead of an ad-hoc copy.
    product_artifacts.push(GeneratedArtifact {
        relative_path: ROUTE_PREVIEW_V2_PATH.into(),
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/route-preview-contract/tokentrimmer.route-preview-coverage.v2.corpus.json"
        ))
        .to_vec(),
    });
    product_artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let product_manifest = product_manifest_artifact(&product_artifacts)?;

    let mut artifacts = receipt_artifacts;
    artifacts.push(receipt_manifest);
    artifacts.extend(product_artifacts);
    artifacts.push(product_manifest);
    artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(artifacts)
}

/// Write generated artifacts below `root`, creating only their narrow parent
/// directories. Existing files are replaced only with generator output.
pub fn write_artifacts(root: &Path) -> Result<()> {
    for artifact in generate_artifacts()? {
        let path = root.join(&artifact.relative_path);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("artifact path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create artifact directory {}", parent.display()))?;
        fs::write(&path, artifact.bytes)
            .with_context(|| format!("write generated artifact {}", path.display()))?;
    }
    Ok(())
}

/// Fail with every missing or stale artifact below `root`.
pub fn check_artifacts(root: &Path) -> Result<()> {
    let mut drift = Vec::new();
    for artifact in generate_artifacts()? {
        let path = root.join(&artifact.relative_path);
        match fs::read(&path) {
            Ok(actual) if actual == artifact.bytes => {}
            Ok(_) => drift.push(format!("stale: {}", artifact.relative_path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                drift.push(format!("missing: {}", artifact.relative_path));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read generated artifact {}", path.display()));
            }
        }
    }
    if !drift.is_empty() {
        bail!(
            "generated contract drift:\n{}\nrun `cargo run -p tt-ts-types -- write`",
            drift.join("\n")
        );
    }
    Ok(())
}

fn json_artifact(path: &str, value: &Value) -> Result<GeneratedArtifact> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serialize generated JSON")?;
    bytes.push(b'\n');
    Ok(GeneratedArtifact {
        relative_path: path.into(),
        bytes,
    })
}

fn typed_json_artifact<T: Serialize>(path: &str, value: &T) -> Result<GeneratedArtifact> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serialize typed fixture")?;
    bytes.push(b'\n');
    Ok(GeneratedArtifact {
        relative_path: path.into(),
        bytes,
    })
}

fn generate_vectors() -> Result<Vec<GeneratedArtifact>> {
    let key = SigningKey::from_bytes(&FIXED_KEY_BYTES);
    let vcr = tt_telemetry::vcr::sign(
        &key,
        Uuid::from_u128(42),
        Uuid::from_u128(99),
        "catalog:openai->gpt-4o-mini",
        "gpt-4o-mini",
        -1_200,
        0.0034,
        "2026-07-06T20:00:00Z",
    );
    let l2 = tt_telemetry::l2_receipt::sign(
        &key,
        Uuid::from_u128(42),
        Uuid::from_u128(99),
        Uuid::from_u128(7),
        0.931_2,
        tt_telemetry::l2_receipt::VERDICT_VERIFIED,
        0.0,
        0.011_7,
        "2026-07-08T12:00:00Z",
    );
    let bundle = deterministic_bundle()?;

    Ok(vec![
        typed_json_artifact("docs/receipt-spec/vcr-v1.golden.json", &vcr)?,
        typed_json_artifact("docs/receipt-spec/l2-v1.golden.json", &l2)?,
        typed_json_artifact(
            "docs/receipt-spec/wfr-v1.golden.json",
            &workflow_receipt("v1")?,
        )?,
        typed_json_artifact(
            "docs/receipt-spec/wfr-v2.golden.json",
            &workflow_receipt("v2")?,
        )?,
        typed_json_artifact(
            "docs/receipt-spec/wfr-v3.golden.json",
            &workflow_receipt("v3")?,
        )?,
        typed_json_artifact(
            "docs/receipt-spec/wfr-v4.golden.json",
            &workflow_receipt("v4")?,
        )?,
        typed_json_artifact(
            "docs/receipt-spec/arr-v1.golden.json",
            &agent_receipt("v1")?,
        )?,
        typed_json_artifact(
            "docs/receipt-spec/arr-v2.golden.json",
            &agent_receipt("v2")?,
        )?,
        typed_json_artifact("docs/receipt-spec/savings-bundle-v1.golden.json", &bundle)?,
    ])
}

fn workflow_receipt(version: &str) -> Result<WfrReceipt> {
    let (cost, baseline, saved, signed, eligible, quality, signed_at) = match version {
        "v1" => (
            70_000,
            180_000,
            110_000,
            None,
            None,
            None,
            "2026-07-15T00:00:00Z",
        ),
        "v2" => (
            70_000,
            180_000,
            110_000,
            None,
            None,
            Some("equivalent"),
            "2026-07-15T00:00:00Z",
        ),
        "v3" => (
            200_000,
            180_000,
            0,
            Some(-50_000),
            Some(2),
            None,
            "2026-07-19T00:00:00Z",
        ),
        "v4" => (
            70_000,
            180_000,
            100_000,
            Some(100_000),
            Some(3),
            Some("equivalent"),
            "2026-07-19T00:00:00Z",
        ),
        _ => bail!("unsupported WFR fixture version {version}"),
    };
    let mut receipt = WfrReceipt {
        run_id: Uuid::from_u128(0xa1),
        org_id: Uuid::from_u128(0x2a),
        workflow_id: Uuid::from_u128(0xb2),
        status: "completed".into(),
        cost_micros: cost,
        baseline_micros: baseline,
        saved_micros: saved,
        signed_request_delta_micros: signed,
        request_delta_formula_version: signed.map(|_| tt_shared::REQUEST_DELTA_ESTIMATE_V1.into()),
        request_delta_eligible_requests: eligible,
        request_delta_measured_requests: eligible,
        cost_usd: Some(cost as f64 / 1_000_000.0),
        baseline_usd: Some(baseline as f64 / 1_000_000.0),
        saved_usd: Some(saved as f64 / 1_000_000.0),
        signed_request_delta_usd: signed.map(|value| value as f64 / 1_000_000.0),
        signature_hex: String::new(),
        verifying_key_hex: FIXED_KEY_HEX.into(),
        canonical_version: version.into(),
        quality_verdict: quality.map(str::to_owned),
        signed_at: Some(signed_at.into()),
    };
    let payload = tt_telemetry::wfr_receipt::canonical_payload(&receipt)
        .context("canonicalize generated WFR fixture")?;
    receipt.signature_hex = sign_hex(payload.as_bytes());
    Ok(receipt)
}

fn agent_receipt(version: &str) -> Result<AgentRunReceipt> {
    let (cost, baseline, saved, signed, eligible, signed_at) = match version {
        "v1" => (70_000, 180_000, 110_000, None, None, "2026-07-15T00:00:00Z"),
        "v2" => (
            200_000,
            180_000,
            0,
            Some(-50_000),
            Some(2),
            "2026-07-19T00:00:00Z",
        ),
        _ => bail!("unsupported ARR fixture version {version}"),
    };
    let mut receipt = AgentRunReceipt {
        run_id: Uuid::from_u128(0xa1),
        org_id: Uuid::from_u128(0x2a),
        status: "completed".into(),
        cost_micros: cost,
        baseline_micros: baseline,
        saved_micros: saved,
        signed_request_delta_micros: signed,
        request_delta_formula_version: signed.map(|_| tt_shared::REQUEST_DELTA_ESTIMATE_V1.into()),
        request_delta_eligible_requests: eligible,
        request_delta_measured_requests: eligible,
        cost_usd: Some(cost as f64 / 1_000_000.0),
        baseline_usd: Some(baseline as f64 / 1_000_000.0),
        saved_usd: Some(saved as f64 / 1_000_000.0),
        signed_request_delta_usd: signed.map(|value| value as f64 / 1_000_000.0),
        signature_hex: String::new(),
        verifying_key_hex: FIXED_KEY_HEX.into(),
        canonical_version: version.into(),
        signed_at: Some(signed_at.into()),
    };
    let payload = tt_telemetry::arr_receipt::canonical_payload(&receipt)
        .context("canonicalize generated ARR fixture")?;
    receipt.signature_hex = sign_hex(payload.as_bytes());
    Ok(receipt)
}

fn sign_hex(payload: &[u8]) -> String {
    let key = SigningKey::from_bytes(&FIXED_KEY_BYTES);
    hex::encode(key.sign(payload).to_bytes())
}

fn deterministic_bundle() -> Result<SavingsBundle> {
    let request = RequestLog {
        id: Uuid::from_u128(0x10),
        org_id: Uuid::from_u128(0x02),
        ts: timestamp("2026-05-01T12:00:00Z")?,
        provider: "anthropic".into(),
        model: "claude-3-5-sonnet".into(),
        requested_model: Some("claude-3-5-sonnet".into()),
        input_tokens: 1_000,
        output_tokens: 100,
        cached_tokens: 0,
        cost_usd: 0.0045,
        baseline_cost_usd: 0.0045,
        cached: false,
        cache_layer: None,
        matched_route_id: None,
        latency_ms: 100,
        upstream_latency_ms: Some(80),
        status: 200,
        tag: None,
        embedding: None,
        finish_reason: None,
        body: None,
        response_body: None,
        task_class: Default::default(),
        diff_saved_usd: None,
        minify_saved_est_usd: None,
    };
    let route = ProposedRoute {
        id: Uuid::from_u128(0x99),
        name: "cheap-for-short".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["claude-3-5-sonnet".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: Some("claude-3-5-haiku".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            format_switch: None,
            diff: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            ..Default::default()
        },
    };
    let pricing = HashMap::from([(
        "anthropic:claude-3-5-haiku".into(),
        ModelPricing {
            input_per_million: 0.25,
            output_per_million: 1.25,
            cached_input_per_million: Some(0.025),
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        },
    )]);
    let input = PlanInput {
        plan_id: Uuid::from_u128(0x01),
        org_id: Uuid::from_u128(0x02),
        window_start: timestamp("2026-05-01T00:00:00Z")?,
        window_end: timestamp("2026-05-08T00:00:00Z")?,
        requests: vec![request],
        proposed_routes: vec![route],
        pricing,
        config: Default::default(),
        seed: 42,
        bootstrap_iterations: 200,
    };
    let result = tt_plan_core::replay(input.clone()).context("replay contract bundle fixture")?;
    Ok(SavingsBundle {
        schema_version: tt_plan_core::BUNDLE_SCHEMA_VERSION,
        tool_version: "0.2.0".into(),
        created_at: "2026-07-19T00:00:00Z".into(),
        plan_input: input,
        expected_result: result,
        attestation: None,
    })
}

fn timestamp(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("parse fixed timestamp {value}"))?
        .with_timezone(&Utc))
}

fn workflow_definition_vector() -> tt_core::workflow::types::WorkflowDefinition {
    use tt_core::workflow::types::{
        BudgetPolicy, Edge, ModelSelection, Node, NodeKind, OnExceed, WorkflowDefinition,
        WorkflowTrigger,
    };

    WorkflowDefinition {
        id: Uuid::from_u128(0xc1),
        version: 1,
        name: "generated-contract-smoke".into(),
        nodes: vec![
            Node {
                id: "input".into(),
                kind: NodeKind::Trigger,
            },
            Node {
                id: "answer".into(),
                kind: NodeKind::Model {
                    selection: ModelSelection::Model {
                        model: "gpt-4o-mini".into(),
                    },
                    prompt: "Answer {{input}}".into(),
                    max_output_tokens: Some(256),
                    max_cost_usd: Some(0.02),
                },
            },
            Node {
                id: "output".into(),
                kind: NodeKind::Output,
            },
        ],
        edges: vec![
            Edge {
                from: "input".into(),
                to: "answer".into(),
                map: None,
            },
            Edge {
                from: "answer".into(),
                to: "output".into(),
                map: None,
            },
        ],
        inputs: json!({"question": "Why is exact wire generation useful?"}),
        budget: BudgetPolicy {
            max_cost_usd: Some(0.05),
            on_exceed: OnExceed::Stop,
        },
        allowed_hosts: Vec::new(),
        metadata: json!({"canvas_positions": {
            "input": {"x": 0, "y": 0},
            "answer": {"x": 240, "y": 0},
            "output": {"x": 480, "y": 0}
        }}),
        triggers: vec![WorkflowTrigger::Schedule {
            interval: "1d".into(),
            environment: Some(tt_core::workflow::types::WorkflowTriggerEnvironment::Production),
        }],
    }
}

fn manifest_artifact(artifacts: &[GeneratedArtifact]) -> Result<GeneratedArtifact> {
    let files = artifacts
        .iter()
        .map(|artifact| {
            json!({
                "path": artifact.relative_path,
                "sha256": hex::encode(Sha256::digest(&artifact.bytes)),
            })
        })
        .collect::<Vec<_>>();
    let manifest = json!({
        "contract": "tokentrimmer.proof-contracts.v1",
        "generated_from": "Rust wire types, canonicalizers, signers, and deterministic replay",
        "typescript": GENERATED_TS_PATH,
        "families": [
            {
                "family": "vcr",
                "kind": "ed25519_receipt",
                "versions": ["v1"],
                "canonical_prefixes": [tt_telemetry::vcr::VCR_PREFIX],
                "schema": "docs/receipt-spec/vcr-receipt.schema.json",
                "vectors": ["docs/receipt-spec/vcr-v1.golden.json"],
                "mint": "POST /v1/admin/requests/{trace_id}/compression-receipt/sign",
                "verify": "tt verify-receipt"
            },
            {
                "family": "l2",
                "kind": "ed25519_receipt",
                "versions": ["v1"],
                "canonical_prefixes": [tt_telemetry::l2_receipt::L2_PREFIX],
                "schema": "docs/receipt-spec/l2-receipt.schema.json",
                "vectors": ["docs/receipt-spec/l2-v1.golden.json"],
                "mint": "POST /v1/admin/requests/{trace_id}/l2-receipt/sign",
                "verify": "tt verify-receipt"
            },
            {
                "family": "wfr",
                "kind": "ed25519_receipt",
                "versions": ["v1", "v2", "v3", "v4"],
                "canonical_prefixes": ["wfr:v1", "wfr:v2", "wfr:v3", "wfr:v4"],
                "schema": "docs/receipt-spec/wfr-receipt.schema.json",
                "vectors": [
                    "docs/receipt-spec/wfr-v1.golden.json",
                    "docs/receipt-spec/wfr-v2.golden.json",
                    "docs/receipt-spec/wfr-v3.golden.json",
                    "docs/receipt-spec/wfr-v4.golden.json"
                ],
                "mint": "POST /v1/admin/workflow-runs/{run_id}/receipt/sign",
                "share": "GET /v1/workflow-runs/{run_id}/receipt?expires=&sig=",
                "verify": "tt verify-receipt"
            },
            {
                "family": "arr",
                "kind": "ed25519_receipt",
                "versions": ["v1", "v2"],
                "canonical_prefixes": ["arr:v1", "arr:v2"],
                "schema": "docs/receipt-spec/arr-receipt.schema.json",
                "vectors": [
                    "docs/receipt-spec/arr-v1.golden.json",
                    "docs/receipt-spec/arr-v2.golden.json"
                ],
                "mint": "POST /v1/admin/agent-runs/{run_id}/receipt/sign",
                "share": "GET /v1/agent-runs/{run_id}/receipt?expires=&sig=",
                "verify": "tt verify-receipt"
            },
            {
                "family": "savings_bundle",
                "kind": "deterministic_replay_bundle",
                "versions": ["v1"],
                "schema": "docs/receipt-spec/savings-bundle.schema.json",
                "vectors": ["docs/receipt-spec/savings-bundle-v1.golden.json"],
                "mint": "tt plan --emit-bundle",
                "verify": "tt verify-bundle"
            }
        ],
        "files": files
    });
    json_artifact(MANIFEST_PATH, &manifest)
}

fn product_manifest_artifact(artifacts: &[GeneratedArtifact]) -> Result<GeneratedArtifact> {
    let files = artifacts
        .iter()
        .map(|artifact| {
            json!({
                "path": artifact.relative_path,
                "sha256": hex::encode(Sha256::digest(&artifact.bytes)),
            })
        })
        .collect::<Vec<_>>();
    let manifest = json!({
        "contract": "tokentrimmer.product-contracts.v1",
        "generated_from": "Authoritative Rust route parser, route-preview coverage decision, workflow definition/write types, model-catalog response type, and gateway capability response type",
        "typescript": PRODUCT_TS_PATH,
        "contracts": [
            {
                "family": "route",
                "id": tt_routing::ROUTE_SCHEMA_ID,
                "versions": [tt_routing::ROUTE_SCHEMA_VERSION],
                "schema": "docs/route-contract/route-write.schema.json",
                "compatibility_corpus": "docs/route-contract/tokentrimmer.route.v1.corpus.json",
                "write": "POST /v1/routes"
            },
            {
                "family": "route_preview_coverage",
                "id": "tokentrimmer.route-preview-coverage-corpus",
                "versions": [2],
                "route_contract_id": tt_routing::ROUTE_SCHEMA_ID,
                "route_contract_versions": [tt_routing::ROUTE_SCHEMA_VERSION],
                "compatibility_corpus": ROUTE_PREVIEW_V2_PATH
            },
            {
                "family": "workflow_definition",
                "id": "tokentrimmer.workflow-definition.v1",
                "versions": [1],
                "schema": "docs/workflow-contract/workflow-definition.schema.json",
                "vectors": ["docs/workflow-contract/workflow-definition-v1.golden.json"],
                "read": "GET /v1/workflows/{id}"
            },
            {
                "family": "workflow_write",
                "id": "tokentrimmer.workflow-write.v1",
                "versions": [1],
                "schema": "docs/workflow-contract/workflow-write.schema.json",
                "write": "POST /v1/workflows"
            },
            {
                "family": "models",
                "id": "tokentrimmer.models.v1",
                "versions": [tt_shared::MODELS_SCHEMA_VERSION],
                "schema": "docs/model-contract/models-response.schema.json",
                "read": "GET /v1/models"
            },
            {
                "family": "gateway_capabilities",
                "id": "tokentrimmer.gateway-capabilities.v1",
                "versions": [tt_core::routes::capabilities::CAPABILITIES_SCHEMA_VERSION],
                "schema": "docs/capability-contract/gateway-capabilities.schema.json",
                "compatibility_corpus": "docs/capability-contract/tokentrimmer.gateway-capabilities.v1.corpus.json",
                "read": "GET /v1/capabilities"
            }
        ],
        "files": files
    });
    json_artifact(PRODUCT_MANIFEST_PATH, &manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_contracts_match_generator() {
        check_artifacts(&repository_root()).expect("generated proof contracts must not drift");
    }

    #[test]
    fn generated_vectors_verify_with_real_implementations() {
        let artifacts = generate_artifacts().expect("generate artifacts");
        let get = |path: &str| {
            artifacts
                .iter()
                .find(|artifact| artifact.relative_path == path)
                .expect("generated vector")
                .bytes
                .as_slice()
        };
        let vcr: tt_telemetry::vcr::VcrReceipt =
            serde_json::from_slice(get("docs/receipt-spec/vcr-v1.golden.json"))
                .expect("parse VCR vector");
        assert!(tt_telemetry::vcr::verify(&vcr));
        let l2: tt_telemetry::l2_receipt::L2Receipt =
            serde_json::from_slice(get("docs/receipt-spec/l2-v1.golden.json"))
                .expect("parse L2 vector");
        assert!(tt_telemetry::l2_receipt::verify(&l2));
        for version in ["v1", "v2", "v3", "v4"] {
            let receipt: WfrReceipt = serde_json::from_slice(get(&format!(
                "docs/receipt-spec/wfr-{version}.golden.json"
            )))
            .expect("parse WFR vector");
            assert!(tt_telemetry::wfr_receipt::verify_with_key(
                FIXED_KEY_HEX,
                &receipt
            ));
        }
        for version in ["v1", "v2"] {
            let receipt: AgentRunReceipt = serde_json::from_slice(get(&format!(
                "docs/receipt-spec/arr-{version}.golden.json"
            )))
            .expect("parse ARR vector");
            assert!(tt_telemetry::arr_receipt::verify_with_key(
                FIXED_KEY_HEX,
                &receipt
            ));
        }
        let bundle: SavingsBundle =
            serde_json::from_slice(get("docs/receipt-spec/savings-bundle-v1.golden.json"))
                .expect("parse savings bundle vector");
        let recomputed = tt_plan_core::replay(bundle.plan_input.clone()).expect("replay bundle");
        assert_eq!(
            serde_json::to_value(recomputed).expect("serialize replay"),
            serde_json::to_value(bundle.expected_result).expect("serialize expected result")
        );

        let workflow: tt_core::workflow::types::WorkflowDefinition = serde_json::from_slice(get(
            "docs/workflow-contract/workflow-definition-v1.golden.json",
        ))
        .expect("parse workflow definition vector");
        assert_eq!(workflow.version, 1);
        assert_eq!(workflow.nodes.len(), 3);
        assert_eq!(workflow.triggers.len(), 1);

        let product_types = std::str::from_utf8(get(PRODUCT_TS_PATH)).expect("product TypeScript");
        assert!(product_types.contains("export type Node = {\n  id: string;\n} & ("));
        assert!(product_types.contains("max_output_tokens?: number | null;"));
        assert!(product_types.contains("export type RouteWriteRequest ="));
        assert!(product_types.contains("export type ModelsResponse ="));
        assert!(product_types.contains("export type GatewayCapabilitiesDocument ="));
    }
}
