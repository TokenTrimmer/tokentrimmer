//! Rule: `inline-data-offload-candidate`
//!
//! Detects a large CSV/TSV, log, or JSON **data dump pasted inline** as a
//! string literal inside an LLM `…create(...)` / `generate_content(...)` call.
//! Inlining bulk data into the prompt re-bills the entire blob as input tokens
//! on *every* call — the dominant waste pattern the code-offload lever targets.
//! The fix is to offload it (retrieval, a reference/id, pre-aggregation, or a
//! code computation) instead of pasting it into the message content.
//!
//! AST-backed, mirroring [`super::cache_anthropic_prompt_cache_missing`]: the
//! tree-sitter harness yields real `call` nodes, the rule keeps only LLM
//! create-calls, and the data-shape + size checks are scoped to each call's
//! own argument span — so a big literal that merely *appears* near a call (a
//! comment, an unrelated assignment) does not trip it.
//!
//! NOTE (deferred, COST-9 gateway half): detecting the same waste *live* —
//! when the bulk data arrives via a variable / templated message at request
//! time — is a runtime diagnostic that belongs in the chat handler, not in this
//! static AST rule. This rule covers only the inline-literal case.

use std::sync::OnceLock;

use regex::Regex;
use tt_inspect_core::ast::{call_sites, is_llm_create_callee};
use tt_inspect_core::parse::parse_cached;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when a large data-shaped string literal is passed inline into an LLM
/// message-content argument.
pub struct InlineDataOffloadCandidateRule;

impl InlineDataOffloadCandidateRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for InlineDataOffloadCandidateRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimum literal size (chars) to be considered a "large" inline blob. ~2 000
/// chars ≈ ~500 input tokens — small enough to catch meaningful dumps, large
/// enough that ordinary prompt strings do not trip it.
const MIN_BLOB_CHARS: usize = 2000;

/// Minimum number of non-empty lines for the tabular / log shape heuristics.
const MIN_DATA_LINES: usize = 5;

/// Char/token proxy used by the sibling cost rules (`prompt-bloated-system`,
/// `prompt-verbose-few-shot`): ~4 chars per token.
const CHARS_PER_TOKEN: usize = 4;

/// Representative flagship input price ($/M tokens) used only for the rough,
/// clearly-labelled per-call cost estimate. The Tier-1 rules are self-contained
/// (no live pricing catalog dependency), so this is a conservative constant in
/// the spirit of the other cost-estimating rules' heuristics — the headline of
/// the finding is the offload action, not a precise dollar figure.
const REPRESENTATIVE_INPUT_USD_PER_MTOK: f64 = 3.00;

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for InlineDataOffloadCandidateRule {
    fn id(&self) -> &'static str {
        "inline-data-offload-candidate"
    }

    fn severity(&self) -> Severity {
        Severity::Medium
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python, Language::Typescript, Language::Javascript]
    }

    fn check(&self, source: &str, language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        // Cheap reject before parsing: the file must contain an LLM create-call
        // token at all (mirrors the sibling rules' substring pre-filter).
        if !source.contains(".create")
            && !source.contains("generate_content")
            && !source.contains("generateContent")
        {
            return vec![];
        }

        let Ok(tree) = parse_cached(source, language) else {
            return vec![];
        };

        let mut findings = Vec::new();
        for call in call_sites(&tree, source) {
            if !is_llm_create_callee(&call.callee) {
                continue;
            }

            // Largest data-shaped literal inside THIS call's argument span.
            let mut best: Option<(usize, &'static str)> = None;
            for literal in extract_string_literals(&call.text) {
                if let Some(kind) = classify_data_blob(&literal) {
                    let len = literal.trim().len();
                    if best.is_none_or(|(blen, _)| len > blen) {
                        best = Some((len, kind));
                    }
                }
            }

            let Some((blob_len, kind)) = best else {
                continue;
            };

            let est_tokens = blob_len.div_ceil(CHARS_PER_TOKEN);
            let est_cost = est_tokens as f64 / 1_000_000.0 * REPRESENTATIVE_INPUT_USD_PER_MTOK;

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: call.line,
                message: format!(
                    "Large inline {kind} data blob (~{est_tokens} tokens, ≈${est_cost:.4} per \
                     call at a representative ${REPRESENTATIVE_INPUT_USD_PER_MTOK:.2}/M input \
                     rate) is passed directly into LLM message content. Inlining bulk data \
                     re-bills the entire blob as input tokens on every call."
                ),
                confidence: 0.70,
                fix_hint: Some(format!(
                    "Offload the bulk {kind} instead of pasting it into the prompt: pass a \
                     reference/id and fetch on demand, pre-aggregate or summarize before \
                     sending, or compute over it in code and send only the result (the \
                     code-offload lever). For data reused across calls, store it once and \
                     reference it rather than re-sending it every request."
                )),
            });
        }

        findings
    }
}

/// Extract the *contents* of every string literal in `block` (Python/JS/TS):
/// triple-quoted (`"""…"""` / `'''…'''`), single/double-quoted, and JS template
/// literals (`` `…` ``). Operates on `char`s and builds owned content strings,
/// so it never slices across a UTF-8 boundary. Quote-prefix sigils (`f`, `r`,
/// `b`) need no special handling — the scanner simply starts at the quote.
fn extract_string_literals(block: &str) -> Vec<String> {
    let chars: Vec<char> = block.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;

    while i < n {
        let c = chars[i];

        // Triple-quoted literal.
        if (c == '"' || c == '\'') && i + 3 <= n && chars[i + 1] == c && chars[i + 2] == c {
            let q = c;
            i += 3;
            let mut content = String::new();
            while i < n {
                if i + 3 <= n && chars[i] == q && chars[i + 1] == q && chars[i + 2] == q {
                    i += 3;
                    break;
                }
                content.push(chars[i]);
                i += 1;
            }
            out.push(content);
            continue;
        }

        // Single-line quoted literal or JS template literal.
        if c == '"' || c == '\'' || c == '`' {
            let q = c;
            i += 1;
            let mut content = String::new();
            while i < n {
                if chars[i] == '\\' && i + 1 < n {
                    // Keep the escape pair verbatim (length-preserving).
                    content.push(chars[i]);
                    content.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if chars[i] == q {
                    i += 1;
                    break;
                }
                content.push(chars[i]);
                i += 1;
            }
            out.push(content);
            continue;
        }

        i += 1;
    }

    out
}

/// Classify `content` as a large inline data dump, returning the kind label
/// (`"JSON"`, `"CSV/TSV"`, `"log"`) or `None`. Conservative by design: prose,
/// code, and small literals return `None`.
fn classify_data_blob(content: &str) -> Option<&'static str> {
    let trimmed = content.trim();
    if trimmed.len() < MIN_BLOB_CHARS {
        return None;
    }

    // JSON: a large body bookended by structural delimiters with many more
    // inside. (A big Python/JS dict/array literal counts too — it is still
    // bulk structured data worth offloading.)
    let structural = trimmed.matches([':', '{', '[']).count();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && (trimmed.ends_with('}') || trimmed.ends_with(']'))
        && structural >= 8
    {
        return Some("JSON");
    }

    let lines: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < MIN_DATA_LINES {
        return None;
    }
    let majority = (lines.len() as f64 * 0.6).ceil() as usize;

    // Tabular: a delimiter that appears a CONSISTENT number of times (>=2) on a
    // majority of lines — the signature of fixed-column rows. Prose, whose
    // punctuation count varies line to line, does not qualify.
    for delim in [',', '\t', '|', ';'] {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(delim).count()).collect();
        let max = counts.iter().copied().max().unwrap_or(0);
        if max < 2 {
            continue;
        }
        for cols in (2..=max).rev() {
            if counts.iter().filter(|&&cnt| cnt == cols).count() >= majority {
                return Some("CSV/TSV");
            }
        }
    }

    // Log: a majority of lines carry a log level or an ISO-ish timestamp.
    let re = log_line_regex();
    let log_lines = lines.iter().filter(|l| re.is_match(l)).count();
    if log_lines >= majority {
        return Some("log");
    }

    None
}

/// Matches a line that looks like a log entry: a level keyword or an ISO-ish
/// `YYYY-MM-DD HH:MM:SS` timestamp.
fn log_line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(\b(error|warn|warning|info|debug|trace|fatal|critical)\b|\d{4}-\d{2}-\d{2}[ t]\d{2}:\d{2}:\d{2})",
        )
        .expect("log-line regex is valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> InlineDataOffloadCandidateRule {
        InlineDataOffloadCandidateRule::new()
    }

    /// A big CSV repeated to exceed the size threshold.
    fn big_csv() -> String {
        let mut s = String::from("id,name,value,ts\n");
        for i in 0..400 {
            s.push_str(&format!("{i},row{i},{},2026-01-01\n", i * 7));
        }
        s
    }

    #[test]
    fn fires_on_inline_csv_in_python_create_call() {
        let src = format!(
            "import openai\nclient = openai.OpenAI()\nresp = client.chat.completions.create(\n  \
             model='gpt-4o', max_tokens=100,\n  messages=[{{'role':'user','content': '''{}'''}}],\n)\n",
            big_csv()
        );
        let f = rule().check(&src, Language::Python, "/src/app.py");
        assert_eq!(f.len(), 1, "inline CSV in a create call should fire once");
        assert!(f[0].message.contains("CSV/TSV"));
    }

    #[test]
    fn does_not_fire_when_blob_is_not_in_an_llm_call() {
        // Same big CSV, but assigned to a variable and never passed to a create
        // call → no LLM create callee → no finding.
        let src = format!("data = '''{}'''\nprint(len(data))\n", big_csv());
        let f = rule().check(&src, Language::Python, "/src/loader.py");
        assert!(f.is_empty(), "data not in an LLM call must not fire");
    }

    #[test]
    fn does_not_fire_on_small_inline_content() {
        let src = "import openai\nresp = openai.chat.completions.create(model='gpt-4o', \
                   messages=[{'role':'user','content':'hello, world'}])\n";
        let f = rule().check(src, Language::Python, "/src/small.py");
        assert!(f.is_empty(), "small content must not fire");
    }

    #[test]
    fn does_not_fire_on_large_prose_prompt() {
        // >MIN_BLOB_CHARS of prose on a few long lines: not tabular (variable
        // comma counts), not JSON, not log.
        let para = "The system should respond helpfully and concisely, weighing tradeoffs \
                    carefully, and it must avoid speculation. ";
        let prose = para.repeat(40); // one long line, well over the threshold
        let src = format!(
            "import openai\nresp = openai.chat.completions.create(model='gpt-4o', \
             messages=[{{'role':'system','content':'{prose}'}}])\n"
        );
        let f = rule().check(&src, Language::Python, "/src/prose.py");
        assert!(f.is_empty(), "large prose prompt must not fire");
    }

    #[test]
    fn fires_on_inline_json_in_typescript_create_call() {
        let mut arr = String::from("[");
        for i in 0..300 {
            arr.push_str(&format!(
                "{{\"id\":{i},\"name\":\"row{i}\",\"v\":{}}},",
                i * 3
            ));
        }
        arr.push_str("{\"id\":999,\"name\":\"last\",\"v\":0}]");
        let src = format!(
            "import OpenAI from 'openai';\nconst client = new OpenAI();\nconst r = await \
             client.chat.completions.create({{ model: 'gpt-4o', messages: [{{ role: 'user', \
             content: `{arr}` }}] }});\n"
        );
        let f = rule().check(&src, Language::Typescript, "/src/app.ts");
        assert_eq!(f.len(), 1, "inline JSON in a TS create call should fire");
        assert!(f[0].message.contains("JSON"));
    }

    #[test]
    fn test_fixture_paths_are_skipped() {
        let src = format!(
            "resp = openai.chat.completions.create(messages=[{{'role':'user','content':'''{}'''}}])\n",
            big_csv()
        );
        assert!(rule()
            .check(&src, Language::Python, "crates/x/tests/fixture.py")
            .is_empty());
    }
}
