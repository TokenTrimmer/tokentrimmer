//! `tt init` — install the TokenTrimmer best-practices harness into a repo.

pub mod baseline;
pub mod detect;
pub mod manifest;
pub mod merge;
pub mod prompts;
pub mod templates;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use detect::detect;
use manifest::{classify_upgrade, Manifest, UpgradeAction};
use merge::{append_gitignore, merge_settings_json};
use templates::{render_all, RenderedFile};

#[derive(Debug, Error)]
pub enum InitError {
    #[error("not a git repo: {0}")]
    NotGit(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("templates: {0}")]
    Templates(#[from] templates::TemplateError),
    #[error("manifest: {0}")]
    Manifest(#[from] manifest::ManifestError),
    #[error("merge: {0}")]
    Merge(#[from] merge::MergeError),
    #[error("baseline: {0}")]
    Baseline(#[from] baseline::BaselineError),
}

pub struct RunOptions {
    pub root: PathBuf,
    pub language_override: Option<String>,
    pub framework_override: Option<String>,
    pub interactive: bool,
    pub upgrade: bool,
    pub force: bool,
    pub diff_only: bool,
    pub skip_baseline: bool,
    pub skip_hooks: bool,
    pub skip_workflows: bool,
    pub dry_run: bool,
    pub tt_cli_version: String,
}

#[derive(Debug)]
pub struct RunReport {
    pub files_written: u32,
    pub files_skipped: u32,
    pub baseline_findings: Option<usize>,
}

pub fn run(opts: RunOptions) -> Result<RunReport, InitError> {
    if !opts.root.join(".git").exists() {
        return Err(InitError::NotGit(opts.root.clone()));
    }

    let detection = if opts.language_override.is_some() {
        let mut d = detect::Detection::default();
        if let Some(l) = &opts.language_override {
            // Best-effort string parse; UI accepts python/typescript/rust/go/java/mixed.
            d.languages.push(match l.to_lowercase().as_str() {
                "python" => detect::Language::Python,
                "typescript" => detect::Language::TypeScript,
                "javascript" => detect::Language::JavaScript,
                "rust" => detect::Language::Rust,
                "go" => detect::Language::Go,
                "java" => detect::Language::Java,
                _ => detect::Language::Mixed,
            });
        }
        if let Some(f) = &opts.framework_override {
            d.frameworks = f.split(',').map(|s| s.trim().to_string()).collect();
        }
        d
    } else {
        detect(&opts.root)
    };

    let project_name = opts.root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();
    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("project_name".into(), project_name);
    vars.insert("language".into(), format!("{:?}", detection.languages.first().unwrap_or(&detect::Language::Unknown)));
    vars.insert("frameworks_csv".into(), detection.frameworks.join(", "));
    vars.insert("tt_cli_version".into(), opts.tt_cli_version.clone());
    vars.insert("initialized_at".into(), chrono::Utc::now().to_rfc3339());

    let files = render_all(&vars)?;

    let manifest_path = opts.root.join(".tt-init.lock");
    let existing_manifest = Manifest::load(&manifest_path)?.unwrap_or_else(|| Manifest::new(&opts.tt_cli_version));

    let mut new_manifest = existing_manifest.clone();
    let mut written = 0u32;
    let mut skipped = 0u32;

    for f in &files {
        if should_skip_by_options(f, &opts) {
            skipped += 1;
            continue;
        }
        let dest = opts.root.join(&f.dest);

        // Read disk current
        let disk_current = std::fs::read_to_string(&dest).ok();

        // Decide action
        let action = if opts.upgrade {
            classify_upgrade(&existing_manifest, &f.dest, disk_current.as_deref())
        } else if disk_current.is_none() {
            UpgradeAction::Fresh
        } else if f.dest.ends_with(".gitignore.append") || f.dest.ends_with(".gitignore") {
            UpgradeAction::SafeOverwrite // append-only, handled below
        } else {
            UpgradeAction::UserModified
        };

        match action {
            UpgradeAction::Fresh => {
                if !opts.dry_run {
                    write_file(&dest, &f.content, f.mode)?;
                }
                new_manifest.record(&f.dest, &f.content);
                written += 1;
                println!("+ Wrote {} ({} bytes)", f.dest.display(), f.content.len());
            }
            UpgradeAction::SafeOverwrite => {
                let new_content = if f.dest.file_name().is_some_and(|n| n == ".gitignore.append") {
                    let target_gitignore = opts.root.join(".gitignore");
                    let existing_gi = std::fs::read_to_string(&target_gitignore).unwrap_or_default();
                    let merged = append_gitignore(&existing_gi, &f.content);
                    if !opts.dry_run {
                        std::fs::write(&target_gitignore, &merged)?;
                    }
                    written += 1;
                    println!("+ Updated .gitignore");
                    continue;
                } else if f.dest.ends_with("settings.json") {
                    let existing = disk_current.as_deref().unwrap_or("{}");
                    let merged = merge_settings_json(existing, &f.content)?;
                    if !opts.dry_run {
                        write_file(&dest, &merged, f.mode)?;
                    }
                    merged
                } else {
                    if !opts.dry_run {
                        write_file(&dest, &f.content, f.mode)?;
                    }
                    f.content.clone()
                };
                new_manifest.record(&f.dest, &new_content);
                written += 1;
                println!("+ Updated {} (safe — unchanged from prior install)", f.dest.display());
            }
            UpgradeAction::UserModified => {
                if opts.force {
                    if !opts.dry_run {
                        write_file(&dest, &f.content, f.mode)?;
                    }
                    new_manifest.record(&f.dest, &f.content);
                    written += 1;
                    println!("! Overwrote user-modified {} (--force)", f.dest.display());
                } else {
                    skipped += 1;
                    println!("- Skipped {} (user-modified; --force to overwrite)", f.dest.display());
                }
            }
        }
    }

    if !opts.dry_run {
        new_manifest.save(&manifest_path)?;
    }

    let baseline_findings = if opts.skip_baseline {
        if !opts.dry_run {
            baseline::write_skipped_baseline(&opts.root)?;
        }
        None
    } else if opts.dry_run {
        None
    } else {
        Some(baseline::run_baseline(&opts.root)?)
    };

    println!();
    println!("Detected: {:?} + frameworks {:?}", detection.languages, detection.frameworks);
    println!("Files written: {written}, skipped: {skipped}");
    if let Some(n) = baseline_findings {
        println!("Inspect baseline: {n} findings -> .claude/inspect-baseline.json");
    }

    Ok(RunReport { files_written: written, files_skipped: skipped, baseline_findings })
}

fn should_skip_by_options(f: &RenderedFile, opts: &RunOptions) -> bool {
    let path_str = f.dest.to_string_lossy();
    if opts.skip_hooks && path_str.contains(".claude/hooks/") {
        return true;
    }
    if opts.skip_workflows && path_str.contains(".github/workflows/") {
        return true;
    }
    false
}

fn write_file(dest: &Path, content: &str, mode: u32) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, content)?;
    set_mode(dest, mode)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(dest: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_dest: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}
