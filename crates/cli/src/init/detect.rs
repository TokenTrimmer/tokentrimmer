//! Detect repo language(s) + LLM frameworks from manifest files.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Language {
    Python,
    TypeScript,
    JavaScript,
    Rust,
    Go,
    Java,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detection {
    pub languages: Vec<Language>,
    pub frameworks: Vec<String>, // free-form: "langchain", "openai", "ai-sdk", ...
}

pub fn detect(root: &Path) -> Detection {
    let mut langs = Vec::new();
    let mut fws = Vec::new();

    if root.join("pyproject.toml").exists()
        || root.join("requirements.txt").exists()
        || root.join("setup.py").exists()
    {
        langs.push(Language::Python);
        scan_python_frameworks(root, &mut fws);
    }
    if root.join("package.json").exists() {
        let lang = detect_js_or_ts(root);
        langs.push(lang);
        scan_js_frameworks(root, &mut fws);
    }
    if root.join("Cargo.toml").exists() {
        langs.push(Language::Rust);
    }
    if root.join("go.mod").exists() {
        langs.push(Language::Go);
    }
    if root.join("pom.xml").exists() || root.join("build.gradle").exists() {
        langs.push(Language::Java);
    }
    if langs.is_empty() {
        langs.push(Language::Unknown);
    } else if langs.len() > 1 {
        // Replace with single "Mixed" entry preserving original list separately.
        let mixed = vec![Language::Mixed];
        // Keep originals appended for callers that want detail.
        let mut all = mixed;
        all.extend(langs);
        langs = all;
    }
    Detection {
        languages: langs,
        frameworks: fws,
    }
}

fn detect_js_or_ts(root: &Path) -> Language {
    if root.join("tsconfig.json").exists() {
        return Language::TypeScript;
    }
    // Cheap heuristic: any .ts file → TS.
    if let Ok(entries) = std::fs::read_dir(root.join("src").as_path()) {
        if entries.flatten().any(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x == "ts" || x == "tsx")
        }) {
            return Language::TypeScript;
        }
    }
    Language::JavaScript
}

fn scan_python_frameworks(root: &Path, out: &mut Vec<String>) {
    let known = [
        "langchain",
        "openai",
        "anthropic",
        "instructor",
        "litellm",
        "fastapi",
    ];
    for f in ["pyproject.toml", "requirements.txt", "setup.py"] {
        if let Ok(s) = std::fs::read_to_string(root.join(f)) {
            for k in &known {
                if s.contains(k) && !out.contains(&k.to_string()) {
                    out.push(k.to_string());
                }
            }
        }
    }
}

fn scan_js_frameworks(root: &Path, out: &mut Vec<String>) {
    let known = [
        "ai",
        "@anthropic-ai/sdk",
        "openai",
        "langchain",
        "@langchain/core",
        "instructor-js",
    ];
    if let Ok(s) = std::fs::read_to_string(root.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&s) {
            for key in ["dependencies", "devDependencies"] {
                if let Some(deps) = json.get(key).and_then(|v| v.as_object()) {
                    for k in &known {
                        if deps.contains_key(*k) && !out.contains(&k.to_string()) {
                            out.push(k.to_string());
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(p: &Path, content: &str) {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn detects_python_with_langchain() {
        let d = make_repo();
        write(
            &d.path().join("pyproject.toml"),
            r#"[tool.poetry.dependencies]
langchain = "^0.3"
openai = "^1.0"
"#,
        );
        let det = detect(d.path());
        assert!(det
            .languages
            .iter()
            .any(|l| matches!(l, Language::Python | Language::Mixed)));
        assert!(det.frameworks.contains(&"langchain".to_string()));
        assert!(det.frameworks.contains(&"openai".to_string()));
    }

    #[test]
    fn detects_typescript_with_ai_sdk() {
        let d = make_repo();
        write(
            &d.path().join("package.json"),
            r#"{
  "dependencies": { "ai": "^4.0", "@anthropic-ai/sdk": "^0.40" }
}"#,
        );
        write(&d.path().join("tsconfig.json"), "{}");
        let det = detect(d.path());
        assert!(det.languages.contains(&Language::TypeScript));
        assert!(det.frameworks.contains(&"ai".to_string()));
        assert!(det.frameworks.contains(&"@anthropic-ai/sdk".to_string()));
    }

    #[test]
    fn detects_rust_workspace() {
        let d = make_repo();
        write(&d.path().join("Cargo.toml"), "[workspace]\nmembers = []\n");
        let det = detect(d.path());
        assert!(det.languages.contains(&Language::Rust));
    }

    #[test]
    fn detects_mixed_repo() {
        let d = make_repo();
        write(&d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n");
        write(&d.path().join("package.json"), "{}");
        let det = detect(d.path());
        assert_eq!(det.languages[0], Language::Mixed);
    }

    #[test]
    fn empty_dir_is_unknown() {
        let d = make_repo();
        let det = detect(d.path());
        assert_eq!(det.languages, vec![Language::Unknown]);
    }
}
