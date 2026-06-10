//! OpenAI-SDK request conformance suite.
//!
//! Feeds request-body JSON shaped exactly like what the official
//! `openai-python` / `openai-node` SDKs emit through
//! `deserialize -> serialize` on [`ChatCompletionRequest`] and asserts that
//! **no top-level field is lost**. This guards the gateway's compat fidelity:
//! a client's `max_completion_tokens` (the spend cap!), `stream_options`,
//! `parallel_tool_calls`, `reasoning_effort`, and any genuinely-unknown / newer
//! field must survive the round-trip instead of being silently dropped.
//!
//! Hermetic: fixtures are committed under `tests/fixtures/openai_sdk_requests/`
//! and read from disk — no network.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tt_shared::ChatCompletionRequest;

/// Directory holding the committed SDK-shaped request fixtures.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("openai_sdk_requests")
}

/// All `*.json` fixtures, sorted for deterministic iteration.
fn fixture_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures dir is readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "expected at least one fixture");
    files
}

/// Round-trip a single fixture and assert every top-level input key survives
/// with an equal value.
fn assert_no_field_lost(path: &Path) {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let input: Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));

    // deserialize -> the canonical type -> serialize back to JSON.
    let req: ChatCompletionRequest = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("deserialize {}: {e}", path.display()));
    let output =
        serde_json::to_value(&req).unwrap_or_else(|e| panic!("serialize {}: {e}", path.display()));

    let in_obj = input.as_object().expect("fixture is a JSON object");
    let out_obj = output.as_object().expect("re-serialized is a JSON object");

    for (key, in_val) in in_obj {
        let out_val = out_obj.get(key).unwrap_or_else(|| {
            panic!(
                "{}: top-level field `{key}` was DROPPED on round-trip",
                path.display()
            )
        });
        assert!(
            values_equivalent(in_val, out_val),
            "{}: field `{key}` changed on round-trip ({in_val} -> {out_val})",
            path.display()
        );
    }
}

/// Compare two JSON values for semantic equivalence, tolerating the precision
/// loss that occurs when a JSON number round-trips through an `f32` field
/// (e.g. `temperature: 0.7` -> `0.699999988079071`). Everything else is exact.
fn values_equivalent(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(xf), Some(yf)) => (xf - yf).abs() <= 1e-6 * xf.abs().max(1.0),
            _ => x == y,
        },
        (Value::Array(xa), Value::Array(ya)) => {
            xa.len() == ya.len() && xa.iter().zip(ya).all(|(x, y)| values_equivalent(x, y))
        }
        (Value::Object(xo), Value::Object(yo)) => {
            xo.len() == yo.len()
                && xo
                    .iter()
                    .all(|(k, xv)| yo.get(k).is_some_and(|yv| values_equivalent(xv, yv)))
        }
        _ => a == b,
    }
}

#[test]
fn every_sdk_fixture_round_trips_without_field_loss() {
    for path in fixture_files() {
        assert_no_field_lost(&path);
    }
}

#[test]
fn reasoning_spend_cap_fields_survive() {
    // Targeted assertion on the highest-risk fixture: the spend cap and
    // reasoning effort must be modeled as typed fields (not just passthrough),
    // because adapters branch on them.
    let path = fixtures_dir().join("03_reasoning_o3_python.json");
    let raw = std::fs::read_to_string(&path).expect("read reasoning fixture");
    let req: ChatCompletionRequest = serde_json::from_str(&raw).expect("deserialize");
    assert_eq!(req.max_completion_tokens, Some(4096));
    assert_eq!(req.reasoning_effort.as_deref(), Some("high"));
    // These are modeled fields, so they must NOT land in the passthrough map.
    assert!(req.extra.is_empty());
}

#[test]
fn unknown_fields_land_in_passthrough_map() {
    let path = fixtures_dir().join("05_newer_fields_passthrough.json");
    let raw = std::fs::read_to_string(&path).expect("read passthrough fixture");
    let req: ChatCompletionRequest = serde_json::from_str(&raw).expect("deserialize");
    // None of these are modeled, so they must be captured for passthrough.
    for key in ["logprobs", "top_logprobs", "service_tier", "metadata"] {
        assert!(
            req.extra.contains_key(key),
            "unknown field `{key}` should be captured in `extra`"
        );
    }
}
