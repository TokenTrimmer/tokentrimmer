//! `tt agent run` end-to-end: spawn the built `tt` binary pointed at an
//! httpmock gateway and assert the command POSTs `/v1/agent/runs`, drives the
//! client agent loop, and prints the final answer + aggregate cost. Mirrors the
//! `proxy_smoke` spawn-against-mock pattern.

use std::process::{Command, Stdio};

use httpmock::prelude::*;

/// Run `tt agent run <prompt>` against `base`, with an isolated `$HOME` and
/// scrubbed gateway env so the flag-supplied key/base are the only config.
fn run_agent(base: &str, args: &[&str]) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    let mut full = vec!["agent", "run"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["--tt-api-key", "tt_test_smoke", "--tt-api-base", base]);
    Command::new(env!("CARGO_BIN_EXE_tt"))
        .args(&full)
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TT_API_KEY")
        .env_remove("TT_API_BASE")
        .stdin(Stdio::null())
        .output()
        .expect("run tt agent run")
}

#[test]
fn agent_run_drives_loop_and_prints_answer() {
    let server = MockServer::start();
    // The gateway runs the whole loop server-side and returns a completed run.
    let create = server.mock(|when, then| {
        when.method(POST).path("/v1/agent/runs");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::json!({
                "id": "00000000-0000-0000-0000-000000000001",
                "status": "completed",
                "turns": 2,
                "usage": { "prompt_tokens": 11, "completion_tokens": 5, "cost_usd": 0.0007 },
                "messages": [
                    { "role": "user", "content": "say hi" },
                    { "role": "assistant", "content": "Hi there!" }
                ]
            }));
    });

    let out = run_agent(&server.base_url(), &["say hi"]);
    create.assert();

    assert!(
        out.status.success(),
        "tt agent run exited non-zero; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "panic; stderr:\n{stderr}");
    // Final answer is printed to stdout; cost/status detail to stderr.
    assert!(stdout.contains("Hi there!"), "stdout:\n{stdout}");
    assert!(stderr.contains("cost: $0.000700"), "stderr:\n{stderr}");
    assert!(stderr.contains("Completed"), "stderr:\n{stderr}");
}

#[test]
fn agent_run_forwards_max_cost_to_body() {
    let server = MockServer::start();
    let create = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/agent/runs")
            .body_includes("\"max_cost_usd\":0.5");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::json!({
                "id": "00000000-0000-0000-0000-000000000000",
                "status": "completed", "messages": [], "turns": 1,
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "cost_usd": 0.1}
            }));
    });
    let out = run_agent(&server.base_url(), &["say hi", "--max-cost", "0.5"]);
    create.assert();
    assert!(out.status.success());
}

#[test]
fn agent_run_with_tools_advertises_gateway_tools() {
    let server = MockServer::start();
    // Assert the four read-only gateway tools are advertised when --tools is set.
    let create = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/agent/runs")
            .body_includes("find_route_for")
            .body_includes("preview_cost")
            .body_includes("\"max_turns\":3");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::json!({
                "id": "x", "status": "completed", "turns": 1,
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "cost_usd": 0.0 },
                "messages": [{ "role": "assistant", "content": "ok" }]
            }));
    });

    let out = run_agent(
        &server.base_url(),
        &["analyze cost", "--tools", "--max-turns", "3"],
    );
    create.assert();
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
