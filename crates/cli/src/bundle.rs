//! Portable "reproduce-this-savings" bundle — a self-contained, offline
//! artifact that lets anyone re-derive a Plan's savings figure bit-for-bit
//! without a network, a database, or trust in TokenTrimmer.
//!
//! The whole design leans on one property already proven by the CI check
//! "plan replay determinism": [`tt_plan_core::replay`] is a pure function of
//! its [`PlanInput`] — the same `(requests, proposed_routes, pricing, config,
//! seed, bootstrap_iterations)` always yields a bit-identical [`PlanResult`].
//! A bundle therefore just needs to capture that exact `PlanInput` (the
//! inputs + the RNG seed + the catalog/pricing snapshot, all of which live
//! inside it) alongside the `PlanResult` it produced (the expected outputs),
//! plus an optional signed attestation reference.
//!
//! - **Produce** a bundle with `tt plan --input <plan.json> --emit-bundle
//!   <bundle.json>` (optionally `--attestation <AUDIT-CHAIN.jsonl>`).
//! - **Verify** it offline with `tt verify-bundle <bundle.json>`: re-run the
//!   replay from the captured input+seed+pricing, assert the recomputed
//!   `PlanResult` matches the recorded one byte-for-byte, verify any embedded
//!   attestation signature, and print PASS/FAIL with the reproduced savings.
//!
//! `verify-bundle` exits non-zero on any mismatch, so it drops straight into a
//! CI gate or a `Download → verify the math` flow.

use std::path::Path;

use serde::{Deserialize, Serialize};

use tt_plan_core::{PlanInput, PlanResult};
use tt_telemetry::audit::AuditEntry;

/// Current bundle schema version. Bumped only on a breaking shape change; a
/// verifier refuses a bundle whose `schema_version` it does not understand
/// rather than silently mis-reading it.
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// A self-contained, offline-reproducible savings bundle.
///
/// Everything `verify-bundle` needs is here: it never touches the network or a
/// database. Serialized as versioned JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavingsBundle {
    /// Schema version — see [`BUNDLE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The `tt` version that produced the bundle (informational; not part of
    /// the reproduction check).
    pub tool_version: String,
    /// RFC 3339 timestamp the bundle was produced (informational; explicitly
    /// NOT part of the reproduction check — replay reads none of it).
    pub created_at: String,
    /// The complete replay input: historical rows, the proposed routes, the
    /// **catalog/pricing snapshot** (`plan_input.pricing`), the non-route
    /// config, the **RNG seed** (`plan_input.seed`), and the bootstrap
    /// iteration count. This is EXACTLY what [`tt_plan_core::replay`] consumes,
    /// so the reproduction is bit-for-bit.
    pub plan_input: PlanInput,
    /// The expected replay output. `verify-bundle` recomputes the result from
    /// `plan_input` and asserts it matches this, field-for-field.
    pub expected_result: PlanResult,
    /// Optional signed attestation reference: an Ed25519 hash-chained audit
    /// export (the same shape `tt audit verify` consumes). When present,
    /// `verify-bundle` checks its signatures + hash chain OFFLINE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<BundleAttestation>,
}

/// A bundled, self-verifying attestation reference — the signed hash-chained
/// audit entries plus the public key they were signed with, so the signature
/// can be checked with no external key lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleAttestation {
    /// Hex-encoded Ed25519 verifying (public) key the entries were signed with.
    pub verifying_key: String,
    /// The signed, hash-chained audit entries (`plan.applied`, etc.).
    pub entries: Vec<AuditEntry>,
}

impl SavingsBundle {
    /// Assemble a bundle from a replay `plan_input`, the `result` it produced,
    /// and an optional attestation reference.
    ///
    /// The caller is responsible for having run `replay(plan_input.clone())`
    /// to obtain `result` — this constructor does not re-run the replay, it
    /// just records both sides so `verify-bundle` can cross-check them later.
    #[must_use]
    pub fn new(
        plan_input: PlanInput,
        result: PlanResult,
        attestation: Option<BundleAttestation>,
    ) -> Self {
        Self {
            schema_version: BUNDLE_SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            plan_input,
            expected_result: result,
            attestation,
        }
    }
}

/// Whether a bundle's embedded attestation verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationOutcome {
    /// No attestation was bundled.
    Absent,
    /// The attestation's signatures + hash chain verified offline.
    Verified {
        /// How many signed entries were checked.
        entries: usize,
    },
    /// The attestation was present but failed verification.
    Failed(String),
}

/// The outcome of verifying a [`SavingsBundle`].
#[derive(Debug, Clone)]
pub struct BundleReport {
    /// True iff the recomputed result matched the recorded one AND any
    /// embedded attestation verified — i.e. an overall PASS.
    pub passed: bool,
    /// Whether the recomputed [`PlanResult`] matched the bundle's
    /// `expected_result` byte-for-byte.
    pub result_matches: bool,
    /// The reproduced projected savings, USD (from the freshly recomputed
    /// result — the number the bundle lets you independently re-derive).
    pub reproduced_savings_usd: f64,
    /// The reproduced projected savings percentage.
    pub reproduced_savings_pct: f64,
    /// SHA-256 fingerprint of the recomputed result's canonical JSON — a short
    /// stable anchor two parties can compare out-of-band.
    pub result_digest: String,
    /// The attestation verification outcome.
    pub attestation: AttestationOutcome,
    /// A human-readable explanation when `result_matches` is false.
    pub mismatch_detail: Option<String>,
}

/// Errors that make a bundle unverifiable before the replay even runs (as
/// opposed to a verification FAIL, which is a well-formed bundle whose numbers
/// don't reproduce — that is reported via [`BundleReport::passed`]).
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// The bundle's `schema_version` is newer/unknown to this `tt`.
    #[error(
        "unsupported bundle schema_version {found} (this tt understands v{supported}); \
         upgrade tt to verify this bundle"
    )]
    UnsupportedSchema {
        /// The version found in the bundle.
        found: u32,
        /// The version this binary supports.
        supported: u32,
    },
    /// The replay itself could not run (invalid window, zero iterations, …) —
    /// the captured input is not a valid `PlanInput`.
    #[error("replay of the bundled input failed: {0}")]
    Replay(#[from] tt_plan_core::PlanError),
    /// JSON (de)serialization failed while comparing results.
    #[error("serialize result for comparison: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Re-run the replay captured in `bundle` and cross-check it against the
/// recorded outputs + attestation. Pure (no I/O): all inputs are in `bundle`.
///
/// # Errors
///
/// Returns [`BundleError`] only when the bundle cannot be evaluated at all
/// (unknown schema, or the captured input is not a runnable `PlanInput`). A
/// well-formed bundle whose numbers simply don't reproduce is NOT an error —
/// it comes back as a [`BundleReport`] with `passed == false`.
pub fn verify_bundle(bundle: &SavingsBundle) -> Result<BundleReport, BundleError> {
    if bundle.schema_version != BUNDLE_SCHEMA_VERSION {
        return Err(BundleError::UnsupportedSchema {
            found: bundle.schema_version,
            supported: BUNDLE_SCHEMA_VERSION,
        });
    }

    // Deterministically RE-RUN the plan from the captured input+seed+pricing.
    // `replay` consumes the input, so hand it a clone.
    let recomputed = tt_plan_core::replay(bundle.plan_input.clone())?;

    // Byte/decimal-stable comparison: serialize BOTH results through the same
    // `Serialize` impl and compare the strings. Because `replay` is
    // deterministic and f64 JSON round-trips losslessly, two logically-equal
    // `PlanResult`s serialize to identical bytes — so any tampered
    // expected-output value flips this to a mismatch.
    let recomputed_json = serde_json::to_string(&recomputed)?;
    let expected_json = serde_json::to_string(&bundle.expected_result)?;
    let result_matches = recomputed_json == expected_json;

    let mismatch_detail = if result_matches {
        None
    } else {
        Some(describe_mismatch(&bundle.expected_result, &recomputed))
    };

    let result_digest = sha256_hex(recomputed_json.as_bytes());

    let attestation = match &bundle.attestation {
        None => AttestationOutcome::Absent,
        Some(att) => verify_attestation(att),
    };

    let passed = result_matches && !matches!(attestation, AttestationOutcome::Failed(_));

    Ok(BundleReport {
        passed,
        result_matches,
        reproduced_savings_usd: recomputed.aggregates.projected_savings_usd,
        reproduced_savings_pct: recomputed.aggregates.projected_savings_pct,
        result_digest,
        attestation,
        mismatch_detail,
    })
}

/// Verify a bundled attestation's Ed25519 signatures + hash chain OFFLINE,
/// reusing the exact `tt audit verify` path ([`tt_telemetry::audit::verify_chain`]).
fn verify_attestation(att: &BundleAttestation) -> AttestationOutcome {
    let key_bytes = match hex::decode(att.verifying_key.trim()) {
        Ok(b) => b,
        Err(e) => return AttestationOutcome::Failed(format!("verifying key hex decode: {e}")),
    };
    let key_array: [u8; 32] = match key_bytes.try_into() {
        Ok(a) => a,
        Err(_) => {
            return AttestationOutcome::Failed(
                "verifying key must be exactly 32 bytes (64 hex chars)".to_string(),
            )
        }
    };
    let verifying_key = match ed25519_dalek::VerifyingKey::from_bytes(&key_array) {
        Ok(k) => k,
        Err(e) => return AttestationOutcome::Failed(format!("invalid Ed25519 verifying key: {e}")),
    };
    match tt_telemetry::audit::verify_chain(&att.entries, &verifying_key) {
        Ok(()) => AttestationOutcome::Verified {
            entries: att.entries.len(),
        },
        Err(e) => AttestationOutcome::Failed(e.to_string()),
    }
}

/// Build a concise, human-readable description of the first headline figure
/// that differs between the recorded `expected` and freshly `recomputed`
/// results. Falls back to a generic message when the divergence is in a deeper
/// field the summary doesn't enumerate.
fn describe_mismatch(expected: &PlanResult, recomputed: &PlanResult) -> String {
    let e = &expected.aggregates;
    let g = &recomputed.aggregates;
    let checks: [(&str, f64, f64); 5] = [
        (
            "sample_size",
            f64::from(expected.sample_size),
            f64::from(recomputed.sample_size),
        ),
        (
            "aggregates.total_baseline_cost_usd",
            e.total_baseline_cost_usd,
            g.total_baseline_cost_usd,
        ),
        (
            "aggregates.total_projected_cost_usd",
            e.total_projected_cost_usd,
            g.total_projected_cost_usd,
        ),
        (
            "aggregates.projected_savings_usd",
            e.projected_savings_usd,
            g.projected_savings_usd,
        ),
        (
            "aggregates.projected_savings_pct",
            e.projected_savings_pct,
            g.projected_savings_pct,
        ),
    ];
    for (field, exp, got) in checks {
        if exp.to_bits() != got.to_bits() {
            return format!(
                "bundle records {field} = {exp}, but replay reproduces {got} \
                 (the recorded output does not match a deterministic replay of the input)"
            );
        }
    }
    "recomputed result differs from the expected result recorded in the bundle \
     (a non-headline field diverged)"
        .to_string()
}

/// Lowercase-hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Load an attestation reference from an `AUDIT-CHAIN.jsonl` export (the file
/// `tt plan --apply` writes and `tt audit verify` reads). The verifying key is
/// taken from the export's `{"meta":true,"verifying_key":"<hex>"}` preamble.
///
/// # Errors
///
/// Fails when the file is unreadable, a line is not valid JSON / not an
/// `AuditEntry`, or no preamble verifying key is present (there is no key to
/// bundle, so the attestation could never be checked offline).
pub fn load_attestation_from_chain(path: &Path) -> anyhow::Result<BundleAttestation> {
    use anyhow::Context;
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read attestation chain {}", path.display()))?;
    let mut verifying_key: Option<String> = None;
    let mut entries: Vec<AuditEntry> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("parse attestation line {} as JSON", i + 1))?;
        if value.get("meta").and_then(serde_json::Value::as_bool) == Some(true) {
            verifying_key = value
                .get("verifying_key")
                .and_then(|k| k.as_str())
                .map(String::from);
            continue;
        }
        let entry: AuditEntry = serde_json::from_value(value)
            .with_context(|| format!("parse attestation line {} as audit entry", i + 1))?;
        entries.push(entry);
    }
    let verifying_key = verifying_key.context(
        "attestation chain has no `{\"meta\":true,\"verifying_key\":...}` preamble — \
         without the verifying key the signature can't be checked offline",
    )?;
    Ok(BundleAttestation {
        verifying_key,
        entries,
    })
}

/// Write `bundle` to `path` as pretty JSON.
///
/// # Errors
///
/// Fails on serialization or filesystem write errors.
pub fn write_bundle(path: &Path, bundle: &SavingsBundle) -> anyhow::Result<()> {
    use anyhow::Context;
    let json = serde_json::to_string_pretty(bundle).context("serialize bundle")?;
    std::fs::write(path, json).with_context(|| format!("write bundle {}", path.display()))?;
    Ok(())
}

/// Read + verify a bundle at `path`, print a PASS/FAIL report, and return an
/// error (→ non-zero exit) on any mismatch.
///
/// This is the `tt verify-bundle <path>` entry point.
///
/// # Errors
///
/// Returns an error — so the process exits non-zero — when the file cannot be
/// read/parsed, the bundle can't be evaluated ([`BundleError`]), the recomputed
/// result does not match, or an embedded attestation fails to verify.
pub fn run_verify_bundle(path: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let raw = std::fs::read_to_string(path).with_context(|| format!("read bundle {path}"))?;
    let bundle: SavingsBundle =
        serde_json::from_str(&raw).with_context(|| format!("parse bundle {path}"))?;

    crate::ui::note(&format!(
        "bundle v{} produced by tt {} at {}",
        bundle.schema_version, bundle.tool_version, bundle.created_at
    ));

    let report = verify_bundle(&bundle).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Report the reproduction, then the attestation, then the verdict.
    if report.result_matches {
        crate::ui::note(&format!(
            "replay reproduced: projected savings ${:.4} ({:.1}%)  ·  result sha256:{}",
            report.reproduced_savings_usd,
            report.reproduced_savings_pct,
            &report.result_digest[..16.min(report.result_digest.len())]
        ));
    } else if let Some(detail) = &report.mismatch_detail {
        crate::ui::error(detail);
    }

    match &report.attestation {
        AttestationOutcome::Absent => {
            crate::ui::note("no signed attestation bundled (reproduction proof only)");
        }
        AttestationOutcome::Verified { entries } => {
            crate::ui::note(&format!(
                "attestation OK — {entries} signed entr{} verified offline",
                if *entries == 1 { "y" } else { "ies" }
            ));
        }
        AttestationOutcome::Failed(e) => {
            crate::ui::error(&format!("attestation FAILED: {e}"));
        }
    }

    if report.passed {
        crate::ui::ok(&format!(
            "PASS — savings reproduced bit-for-bit (${:.4}, {:.1}%)",
            report.reproduced_savings_usd, report.reproduced_savings_pct
        ));
        Ok(())
    } else {
        anyhow::bail!("verify-bundle FAILED — the bundle did not reproduce (see above)");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{TimeZone, Utc};
    use tt_plan_core::{
        ModelPricing, PlanInput, ProposedRoute, RequestLog, RouteAction, RouteConditions,
    };
    use tt_telemetry::audit::{build_entry, generate_signing_key, Actor};
    use uuid::Uuid;

    use super::*;

    fn det_uuid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    /// A small, real replay input: one sonnet request routed to cheaper haiku,
    /// so the reproduced savings are strictly > 0.
    fn sample_input() -> PlanInput {
        let req = RequestLog {
            id: det_uuid(0x10),
            org_id: det_uuid(0x02),
            ts: Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap(),
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet".into(),
            input_tokens: 1000,
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
            id: det_uuid(0x99),
            name: "cheap-for-short".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["claude-3-5-sonnet".into()],
                ..Default::default()
            },
            then: RouteAction {
                format_switch: None,
                diff: false,
                target_model: Some("claude-3-5-haiku".into()),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
            },
        };
        let mut pricing = HashMap::new();
        pricing.insert(
            "anthropic:claude-3-5-haiku".to_string(),
            ModelPricing {
                input_per_million: 0.25,
                output_per_million: 1.25,
                cached_input_per_million: Some(0.025),
                batch_input_per_million: None,
                batch_output_per_million: None,
                flex_input_per_million: None,
                flex_output_per_million: None,
            },
        );
        PlanInput {
            plan_id: det_uuid(0x01),
            org_id: det_uuid(0x02),
            window_start: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            window_end: Utc.with_ymd_and_hms(2026, 5, 8, 0, 0, 0).unwrap(),
            requests: vec![req],
            proposed_routes: vec![route],
            pricing,
            config: Default::default(),
            seed: 42,
            bootstrap_iterations: 200,
        }
    }

    fn sample_bundle() -> SavingsBundle {
        let input = sample_input();
        let result = tt_plan_core::replay(input.clone()).expect("replay");
        SavingsBundle::new(input, result, None)
    }

    #[test]
    fn round_trip_produce_then_verify_passes() {
        let bundle = sample_bundle();
        // Sanity: the sample really does save money (else the test is vacuous).
        assert!(bundle.expected_result.aggregates.projected_savings_usd > 0.0);

        let report = verify_bundle(&bundle).expect("verify");
        assert!(report.passed, "a freshly-produced bundle must PASS");
        assert!(report.result_matches);
        assert_eq!(report.attestation, AttestationOutcome::Absent);
        assert!(report.mismatch_detail.is_none());
        assert!(
            (report.reproduced_savings_usd
                - bundle.expected_result.aggregates.projected_savings_usd)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn json_round_trip_through_disk_still_verifies() {
        let bundle = sample_bundle();
        let json = serde_json::to_string_pretty(&bundle).unwrap();
        let reloaded: SavingsBundle = serde_json::from_str(&json).unwrap();
        let report = verify_bundle(&reloaded).expect("verify");
        assert!(report.passed, "serialize→deserialize must not break replay");
    }

    #[test]
    fn tampered_expected_savings_fails() {
        let mut bundle = sample_bundle();
        // Corrupt ONE expected-output value.
        bundle.expected_result.aggregates.projected_savings_usd += 0.01;
        let report = verify_bundle(&bundle).expect("verify runs");
        assert!(!report.passed, "a tampered expected output must FAIL");
        assert!(!report.result_matches);
        let detail = report.mismatch_detail.expect("detail present");
        assert!(
            detail.contains("projected_savings_usd"),
            "detail should name the tampered field: {detail}"
        );
    }

    #[test]
    fn tampered_input_pricing_fails() {
        // Tamper the INPUT instead of the output: the recomputed result then
        // diverges from the recorded expected_result → FAIL.
        let mut bundle = sample_bundle();
        if let Some(p) = bundle
            .plan_input
            .pricing
            .get_mut("anthropic:claude-3-5-haiku")
        {
            p.input_per_million = 99.0;
        }
        let report = verify_bundle(&bundle).expect("verify runs");
        assert!(!report.passed, "a tampered input must FAIL");
        assert!(!report.result_matches);
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let mut bundle = sample_bundle();
        bundle.schema_version = BUNDLE_SCHEMA_VERSION + 1;
        let err = verify_bundle(&bundle).expect_err("unsupported schema must error");
        assert!(matches!(err, BundleError::UnsupportedSchema { .. }));
    }

    #[test]
    fn valid_attestation_verifies_offline() {
        let key = generate_signing_key();
        let org = det_uuid(0x02);
        let e0 = build_entry(
            &key,
            None,
            org,
            Actor::System,
            "plan.applied".into(),
            serde_json::json!({"plan_id": det_uuid(0x01).to_string()}),
        )
        .expect("entry");
        let att = BundleAttestation {
            verifying_key: hex::encode(key.verifying_key().to_bytes()),
            entries: vec![e0],
        };
        let mut bundle = sample_bundle();
        bundle.attestation = Some(att);

        let report = verify_bundle(&bundle).expect("verify");
        assert!(report.passed, "valid attestation + reproduction must PASS");
        assert_eq!(
            report.attestation,
            AttestationOutcome::Verified { entries: 1 }
        );
    }

    #[test]
    fn tampered_attestation_signature_fails() {
        let key = generate_signing_key();
        let org = det_uuid(0x02);
        let mut e0 = build_entry(
            &key,
            None,
            org,
            Actor::System,
            "plan.applied".into(),
            serde_json::json!({"n": 1}),
        )
        .expect("entry");
        // Flip the signed payload after signing → signature no longer valid.
        e0.payload = serde_json::json!({"n": 2});
        let att = BundleAttestation {
            verifying_key: hex::encode(key.verifying_key().to_bytes()),
            entries: vec![e0],
        };
        let mut bundle = sample_bundle();
        bundle.attestation = Some(att);

        let report = verify_bundle(&bundle).expect("verify runs");
        assert!(
            !report.passed,
            "a tampered attestation must fail the overall verdict"
        );
        assert!(matches!(report.attestation, AttestationOutcome::Failed(_)));
        // Reproduction of the plan itself is unaffected — only the attestation broke.
        assert!(report.result_matches);
    }

    #[test]
    fn load_attestation_from_chain_reads_preamble_and_entries() {
        let key = generate_signing_key();
        let org = det_uuid(0x02);
        let e0 = build_entry(
            &key,
            None,
            org,
            Actor::System,
            "plan.applied".into(),
            serde_json::json!({"n": 1}),
        )
        .expect("entry");
        let vk_hex = hex::encode(key.verifying_key().to_bytes());
        let jsonl = format!(
            "{}\n{}\n",
            serde_json::json!({"meta": true, "verifying_key": vk_hex}),
            serde_json::to_string(&e0).unwrap()
        );
        let dir = std::env::temp_dir().join(format!("tt-bundle-att-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let chain = dir.join("AUDIT-CHAIN.jsonl");
        std::fs::write(&chain, jsonl).unwrap();

        let att = load_attestation_from_chain(&chain).expect("load");
        assert_eq!(att.verifying_key, vk_hex);
        assert_eq!(att.entries.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_attestation_without_preamble_errors() {
        let dir = std::env::temp_dir().join(format!("tt-bundle-att-nokey-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let chain = dir.join("AUDIT-CHAIN.jsonl");
        // A single entry, no preamble → no verifying key to bundle.
        let key = generate_signing_key();
        let e0 = build_entry(
            &key,
            None,
            det_uuid(0x02),
            Actor::System,
            "plan.applied".into(),
            serde_json::json!({"n": 1}),
        )
        .expect("entry");
        std::fs::write(&chain, serde_json::to_string(&e0).unwrap()).unwrap();
        assert!(load_attestation_from_chain(&chain).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
