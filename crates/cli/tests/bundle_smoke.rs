//! End-to-end smoke for the reproduce-this-savings bundle: build the `tt`
//! binary, produce a bundle from the built-in `--example` plan, verify it
//! (PASS + exit 0), then corrupt one expected-output value and confirm
//! `tt verify-bundle` FAILs with a non-zero exit. Mirrors the spawn-the-binary
//! pattern used by the other `*_smoke` tests.

use std::process::{Command, Stdio};

/// Run `tt <args...>` with an isolated `$HOME` and scrubbed gateway env.
fn run_tt(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tt"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TT_API_KEY")
        .env_remove("TT_API_BASE")
        .env("NO_COLOR", "1")
        // Silence the telemetry "tracing initialized" INFO line so captured
        // stdout is the pure `--example` JSON (and stderr is just the report).
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .output()
        .expect("spawn tt")
}

#[test]
fn produce_verify_and_tamper_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let plan_path = work.path().join("plan.json");
    let bundle_path = work.path().join("bundle.json");

    // 1. Materialize a runnable PlanInput from the built-in example.
    let example = run_tt(home.path(), &["plan", "--example"]);
    assert!(
        example.status.success(),
        "tt plan --example failed: {}",
        String::from_utf8_lossy(&example.stderr)
    );
    std::fs::write(&plan_path, &example.stdout).unwrap();

    // 2. Produce a bundle.
    let produce = run_tt(
        home.path(),
        &[
            "plan",
            "--input",
            plan_path.to_str().unwrap(),
            "--emit-bundle",
            bundle_path.to_str().unwrap(),
        ],
    );
    assert!(
        produce.status.success(),
        "tt plan --emit-bundle failed: {}",
        String::from_utf8_lossy(&produce.stderr)
    );
    assert!(bundle_path.exists(), "bundle file must be written");

    // 3. Verify the pristine bundle → PASS, exit 0.
    let verify = run_tt(
        home.path(),
        &["verify-bundle", bundle_path.to_str().unwrap()],
    );
    let verify_err = String::from_utf8_lossy(&verify.stderr);
    assert!(
        verify.status.success(),
        "verify-bundle on a pristine bundle must PASS (exit 0); stderr:\n{verify_err}"
    );
    assert!(
        verify_err.contains("PASS"),
        "expected a PASS line; stderr:\n{verify_err}"
    );

    // 4. Tamper ONE expected-output value, then verify → FAIL, non-zero exit.
    let raw = std::fs::read_to_string(&bundle_path).unwrap();
    let mut bundle: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let savings = &mut bundle["expected_result"]["aggregates"]["projected_savings_usd"];
    let original = savings.as_f64().expect("savings is a number");
    *savings = serde_json::json!(original + 1.0);
    std::fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

    let tampered = run_tt(
        home.path(),
        &["verify-bundle", bundle_path.to_str().unwrap()],
    );
    let tampered_err = String::from_utf8_lossy(&tampered.stderr);
    assert!(
        !tampered.status.success(),
        "verify-bundle on a tampered bundle must FAIL (non-zero exit); stderr:\n{tampered_err}"
    );
    assert!(
        tampered_err.contains("FAILED") || tampered_err.contains("projected_savings_usd"),
        "expected a FAIL explanation; stderr:\n{tampered_err}"
    );
}

/// Produce a bundle that EMBEDS a signed attestation, then verify it offline
/// end-to-end through the binary: the reproduction PASSes AND the attestation's
/// Ed25519 chain verifies. The attestation is a real signed `AUDIT-CHAIN.jsonl`
/// minted with the public `tt_cli::local_audit` helper (the same path
/// `tt plan --apply` uses), so no crypto is faked.
#[test]
fn bundle_with_signed_attestation_verifies_offline() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let plan_path = work.path().join("plan.json");
    let chain_path = work.path().join("AUDIT-CHAIN.jsonl");
    let bundle_path = work.path().join("bundle.json");

    // A real signed chain, keyed by a signing key generated under this test's HOME.
    let key = {
        std::env::set_var("HOME", home.path());
        tt_cli::local_audit::load_or_create_signing_key().expect("signing key")
    };
    tt_cli::local_audit::append_entry(
        &chain_path,
        &key,
        uuid::Uuid::from_u128(0x02),
        "plan.applied",
        serde_json::json!({"plan_id": "00000000-0000-0000-0000-000000000001"}),
    )
    .expect("append signed attestation entry");

    // Materialize the example plan, produce a bundle embedding the attestation.
    let example = run_tt(home.path(), &["plan", "--example"]);
    assert!(example.status.success());
    std::fs::write(&plan_path, &example.stdout).unwrap();

    let produce = run_tt(
        home.path(),
        &[
            "plan",
            "--input",
            plan_path.to_str().unwrap(),
            "--emit-bundle",
            bundle_path.to_str().unwrap(),
            "--attestation",
            chain_path.to_str().unwrap(),
        ],
    );
    assert!(
        produce.status.success(),
        "produce with --attestation failed: {}",
        String::from_utf8_lossy(&produce.stderr)
    );

    let verify = run_tt(
        home.path(),
        &["verify-bundle", bundle_path.to_str().unwrap()],
    );
    let err = String::from_utf8_lossy(&verify.stderr);
    assert!(
        verify.status.success(),
        "verify with a valid attestation must PASS; stderr:\n{err}"
    );
    assert!(
        err.contains("attestation OK"),
        "expected the attestation-verified line; stderr:\n{err}"
    );
}
