//! Spawn smoke tests: run the built `tt` binary once per top-level subcommand
//! with a short-lived (or early-killed) invocation and assert it never panics.
//!
//! Regression guard for the nested-runtime startup panics — the Mcp,
//! Retrieval, and Proxy arms used to call `Runtime::block_on` from inside
//! `#[tokio::main]`, which aborts with "Cannot start a runtime from within a
//! runtime". In-process tests can't catch that class of bug; only spawning
//! the real binary exercises the `main()` command arms.
//!
//! Each test isolates `$HOME` (so `~/.tokentrimmer` is never touched) and
//! scrubs TT_*/gateway env vars. Network-touching commands point at a closed
//! loopback port for an immediate, clean connection-refused error — a non-zero
//! exit is fine; a panic is not.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Closed loopback port → immediate ECONNREFUSED, no real traffic.
const REFUSED_BASE: &str = "http://127.0.0.1:9";
/// Generous ceiling for commands that must exit on their own (CI can be slow).
const EXIT_WAIT: Duration = Duration::from_secs(30);
/// How long a long-running server gets to crash at startup before we declare
/// it healthy and kill it. Generous so a loaded CI machine still reaches the
/// would-be panic point; a crashing process exits (and is detected) far sooner.
const SERVER_GRACE: Duration = Duration::from_secs(4);

struct SpawnResult {
    exited: bool,
    stderr: String,
}

/// Spawn `tt <args>` with an isolated `$HOME`, closed stdin, and scrubbed
/// gateway env. Wait up to `wait` for exit, then kill whatever is left and
/// collect stderr.
fn run_tt(home: &std::path::Path, args: &[&str], wait: Duration) -> SpawnResult {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tt"));
    cmd.args(args)
        .env("HOME", home)
        // `dirs` honors XDG_* on Linux; scrub them so $HOME isolation holds.
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("TT_API_KEY")
        .env_remove("TT_API_BASE")
        .env_remove("DATABASE_URL")
        .env_remove("REDIS_URL")
        .env_remove("SENTRY_DSN")
        .env_remove("TT_MASTER_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("PORT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn tt binary");

    let deadline = Instant::now() + wait;
    let mut exited = false;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(_) => {
                exited = true;
                break;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    if !exited {
        let _ = child.kill();
    }
    let out = child.wait_with_output().expect("collect output");
    SpawnResult {
        exited,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// The command must exit on its own (success OR clean error) without panicking.
fn assert_exits_cleanly(args: &[&str]) {
    let home = tempfile::tempdir().unwrap();
    let r = run_tt(home.path(), args, EXIT_WAIT);
    assert!(
        r.exited,
        "`tt {}` did not exit within {EXIT_WAIT:?}; stderr:\n{}",
        args.join(" "),
        r.stderr
    );
    assert_no_panic(args, &r.stderr);
}

/// A long-running server: give it a grace window to crash at startup, then
/// kill it. Either way, stderr must not show a panic.
fn assert_server_starts_without_panic(args: &[&str]) {
    let home = tempfile::tempdir().unwrap();
    let r = run_tt(home.path(), args, SERVER_GRACE);
    assert_no_panic(args, &r.stderr);
}

fn assert_no_panic(args: &[&str], stderr: &str) {
    assert!(
        !stderr.contains("panicked"),
        "`tt {}` panicked; stderr:\n{stderr}",
        args.join(" ")
    );
}

// --- one test per top-level subcommand --------------------------------------

#[test]
fn gateway_spawns_without_panic() {
    // DATABASE_URL is scrubbed → clean "--migrate-only requires DATABASE_URL".
    assert_exits_cleanly(&["gateway", "--migrate-only"]);
}

#[test]
fn inspect_spawns_without_panic() {
    let dir = tempfile::tempdir().unwrap();
    assert_exits_cleanly(&["inspect", dir.path().to_str().unwrap()]);
}

#[test]
fn plan_spawns_without_panic() {
    assert_exits_cleanly(&["plan", "--example"]);
}

#[test]
fn audit_spawns_without_panic() {
    // Nonexistent chain file → clean error.
    assert_exits_cleanly(&["audit", "verify", "/nonexistent/tt-smoke/chain.jsonl"]);
}

#[test]
fn audit_create_checkpoint_help_is_exposed() {
    assert_exits_cleanly(&["audit", "create-checkpoint", "--help"]);
}

#[test]
fn mcp_spawns_without_panic() {
    // stdio transport with stdin already at EOF → serves zero requests and
    // exits. This is the arm that used to die on a nested Runtime::block_on.
    assert_exits_cleanly(&["mcp", "--tt-api-key", "tt_test_smoke"]);
}

#[test]
fn mcp_allow_write_without_database_fails_closed() {
    // --allow-write without DATABASE_URL must refuse to boot — write tools are
    // org-scoped and need store-backed key verification. A clear error, not a
    // silent fall-back to a read-only server the operator believes is writable.
    let home = tempfile::tempdir().unwrap();
    let args = &["mcp", "--tt-api-key", "tt_test_smoke", "--allow-write"];
    let r = run_tt(home.path(), args, EXIT_WAIT);
    assert!(
        r.exited,
        "`tt mcp --allow-write` did not exit; stderr:\n{}",
        r.stderr
    );
    assert_no_panic(args, &r.stderr);
    assert!(
        r.stderr.contains("--allow-write requires DATABASE_URL"),
        "expected the fail-closed message, got stderr:\n{}",
        r.stderr
    );
}

#[test]
fn login_spawns_without_panic() {
    // --token path: no browser, writes only under the isolated $HOME.
    assert_exits_cleanly(&["login", "--token", "tt_test_smoke"]);
}

#[test]
fn logout_spawns_without_panic() {
    assert_exits_cleanly(&["logout"]);
}

#[test]
fn whoami_spawns_without_panic() {
    // No key anywhere → documented exit 1, cleanly.
    assert_exits_cleanly(&["whoami"]);
}

#[test]
fn chat_spawns_without_panic() {
    // Catalog fetch fails fast (refused), then readline sees EOF and quits.
    assert_exits_cleanly(&[
        "chat",
        "--tt-api-key",
        "tt_test_smoke",
        "--tt-api-base",
        REFUSED_BASE,
    ]);
}

#[test]
fn models_spawns_without_panic() {
    assert_exits_cleanly(&[
        "models",
        "--tt-api-key",
        "tt_test_smoke",
        "--tt-api-base",
        REFUSED_BASE,
    ]);
}

#[test]
fn embed_spawns_without_panic() {
    assert_exits_cleanly(&[
        "embed",
        "hello",
        "--tt-api-key",
        "tt_test_smoke",
        "--tt-api-base",
        REFUSED_BASE,
    ]);
}

#[test]
fn advise_spawns_without_panic() {
    let dir = tempfile::tempdir().unwrap();
    assert_exits_cleanly(&[
        "advise",
        dir.path().to_str().unwrap(),
        "--tt-api-key",
        "tt_test_smoke",
        "--tt-api-base",
        REFUSED_BASE,
    ]);
}

#[test]
fn agent_run_spawns_without_panic() {
    // Create POST hits the gateway; a refused base → clean connection error,
    // no panic.
    assert_exits_cleanly(&[
        "agent",
        "run",
        "say hi",
        "--tt-api-key",
        "tt_test_smoke",
        "--tt-api-base",
        REFUSED_BASE,
    ]);
}

#[test]
fn init_spawns_without_panic() {
    // Non-git tempdir → clean refusal; --dry-run guarantees no writes anyway.
    let dir = tempfile::tempdir().unwrap();
    assert_exits_cleanly(&["init", "--path", dir.path().to_str().unwrap(), "--dry-run"]);
}

#[test]
fn retrieval_spawns_without_panic() {
    // Nonexistent doc → clean error before any network use. This is the arm
    // that used to die on a nested Runtime::block_on.
    assert_exits_cleanly(&[
        "retrieval",
        "doc-add",
        "smoke",
        "/nonexistent/tt-smoke/doc.txt",
        "--openai-key",
        "sk-smoke",
    ]);
}

#[test]
fn retrieval_help_labels_subcommands_experimental() {
    // The CLI retrieval store is in-process only: nothing survives a fresh
    // process. Until that's wired to a persistent store, both subcommands must
    // be labelled EXPERIMENTAL in their help so users aren't misled into
    // thinking `doc-add` builds a durable corpus. `--help` is fully offline.
    let home = tempfile::tempdir().unwrap();

    for sub in [
        vec!["retrieval", "doc-add", "--help"],
        vec!["retrieval", "search", "--help"],
    ] {
        let r = run_tt(home.path(), &sub, EXIT_WAIT);
        assert!(r.exited, "`tt {}` did not exit", sub.join(" "));
        assert_no_panic(&sub, &r.stderr);
        // clap prints --help to stdout, but our harness only captures stderr;
        // re-capture stdout here for the assertion.
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_tt"))
            .args(&sub)
            .env("HOME", home.path())
            .env_remove("OPENAI_API_KEY")
            .stdin(Stdio::null())
            .output()
            .expect("run tt --help");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("EXPERIMENTAL"),
            "`tt {}` help is missing the EXPERIMENTAL label; stdout:\n{stdout}",
            sub.join(" ")
        );
    }
}

#[test]
fn proxy_spawns_without_panic() {
    // Long-running server on an ephemeral port; killed after the grace window.
    // This is the arm that used to die on a nested Runtime::block_on.
    assert_server_starts_without_panic(&[
        "proxy",
        "--mode",
        "bypass",
        "--no-tui",
        "--no-preview",
        "--port",
        "0",
    ]);
}

#[test]
fn route_spawns_without_panic() {
    assert_exits_cleanly(&[
        "route",
        "list",
        "--tt-api-key",
        "tt_test_smoke",
        "--tt-api-base",
        REFUSED_BASE,
    ]);
}

#[test]
fn recipes_list_spawns_without_panic() {
    // `recipes list` is fully offline (embedded assets) → exits 0 cleanly.
    assert_exits_cleanly(&["recipes", "list"]);
}

#[test]
fn recipes_apply_spawns_without_panic() {
    // Apply hits the gateway; a refused base → clean connection error, no panic.
    assert_exits_cleanly(&[
        "recipes",
        "apply",
        "cheap-classification",
        "--tt-api-key",
        "tt_test_smoke",
        "--tt-api-base",
        REFUSED_BASE,
    ]);
}

/// Run `tt <args>` to completion, draining stdout+stderr concurrently (so a
/// large completion script can't deadlock on a full pipe the way the
/// `try_wait`-polling `run_tt` harness would), and return (stdout, stderr).
fn output_tt(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tt"))
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("run tt binary to completion")
}

/// `tt completions <shell>` must exit cleanly and emit a non-empty script to
/// stdout for every supported shell. Fully offline.
#[test]
fn completions_emit_scripts_for_every_shell() {
    let home = tempfile::tempdir().unwrap();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let out = output_tt(home.path(), &["completions", shell]);
        assert!(
            out.status.success(),
            "`tt completions {shell}` exited non-zero"
        );
        assert!(
            !out.stdout.is_empty(),
            "`tt completions {shell}` produced no output"
        );
        // The generated script must not be corrupted by a startup log line.
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("panicked"),
            "`tt completions {shell}` panicked"
        );
    }
}

/// `tt man` must exit cleanly and emit a non-empty roff man page to stdout.
#[test]
fn man_emits_a_page() {
    let home = tempfile::tempdir().unwrap();
    let out = output_tt(home.path(), &["man"]);
    assert!(out.status.success(), "`tt man` exited non-zero");
    assert!(!out.stdout.is_empty(), "`tt man` produced no output");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("panicked"),
        "`tt man` panicked"
    );
}
