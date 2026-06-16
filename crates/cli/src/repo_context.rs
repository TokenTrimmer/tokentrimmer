//! `tt context` — print the most relevant repo files for a task (json or md).
use tt_context::assemble::ContextPack;

pub fn run(
    path: &str,
    task: &str,
    format: &str,
    max_files: usize,
    token_budget: u32,
) -> anyhow::Result<()> {
    let pack = tt_context::repo_context(std::path::Path::new(path), task, max_files, token_budget);
    println!("{}", render(&pack, format));
    Ok(())
}

fn render(pack: &ContextPack, format: &str) -> String {
    if format.eq_ignore_ascii_case("json") {
        return serde_json::to_string_pretty(pack).unwrap_or_else(|_| "{}".into());
    }
    let mut out = String::new();
    out.push_str(&format!(
        "# Relevant context ({} files, ~{} inlined tokens)\n\n",
        pack.files.len(),
        pack.token_estimate
    ));
    out.push_str(&format!("> {}\n\n", pack.note));
    for f in &pack.files {
        out.push_str(&format!("## {}\n", f.path.display()));
        out.push_str(&format!("- {}\n", f.summary));
        if !f.reasons.is_empty() {
            out.push_str(&format!("- why: {}\n", f.reasons.join("; ")));
        }
        if let Some(c) = &f.content {
            out.push_str("\n```\n");
            out.push_str(c);
            out.push_str("\n```\n");
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_json_and_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "def helper():\n    pass\n").unwrap();
        let pack = tt_context::repo_context(dir.path(), "helper", 5, 1000);
        let j = render(&pack, "json");
        assert!(serde_json::from_str::<serde_json::Value>(&j).is_ok());
        let md = render(&pack, "md");
        assert!(md.contains("a.py"));
    }
}
