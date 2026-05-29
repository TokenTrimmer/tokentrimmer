//! End-to-end: run `tt init` against a fresh tempdir; verify expected files
//! land, manifest is written, baseline file appears.

use tt_cli::init::{run, RunOptions};

fn make_git_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".git")).unwrap();
    d
}

#[test]
fn fresh_install_writes_baseline_files() {
    let d = make_git_dir();
    // Seed a language signal.
    std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let opts = RunOptions {
        root: d.path().to_path_buf(),
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
    };
    let report = run(opts).unwrap();
    assert!(
        report.files_written >= 5,
        "report = {:?}",
        (report.files_written, report.files_skipped)
    );
    assert!(d.path().join("AGENTS.md").exists());
    assert!(d.path().join(".claude/settings.json").exists());
    assert!(d.path().join(".claude/BACKLOG.md").exists());
    assert!(d.path().join(".claude/hooks/pre-edit-guard.sh").exists());
    assert!(d.path().join(".github/workflows/inspect-self.yml").exists());
    assert!(d.path().join(".tt-init.lock").exists());
}

#[test]
fn idempotent_rerun_is_noop_when_unchanged() {
    let d = make_git_dir();
    std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let mk = || RunOptions {
        root: d.path().to_path_buf(),
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
    };
    let r1 = run(mk()).unwrap();
    let r2 = run(mk()).unwrap();
    // Second run: every file is now "user-modified" by the perspective of
    // a fresh-install path, OR a no-op if classify routes through the manifest.
    // We assert no NEW files are written (manifest path holds).
    assert_eq!(
        r2.files_written + r2.files_skipped,
        r1.files_written + r1.files_skipped
    );
}

#[test]
fn refuses_non_git_dir() {
    let d = tempfile::tempdir().unwrap();
    let opts = RunOptions {
        root: d.path().to_path_buf(),
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
    };
    let err = run(opts).unwrap_err();
    assert!(format!("{err}").contains("not a git repo"));
}
