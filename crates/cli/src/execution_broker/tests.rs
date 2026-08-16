use std::{path::Path, time::Duration};

use tt_client::ToolExecutor as _;

use crate::agent_policy::{
    AgentPolicy, AgentRunner, ApprovalGate, ApprovalPolicy, CommandRule, CostBudgets,
    FilesystemPolicy, InferencePolicy, NetworkDefault, NetworkDestination, NetworkPolicy,
    PolicyCostBasis, ProcessPolicy, ResolvedAgentPolicy, RunLimits, ValidationPolicy,
    AGENT_POLICY_SCHEMA_VERSION,
};

use super::*;

fn policy(
    commands: Vec<CommandRule>,
    destinations: Vec<NetworkDestination>,
) -> ResolvedAgentPolicy {
    ResolvedAgentPolicy {
        policy: AgentPolicy {
            schema_version: AGENT_POLICY_SCHEMA_VERSION,
            filesystem: FilesystemPolicy {
                readable_roots: vec![".".into()],
                writable_roots: vec!["src".into()],
                max_files: 100,
                max_file_bytes: 128 * 1024,
                max_total_read_bytes: 1024 * 1024,
                max_total_write_bytes: 1024 * 1024,
                allow_symlinks: false,
                excluded_paths: vec![
                    ".env".into(),
                    ".env/**".into(),
                    ".git".into(),
                    ".git/**".into(),
                ],
            },
            process: ProcessPolicy {
                allowed_commands: commands,
                max_subprocesses: 8,
                max_duration_seconds: 1,
                max_output_bytes: 16 * 1024,
                allow_shell: true,
            },
            network: NetworkPolicy {
                default: NetworkDefault::Deny,
                allowed_destinations: destinations,
                allow_redirects: false,
                inherit_proxy_env: false,
            },
            inference: InferencePolicy {
                allowed_runners: vec![AgentRunner::TokenTrimmerApi],
                allowed_providers: vec!["openai".into()],
                allowed_models: vec!["openai/gpt-5".into()],
                allowed_cost_bases: vec![PolicyCostBasis::ApiMetered],
            },
            limits: RunLimits {
                max_api_calls: 10,
                max_model_turns: 10,
                max_retries: 0,
                max_wall_time_seconds: 30,
                max_diff_bytes: 32 * 1024,
                max_changed_files: 5,
            },
            budgets: CostBudgets {
                max_api_cash_micros: 1_000_000,
                max_subscription_marginal_cash_micros: 0,
                max_subscription_allocated_micros: 0,
                max_self_hosted_tco_micros: 0,
                subscription_quota_caps: Vec::new(),
                allow_unmeasured: false,
            },
            approvals: ApprovalPolicy {
                destructive_operations: ApprovalGate::Deny,
                rollback: ApprovalGate::Deny,
            },
            validation: ValidationPolicy {
                required_commands: Vec::new(),
                stop_on_regression: true,
            },
        },
        effective_sha256: "fixture-policy-sha256".into(),
        layers: Vec::new(),
    }
}

fn repository() -> tempfile::TempDir {
    let repository = tempfile::tempdir().unwrap();
    std::fs::create_dir(repository.path().join("src")).unwrap();
    std::fs::write(repository.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(repository.path().join("README.txt"), "safe\n").unwrap();
    std::fs::write(repository.path().join(".env"), "SECRET=canary\n").unwrap();
    repository
}

async fn call(
    broker: &LocalExecutionBroker,
    name: &str,
    value: serde_json::Value,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    broker.call(name, &value.to_string()).await
}

#[tokio::test]
async fn stages_bounded_files_without_touching_source() {
    let repository = repository();
    let broker =
        LocalExecutionBroker::new(repository.path(), "run-files", &policy(vec![], vec![])).unwrap();

    let listed = call(&broker, LIST_FILES_TOOL, serde_json::json!({"path": "."}))
        .await
        .unwrap();
    assert!(listed.contains("src/main.rs"));
    assert!(!listed.contains(".env"));
    let read = call(
        &broker,
        READ_FILE_TOOL,
        serde_json::json!({"path": "src/main.rs"}),
    )
    .await
    .unwrap();
    assert!(read.contains("fn main"));

    call(
        &broker,
        WRITE_FILE_TOOL,
        serde_json::json!({"path": "src/nested/new.rs", "content": "pub fn new() {}\n"}),
    )
    .await
    .unwrap();
    assert!(!repository.path().join("src/nested/new.rs").exists());

    for forbidden in ["../.env", "/etc/passwd", ".env"] {
        assert!(
            call(
                &broker,
                READ_FILE_TOOL,
                serde_json::json!({"path": forbidden}),
            )
            .await
            .is_err(),
            "unexpectedly read {forbidden}"
        );
    }
    assert!(call(
        &broker,
        "delete_file",
        serde_json::json!({"path": "src/main.rs", "approval": "invented"}),
    )
    .await
    .is_err());

    let evidence = broker.evidence().await.unwrap();
    assert!(!evidence.source_checkout_modified);
    assert_eq!(evidence.patch.changes.len(), 1);
    assert_eq!(evidence.patch.changes[0].kind, FileChangeKind::Added);
    assert!(evidence.patch.unified_diff.contains("pub fn new"));
    assert!(evidence
        .policy_decisions
        .iter()
        .any(|decision| !decision.allowed));
}

#[tokio::test]
async fn rejected_write_rolls_back_staged_state() {
    let repository = repository();
    let mut constrained = policy(vec![], vec![]);
    constrained.policy.limits.max_diff_bytes = 20;
    let broker =
        LocalExecutionBroker::new(repository.path(), "run-rollback", &constrained).unwrap();

    assert!(call(
        &broker,
        WRITE_FILE_TOOL,
        serde_json::json!({"path": "src/main.rs", "content": "this replacement is intentionally too large\n"}),
    )
    .await
    .is_err());
    assert!(broker.evidence().await.unwrap().patch.changes.is_empty());
    assert_eq!(
        std::fs::read_to_string(repository.path().join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
}

#[cfg(unix)]
#[test]
fn source_symlinks_and_hardlinks_fail_closed() {
    let outside = tempfile::NamedTempFile::new().unwrap();

    let symlink_repo = repository();
    std::os::unix::fs::symlink(outside.path(), symlink_repo.path().join("src/link")).unwrap();
    assert!(
        LocalExecutionBroker::new(symlink_repo.path(), "run-symlink", &policy(vec![], vec![]))
            .is_err()
    );

    let hardlink_repo = repository();
    std::fs::hard_link(outside.path(), hardlink_repo.path().join("src/hardlink")).unwrap();
    assert!(LocalExecutionBroker::new(
        hardlink_repo.path(),
        "run-hardlink",
        &policy(vec![], vec![])
    )
    .is_err());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_commands_are_literal_bounded_and_source_isolated() {
    let repository = repository();
    let source_file = repository.path().join("src/main.rs");
    let source_arg = source_file.display().to_string();
    let commands = vec![
        CommandRule {
            executable: "/bin/echo".into(),
            argv_prefixes: vec![vec!["literal;touch".into(), "/tmp/tt-injected".into()]],
        },
        CommandRule {
            executable: "/bin/sh".into(),
            argv_prefixes: vec![
                vec!["-c".into(), "printf changed > src/main.rs".into()],
                vec!["-c".into(), "sleep 30 & echo $!; wait".into()],
                vec!["-c".into(), "rm src/main.rs".into()],
            ],
        },
        CommandRule {
            executable: "/bin/cat".into(),
            argv_prefixes: vec![vec![source_arg.clone()]],
        },
        CommandRule {
            executable: "/usr/bin/yes".into(),
            argv_prefixes: vec![vec!["flood".into()]],
        },
    ];
    let mut execution_policy = policy(commands, vec![]);
    execution_policy.policy.process.max_output_bytes = 4 * 1024;
    execution_policy.policy.process.max_subprocesses = 6;
    let broker =
        LocalExecutionBroker::new(repository.path(), "run-process", &execution_policy).unwrap();

    let literal = call(
        &broker,
        RUN_COMMAND_TOOL,
        serde_json::json!({
            "executable": "/bin/echo",
            "args": ["literal;touch", "/tmp/tt-injected"]
        }),
    )
    .await
    .unwrap();
    assert!(literal.contains("literal;touch"), "{literal}");
    assert!(!Path::new("/tmp/tt-injected").exists());

    call(
        &broker,
        RUN_COMMAND_TOOL,
        serde_json::json!({
            "executable": "/bin/sh",
            "args": ["-c", "printf changed > src/main.rs"]
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&source_file).unwrap(),
        "fn main() {}\n"
    );
    assert!(broker
        .evidence()
        .await
        .unwrap()
        .patch
        .unified_diff
        .contains("changed"));

    let denied_source = call(
        &broker,
        RUN_COMMAND_TOOL,
        serde_json::json!({"executable": "/bin/cat", "args": [source_arg]}),
    )
    .await
    .unwrap();
    assert!(denied_source.contains("Operation not permitted"));

    let timeout = call(
        &broker,
        RUN_COMMAND_TOOL,
        serde_json::json!({
            "executable": "/bin/sh",
            "args": ["-c", "sleep 30 & echo $!; wait"]
        }),
    )
    .await
    .unwrap();
    let timeout: serde_json::Value = serde_json::from_str(&timeout).unwrap();
    assert_eq!(timeout["timed_out"], true, "{timeout}");
    let child_pid = timeout["stdout"]
        .as_str()
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let status = std::process::Command::new("/bin/ps")
        .args(["-p", &child_pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "timed-out descendant remained alive");

    assert!(call(
        &broker,
        RUN_COMMAND_TOOL,
        serde_json::json!({
            "executable": "/bin/sh",
            "args": ["-c", "rm src/main.rs"]
        }),
    )
    .await
    .is_err());
    assert!(broker
        .evidence()
        .await
        .unwrap()
        .patch
        .unified_diff
        .contains("changed"));

    let flood = call(
        &broker,
        RUN_COMMAND_TOOL,
        serde_json::json!({"executable": "/usr/bin/yes", "args": ["flood"]}),
    )
    .await
    .unwrap();
    let flood: serde_json::Value = serde_json::from_str(&flood).unwrap();
    assert_eq!(flood["output_limit_exceeded"], true);
    assert!(broker.evidence().await.unwrap().output_bytes_returned <= 4 * 1024);
    assert!(call(
        &broker,
        RUN_COMMAND_TOOL,
        serde_json::json!({
            "executable": "/bin/echo",
            "args": ["literal;touch", "/tmp/tt-injected"]
        }),
    )
    .await
    .is_err());
}

#[tokio::test]
async fn pinned_http_allows_only_exact_origin_and_sanitizes_terminal_controls() {
    let (port, server) = one_response_server(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 16\r\nConnection: close\r\n\r\n\x1b[31mFORGED\x1b[0m\n",
    )
    .await;
    let destination = NetworkDestination {
        scheme: crate::agent_policy::NetworkScheme::Http,
        host: "127.0.0.1".into(),
        port,
    };
    let broker = LocalExecutionBroker::new(
        repository().path(),
        "run-network",
        &policy(vec![], vec![destination]),
    )
    .unwrap();
    let output = call(
        &broker,
        FETCH_URL_TOOL,
        serde_json::json!({"url": format!("http://127.0.0.1:{port}/fixture")}),
    )
    .await
    .unwrap();
    server.await.unwrap();
    assert!(output.contains("FORGED"));
    assert!(!output.contains("\\u001b"));
    assert!(broker.evidence().await.unwrap().network_fetches[0]
        .resolved_addresses
        .iter()
        .all(|address| address.ends_with(&format!(":{port}"))));

    for url in [
        "http://169.254.169.254/latest/meta-data/".to_string(),
        format!("http://127.0.0.1:{}/wrong-port", port + 1),
        format!("http://user@127.0.0.1:{port}/userinfo"),
    ] {
        assert!(
            call(&broker, FETCH_URL_TOOL, serde_json::json!({"url": url}))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn redirects_must_be_explicitly_enabled_and_reauthorized() {
    let (port, server) = one_response_server(
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    let destination = NetworkDestination {
        scheme: crate::agent_policy::NetworkScheme::Http,
        host: "127.0.0.1".into(),
        port,
    };
    let broker = LocalExecutionBroker::new(
        repository().path(),
        "run-redirect",
        &policy(vec![], vec![destination]),
    )
    .unwrap();
    assert!(call(
        &broker,
        FETCH_URL_TOOL,
        serde_json::json!({"url": format!("http://127.0.0.1:{port}/redirect")}),
    )
    .await
    .is_err());
    server.await.unwrap();
}

async fn one_response_server(response: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0u8; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    (port, server)
}
