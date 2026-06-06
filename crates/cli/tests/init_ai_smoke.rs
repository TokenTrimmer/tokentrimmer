//! `tt init --ai`: deterministic init + an AI pass (mock gateway) tailors
//! budget.toml caps + an AGENTS.md section; idempotent across re-runs.

use httpmock::prelude::*;
use serde_json::json;
use tt_cli::init::{ai_tailor, run, RunOptions};

fn git_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".git")).unwrap();
    std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(d.path().join("main.rs"), "// uses gpt-4o\n").unwrap();
    d
}

fn base_opts(root: std::path::PathBuf) -> RunOptions {
    RunOptions {
        root,
        language_override: None,
        framework_override: None,
        interactive: false,
        upgrade: false,
        force: false,
        diff_only: false,
        skip_baseline: true,
        skip_hooks: false,
        skip_workflows: false,
        dry_run: false,
        tt_cli_version: "0.1.0".into(),
    }
}

fn mock_chat(server: &MockServer, content: &str) {
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({
                "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
                "model": "gpt-4o-mini",
                "choices": [{ "index": 0, "finish_reason": "stop",
                    "message": { "role": "assistant", "content": content } }],
                "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 }
            }));
    });
}

#[tokio::test]
async fn init_ai_tailors_budget_and_agents_idempotently() {
    let d = git_dir();
    run(base_opts(d.path().to_path_buf())).unwrap();

    let server = MockServer::start_async().await;
    mock_chat(
        &server,
        r#"{"daily_cap_usd": 7, "weekly_cap_usd": 35,
            "routes": [{"from": "gpt-4o", "to": "gpt-4o-mini", "reason": "10x cheaper"}],
            "notes": "Conservative starter caps."}"#,
    );

    ai_tailor(d.path(), None, Some("k".into()), Some(server.base_url()))
        .await
        .unwrap();

    let budget = std::fs::read_to_string(d.path().join(".claude/budget.toml")).unwrap();
    assert!(budget.contains("daily_cap_usd = 7"), "{budget}");
    assert!(budget.contains("weekly_cap_usd = 35"), "{budget}");

    let agents = std::fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
    assert!(agents.contains("<!-- tt:ai:start -->"));
    assert!(agents.contains("`gpt-4o` → `gpt-4o-mini`"), "{agents}");
    assert!(agents.contains("Conservative starter caps."));

    // Re-run → exactly one AI section (idempotent).
    ai_tailor(d.path(), None, Some("k".into()), Some(server.base_url()))
        .await
        .unwrap();
    let agents2 = std::fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
    assert_eq!(
        agents2.matches("<!-- tt:ai:start -->").count(),
        1,
        "{agents2}"
    );
}
