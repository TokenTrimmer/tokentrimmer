//! `tt advise` — scan a repo for model usage and ask a tool-grounded model for
//! cost/routing recommendations. Read-only. Reuses the V5b-2 tool-calling loop.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context as _;
use regex::Regex;

use crate::chat::{tools, Conversation, Ledger};
use crate::context::ResolvedContext;
use crate::ui;

const ADVISOR_MODEL: &str = "gpt-4o-mini";

const ADVISOR_SYSTEM: &str = "You are a TokenTrimmer cost-optimization advisor. \
Use the provided tools (preview_cost, find_route_for, inspect_diff, batch_savings) to \
ground EVERY recommendation in real numbers — never invent prices, call the tools. Be \
concrete and brief: list specific routing/model changes with their dollar impact, name \
cheaper equivalents, and flag risky or wasteful prompt patterns. When you have \
request-log aggregates grouped by tag, call batch_savings to flag latency-insensitive \
traffic (tags like background/offline/nightly/bulk) that could move to the provider's \
Batch API at ~50% off — surface the projected dollar savings. End with the single \
highest-impact change.";

/// File extensions worth scanning for model usage.
const SCAN_EXTS: &[&str] = &[
    "py", "js", "ts", "tsx", "jsx", "rs", "go", "rb", "java", "kt", "php", "cs",
];
/// Directories never scanned.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "vendor",
    ".next",
];
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_FILES: usize = 5000;

/// One model id found in the codebase.
pub struct ModelUsage {
    pub id: String,
    pub count: usize,
    pub example_file: String,
}

/// Dot-segment suffixes that mark a match as a filename/hostname, not a model id.
const FILE_HOST_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "rs", "go", "rb", "java", "kt", "php", "cs",
    "json", "yaml", "yml", "toml", "md", "txt", "lock", "html", "css", "scss", "sh", "com", "net",
    "org", "io", "dev", "example", "local",
];
/// Common second-segments that mean a `<prefix>-<word>` match is not a model id.
const NON_MODEL_WORDS: &[&str] = &[
    "config", "helper", "utils", "util", "client", "service", "handler", "manager", "loader",
    "wrapper", "factory", "provider", "adapter", "key", "keys", "token", "tokens", "secret", "api",
];

fn model_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(gpt-[\w.\-]+|claude-[\w.\-]+|gemini-[\w.\-]+|o[1-9](?:-(?:mini|preview|pro|high|low|nano))?|mistral-[\w.\-]+|llama-?[0-9][\w.\-]*|deepseek-[\w.\-]+|grok-[\w.\-]+|qwen[\w.\-]*|command-r[\w.\-]*|text-embedding-[\w.\-]+)\b",
        )
        .expect("valid model regex")
    })
}

/// Reject regex matches that are actually filenames, hostnames, non-model words,
/// or non-existent o-series numbers — the regex prefixes over-match by design.
fn looks_like_model_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    if lower.contains('/') {
        return false; // path / URL fragment
    }
    if let Some((_, ext)) = lower.rsplit_once('.') {
        if FILE_HOST_EXTS.contains(&ext) {
            return false; // foo.ts, creds.json, host.com, …
        }
    }
    // o-series: only the real reasoning ids (o1 / o3 / o4), not o2-sensor etc.
    if lower.starts_with('o') && lower.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit()) {
        let base: String = lower
            .chars()
            .take_while(|c| *c == 'o' || c.is_ascii_digit())
            .collect();
        return matches!(base.as_str(), "o1" | "o3" | "o4");
    }
    // reject obvious non-model second segments (claude-config, gpt-helper, …)
    if let Some((_, rest)) = lower.split_once('-') {
        let second = rest.split(['-', '.']).next().unwrap_or(rest);
        if NON_MODEL_WORDS.contains(&second) {
            return false;
        }
    }
    true
}

/// Extract known model-id mentions from `text` (de-duped, first-seen order).
#[must_use]
pub fn scan_text_for_models(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in model_re().find_iter(text) {
        let id = m.as_str().to_string();
        if !looks_like_model_id(&id) {
            continue;
        }
        if seen.insert(id.to_ascii_lowercase()) {
            out.push(id);
        }
    }
    out
}

/// A compact brief: detected models + the optional `--describe`, asking for
/// tool-grounded recommendations.
#[must_use]
pub fn build_context_message(detected: &[ModelUsage], describe: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("Review this project's LLM usage and recommend cost/routing optimizations.\n\n");
    if detected.is_empty() {
        s.push_str("No model usage was detected by scanning the code.\n");
    } else {
        s.push_str("Models referenced in the codebase:\n");
        for m in detected {
            s.push_str(&format!(
                "- {} (in {} file(s), e.g. {})\n",
                m.id, m.count, m.example_file
            ));
        }
    }
    if let Some(d) = describe {
        s.push_str(&format!("\nWhat the app does: {d}\n"));
    }
    s.push_str(
        "\nFor each suggestion, call preview_cost / find_route_for to ground the numbers, \
         then give the single highest-impact change.",
    );
    s
}

/// Scan `root` for model-id usage across source files, skipping vendor dirs and
/// over-large files. Returns ids aggregated by file count, most-used first.
#[must_use]
pub fn detect_models(root: &Path) -> Vec<ModelUsage> {
    let mut agg: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut files = 0usize;
    let walk = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        // Never prune the root itself (it may be named like a skip dir, e.g.
        // running inside a "build" dir); only prune skip-named subdirectories.
        e.depth() == 0
            || !e
                .file_name()
                .to_str()
                .is_some_and(|n| SKIP_DIRS.contains(&n))
    });
    for entry in walk.flatten() {
        if files >= MAX_FILES {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !SCAN_EXTS.contains(&ext) {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        files += 1;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        for id in scan_text_for_models(&text) {
            let e = agg.entry(id).or_insert((0, rel.clone()));
            e.0 += 1;
        }
    }
    let mut out: Vec<ModelUsage> = agg
        .into_iter()
        .map(|(id, (count, example_file))| ModelUsage {
            id,
            count,
            example_file,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.id.cmp(&b.id)));
    out
}

/// `tt advise` entry point: scan the repo, then run one tool-grounded turn.
///
/// With `server_loop`, the turn is driven through the server-side agent loop
/// (`POST /v1/agent/runs`) instead of the client-side tool-calling loop: the
/// four advisor tools (`find_route_for`, `preview_cost`, `inspect_diff`,
/// `batch_savings`) are exactly the gateway's read-only tools, so the gateway
/// runs the whole model->tool->model loop SERVER-SIDE — exercising TT's own
/// levers (mid-loop down-routing, judge-gated summarize) on its showcase
/// command. OFF by default (opt-in; no behavior change).
pub async fn run(
    path: Option<String>,
    describe: Option<String>,
    model: Option<String>,
    server_loop: bool,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base.clone(), key.clone());

    let root = path.unwrap_or_else(|| ".".to_string());
    let detected = detect_models(Path::new(&root));
    if detected.is_empty() {
        ui::note(
            "no model usage detected in the code — advising from --describe / general guidance",
        );
    } else {
        ui::note(&format!("scanned: {} model(s) referenced", detected.len()));
    }

    let mut conv = Conversation::new(
        model.unwrap_or_else(|| ADVISOR_MODEL.to_string()),
        Some(ADVISOR_SYSTEM.to_string()),
    );
    conv.push_user(build_context_message(&detected, describe.as_deref()));

    if server_loop {
        ui::heading("TokenTrimmer advisor · server loop");
        // The advisor's four tools ARE the gateway's read-only tools, so the
        // gateway runs the entire tool loop server-side; `DeclineExecutor` is
        // never actually asked to run anything. Cost is gateway-attributed and
        // printed honestly by `print_outcome` (no fabricated savings %).
        let outcome = client
            .agent()
            .model(&conv.model)
            .interactive()
            .messages(conv.wire_messages())
            .tools(crate::agent::gateway_tool_defs())
            .run(&crate::agent::DeclineExecutor)
            .await
            .map_err(|e| anyhow::anyhow!("agent run failed: {e}"))?;
        crate::agent::print_outcome(&outcome);
    } else {
        let reg = tools::build_advisor_registry();
        let mut ledger = Ledger::default();
        ui::heading("TokenTrimmer advisor");
        tools::run_tool_turn(&client, &mut conv, &reg, &mut ledger, true).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_extracts_model_ids() {
        let t = r#"client.chat("gpt-4o-mini"); model = "claude-3-5-sonnet"; use o3; o1;
                   pick "mistral-large-latest"; llama-3.3-70b; "deepseek-r1"; grok-2; "qwen-2.5-72b""#;
        let ids = scan_text_for_models(t);
        for want in [
            "gpt-4o-mini",
            "claude-3-5-sonnet",
            "o3",
            "o1",
            "mistral-large-latest",
            "llama-3.3-70b",
            "deepseek-r1",
            "grok-2",
            "qwen-2.5-72b",
        ] {
            assert!(
                ids.iter().any(|s| s.eq_ignore_ascii_case(want)),
                "missing {want}: {ids:?}"
            );
        }
        assert!(scan_text_for_models("no models here").is_empty());
        assert_eq!(scan_text_for_models("gpt-4o gpt-4o").len(), 1); // de-duped
    }

    #[test]
    fn scan_rejects_false_positives() {
        // filenames, hostnames, non-model words, o2/o5 must NOT be detected
        for c in [
            "import claude-helper.ts",
            "the o2-sensor reading",
            "load gemini-credentials.json",
            "https://api.gpt-4o.example.com/v1",
            "claude-config",
            "var llamaindex = 1",
        ] {
            assert!(
                scan_text_for_models(c).is_empty(),
                "{c:?} → {:?}",
                scan_text_for_models(c)
            );
        }
    }

    #[test]
    fn detect_models_walks_and_skips_vendor_dirs() {
        let dir = std::env::temp_dir().join(format!("tt-advise-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/foo")).unwrap();
        std::fs::write(dir.join("src/a.py"), "m = \"gpt-4o\"\n").unwrap();
        std::fs::write(
            dir.join("src/b.rs"),
            "let m = \"gpt-4o\"; // and claude-3-5-sonnet\n",
        )
        .unwrap();
        std::fs::write(dir.join("node_modules/foo/x.js"), "\"gpt-4o\"\n").unwrap(); // skipped
        std::fs::write(dir.join("README.md"), "uses gpt-4o\n").unwrap(); // wrong ext, skipped

        let found = detect_models(&dir);
        let ids: Vec<&str> = found.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"gpt-4o"), "{ids:?}");
        assert!(ids
            .iter()
            .any(|s| s.eq_ignore_ascii_case("claude-3-5-sonnet")));
        // gpt-4o is only in src/a.py + src/b.rs (node_modules + README skipped) → 2 files
        let gpt = found.iter().find(|m| m.id == "gpt-4o").unwrap();
        assert_eq!(gpt.count, 2, "node_modules + README must be skipped");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn context_message_lists_detected_and_describe() {
        let det = vec![ModelUsage {
            id: "gpt-4o".into(),
            count: 3,
            example_file: "src/llm.py".into(),
        }];
        let msg = build_context_message(&det, Some("a support chatbot"));
        assert!(msg.contains("gpt-4o"));
        assert!(msg.contains("3 file(s)"));
        assert!(msg.contains("src/llm.py"));
        assert!(msg.contains("a support chatbot"));
        let empty = build_context_message(&[], None);
        assert!(empty.contains("No model usage was detected"));
    }
}
