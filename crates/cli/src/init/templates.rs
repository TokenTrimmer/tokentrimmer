//! Templates: embedded via include_dir at compile time, rendered via Tera.

use std::collections::HashMap;
use std::path::PathBuf;

use include_dir::{include_dir, Dir};
use tera::{Context, Tera};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("tera: {0}")]
    Tera(#[from] tera::Error),
    #[error("template not found: {0}")]
    NotFound(String),
}

static TEMPLATES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/templates/init");

/// One file to write, with its destination path (relative to repo root)
/// and rendered content.
#[derive(Debug, Clone)]
pub struct RenderedFile {
    pub dest: PathBuf,
    pub content: String,
    pub mode: u32, // 0o644 default; 0o755 for .sh scripts
}

/// Walk a `Dir` recursively and collect all `File` entries.
fn collect_files<'a>(dir: &'a include_dir::Dir<'a>) -> Vec<&'a include_dir::File<'a>> {
    let mut out = Vec::new();
    for entry in dir.entries() {
        match entry {
            include_dir::DirEntry::File(f) => out.push(f),
            include_dir::DirEntry::Dir(d) => out.extend(collect_files(d)),
        }
    }
    out
}

/// Render the complete template set into one `RenderedFile` per template.
pub fn render_all(vars: &HashMap<String, String>) -> Result<Vec<RenderedFile>, TemplateError> {
    let mut out = Vec::new();
    let mut tera = Tera::default();
    // Collect all files recursively via include_dir's find() glob.
    // fixup: find() returns an opaque impl Iterator, so we collect into a helper vec.
    let all_files = collect_files(&TEMPLATES);

    // Register every .tera file.
    for f in &all_files {
        if f.path().extension().is_some_and(|e| e == "tera") {
            let name = f.path().to_str().unwrap();
            let body = std::str::from_utf8(f.contents()).unwrap_or("");
            tera.add_raw_template(name, body)?;
        }
    }

    let mut ctx = Context::new();
    for (k, v) in vars {
        ctx.insert(k, v);
    }

    for f in &all_files {
        let path = f.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let dest_path = if ext == "tera" {
            // Strip .tera suffix.
            path.with_extension("")
        } else {
            path.to_path_buf()
        };
        let content = if ext == "tera" {
            tera.render(path.to_str().unwrap(), &ctx)?
        } else {
            std::str::from_utf8(f.contents())
                .map(|s| s.to_string())
                .map_err(|_| TemplateError::NotFound(path.display().to_string()))?
        };
        let mode = if dest_path.extension().is_some_and(|e| e == "sh") {
            0o755
        } else {
            0o644
        };
        out.push(RenderedFile {
            dest: dest_path,
            content,
            mode,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_agents_md_with_project_name() {
        let mut vars = HashMap::new();
        vars.insert("project_name".into(), "my-app".into());
        vars.insert("language".into(), "Rust".into());
        vars.insert("frameworks_csv".into(), "".into());
        vars.insert("tt_cli_version".into(), "0.1.0".into());
        vars.insert("initialized_at".into(), "2026-05-28".into());

        let files = render_all(&vars).unwrap();
        let agents = files
            .iter()
            .find(|f| f.dest.ends_with("AGENTS.md"))
            .expect("AGENTS.md missing");
        assert!(agents.content.contains("# AGENTS.md — my-app"));
        assert!(agents.content.contains("Primary language: Rust"));
    }

    #[test]
    fn sh_scripts_get_0o755() {
        let mut vars = HashMap::new();
        vars.insert("project_name".into(), "x".into());
        vars.insert("language".into(), "Rust".into());
        vars.insert("frameworks_csv".into(), "".into());
        vars.insert("tt_cli_version".into(), "0".into());
        vars.insert("initialized_at".into(), "x".into());
        let files = render_all(&vars).unwrap();
        let hook = files
            .iter()
            .find(|f| f.dest.ends_with("pre-edit-guard.sh"))
            .unwrap();
        assert_eq!(hook.mode, 0o755);
    }

    #[test]
    fn installs_committed_agent_policy_without_exposing_runtime_state() {
        let vars = HashMap::from([
            ("project_name".into(), "x".into()),
            ("language".into(), "Rust".into()),
            ("frameworks_csv".into(), String::new()),
            ("tt_cli_version".into(), "0".into()),
            ("initialized_at".into(), "x".into()),
        ]);
        let files = render_all(&vars).unwrap();
        let policy = files
            .iter()
            .find(|file| file.dest.ends_with(".tokentrimmer/agent.toml"))
            .expect("agent policy missing");
        assert!(policy.content.contains("schema_version = 1"));
        assert!(policy.content.contains("allowed_runners = []"));

        let ignore = files
            .iter()
            .find(|file| file.dest.ends_with(".gitignore.append"))
            .expect("gitignore append missing");
        assert!(ignore.content.contains("!.tokentrimmer/\n"));
        assert!(ignore.content.contains(".tokentrimmer/*\n"));
        assert!(ignore.content.contains("!.tokentrimmer/agent.toml\n"));
    }
}
