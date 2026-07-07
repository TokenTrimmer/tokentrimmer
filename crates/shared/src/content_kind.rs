//! Content-kind classification for content-aware compression (Phase 1, P1a).
//!
//! A single-pass, allocation-light classifier that tags a large text block as
//! one of [`ContentKind`] — JSON, CSV, log, code, diff, or prose. It PROMOTES
//! the offline `classify_data_blob` heuristics
//! (`inspect-rules-tier1::inline_data_offload_candidate`) — JSON structural
//! delimiters, consistent-delimiter tabular rows, log level / ISO-timestamp
//! majority — into a live signal, and ADDS Code / Diff / Prose detection.
//!
//! It lives in `tt-shared` (not `tt-core`) so BOTH the gateway's
//! `content_compress` dispatcher (`tt-core`) and the routing engine's
//! `content_type` condition (`tt-routing`) can reuse ONE classifier without a
//! `tt-core`→`tt-routing` dependency cycle: `tt-core::content_compress`
//! re-exports it, and `tt-shared::capability_check::request_dominant_content_kind`
//! drives the routing match. No LLM, no regex — the hot path runs per large
//! block, so it early-returns on small blocks and scans each line at most a few
//! times.

use serde::{Deserialize, Serialize};

/// The classified kind of a large text block. `None` from [`classify`] means
/// "too small / unclassifiable" (treated as plain text — left untouched).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentKind {
    /// A JSON (or JSON-shaped dict/array) body.
    Json,
    /// Delimiter-separated tabular rows (CSV / TSV / pipe / semicolon).
    Csv,
    /// Log lines carrying a level keyword or an ISO-ish timestamp.
    Log,
    /// Source code (fenced, or a high density of code signals).
    Code,
    /// A unified diff / patch.
    Diff,
    /// Natural-language prose (the fallback for a large, sentence-shaped block).
    Prose,
}

impl ContentKind {
    /// The lowercase wire/telemetry string for this kind (`"json"`, `"code"`, …)
    /// — the value a `RouteConditions.content_type` targets and the flywheel's
    /// `content_compress_kind` column records.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ContentKind::Json => "json",
            ContentKind::Csv => "csv",
            ContentKind::Log => "log",
            ContentKind::Code => "code",
            ContentKind::Diff => "diff",
            ContentKind::Prose => "prose",
        }
    }
}

/// Minimum (trimmed) length in bytes for a block to be worth classifying.
/// Blocks below this return `None` (plain / too small to bother). Deliberately
/// small: the `content_compress` structural backends are content-preserving and
/// ride the token-true gate, so a modest floor is safe, and the classifier's
/// per-kind tests exercise blocks as small as ~120 bytes.
pub const MIN_BLOB_CHARS: usize = 64;

/// Minimum non-empty lines for the line-oriented heuristics (CSV / log / diff).
const MIN_DATA_LINES: usize = 4;

/// Classify a text `block`. Returns the detected [`ContentKind`], or `None` when
/// the block is below [`MIN_BLOB_CHARS`] or does not look like any known kind.
///
/// Detection order (first match wins, most-specific first): Diff → JSON → CSV →
/// Log → Code → Prose. The order matters — a diff OF code must classify as
/// `Diff`, and JSON's structural signature is checked before the code/prose
/// density heuristics.
#[must_use]
pub fn classify(block: &str) -> Option<ContentKind> {
    let trimmed = block.trim();
    if trimmed.len() < MIN_BLOB_CHARS {
        return None;
    }

    let lines: Vec<&str> = trimmed.lines().filter(|l| !l.trim().is_empty()).collect();

    // 1. Diff / patch — the most specific line-prefix signature.
    if is_diff(&lines) {
        return Some(ContentKind::Diff);
    }

    // 2. JSON — a body bookended by structural delimiters with many inside
    //    (promoted verbatim from `classify_data_blob`). A big Python/JS
    //    dict/array literal counts too.
    let structural = trimmed.matches([':', '{', '[']).count();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && (trimmed.ends_with('}') || trimmed.ends_with(']'))
        && structural >= 8
    {
        return Some(ContentKind::Json);
    }

    // The remaining line-oriented heuristics need enough rows to be meaningful.
    if lines.len() >= MIN_DATA_LINES {
        let majority = (lines.len() as f64 * 0.6).ceil() as usize;

        // 3. CSV/TSV — a delimiter that appears a CONSISTENT number of times
        //    (>=2) on a majority of lines (promoted from `classify_data_blob`).
        if is_tabular(&lines, majority) {
            return Some(ContentKind::Csv);
        }

        // 4. Log — a majority of lines carry a level keyword or ISO-ish
        //    timestamp (promoted from `classify_data_blob`, regex-free here).
        let log_lines = lines.iter().filter(|l| is_log_line(l)).count();
        if log_lines >= majority {
            return Some(ContentKind::Log);
        }
    }

    // 5. Code — fenced, or a high density of code signals + balanced braces.
    if is_code(trimmed, &lines) {
        return Some(ContentKind::Code);
    }

    // 6. Prose — the fallback for a large, sentence-shaped block.
    if is_prose(trimmed) {
        return Some(ContentKind::Prose);
    }

    None
}

/// A block is a diff when a majority of its lines start with a diff line-marker
/// (`+`/`-`/`@@ `) AND it carries at least one unambiguous hunk/file header
/// (`@@ `, `--- `/`+++ `, `diff `, `index `) — the header requirement keeps a
/// markdown bullet list (whose lines start with `- `) from reading as a diff.
fn is_diff(lines: &[&str]) -> bool {
    if lines.len() < MIN_DATA_LINES {
        return false;
    }
    let mut marker_lines = 0usize;
    let mut has_header = false;
    for l in lines {
        let starts_marker = l.starts_with('+')
            || l.starts_with('-')
            || l.starts_with("@@")
            || l.starts_with("diff ")
            || l.starts_with("index ");
        if starts_marker {
            marker_lines += 1;
        }
        if l.starts_with("@@ ")
            || l.starts_with("--- ")
            || l.starts_with("+++ ")
            || l.starts_with("diff ")
        {
            has_header = true;
        }
    }
    has_header && marker_lines * 5 >= lines.len() * 3 // >= 60% marker lines
}

/// Consistent-delimiter tabular detection (comma/tab/pipe/semicolon — NOT colon,
/// which appears in timestamps/JSON). Mirrors `classify_data_blob`.
fn is_tabular(lines: &[&str], majority: usize) -> bool {
    for delim in [',', '\t', '|', ';'] {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(delim).count()).collect();
        let max = counts.iter().copied().max().unwrap_or(0);
        if max < 2 {
            continue;
        }
        for cols in (2..=max).rev() {
            if counts.iter().filter(|&&c| c == cols).count() >= majority {
                return true;
            }
        }
    }
    false
}

/// True when a line looks like a log entry: an ISO-ish `YYYY-MM-DD HH:MM:SS`
/// timestamp or a level keyword (case-insensitive). Regex-free / allocation-free.
fn is_log_line(line: &str) -> bool {
    if has_iso_timestamp(line) {
        return true;
    }
    const LEVELS: [&[u8]; 8] = [
        b"error",
        b"warn",
        b"warning",
        b"info",
        b"debug",
        b"trace",
        b"fatal",
        b"critical",
    ];
    let bytes = line.as_bytes();
    LEVELS.iter().any(|lvl| contains_ci(bytes, lvl))
}

/// Scan for a `dddd-dd-dd[ tT]dd:dd:dd` timestamp anywhere in the line without
/// allocating.
fn has_iso_timestamp(line: &str) -> bool {
    let b = line.as_bytes();
    if b.len() < 19 {
        return false;
    }
    let d = |c: u8| c.is_ascii_digit();
    for i in 0..=(b.len() - 19) {
        let w = &b[i..i + 19];
        if d(w[0])
            && d(w[1])
            && d(w[2])
            && d(w[3])
            && w[4] == b'-'
            && d(w[5])
            && d(w[6])
            && w[7] == b'-'
            && d(w[8])
            && d(w[9])
            && (w[10] == b' ' || w[10] == b't' || w[10] == b'T')
            && d(w[11])
            && d(w[12])
            && w[13] == b':'
            && d(w[14])
            && d(w[15])
            && w[16] == b':'
            && d(w[17])
            && d(w[18])
        {
            return true;
        }
    }
    false
}

/// Case-insensitive ASCII substring search that allocates nothing.
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return needle.is_empty();
    }
    'outer: for start in 0..=(hay.len() - needle.len()) {
        for (j, &nb) in needle.iter().enumerate() {
            if !hay[start + j].eq_ignore_ascii_case(&nb) {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

/// Source-code detection: a fenced code block, OR a code keyword signal combined
/// with a high density of statement terminators / balanced braces.
fn is_code(block: &str, lines: &[&str]) -> bool {
    if block.contains("```") {
        return true;
    }
    const KEYWORDS: [&str; 12] = [
        "fn ",
        "def ",
        "function ",
        "class ",
        "import ",
        "#include",
        "=>",
        "public ",
        "private ",
        "const ",
        "return ",
        "func ",
    ];
    let has_keyword = KEYWORDS.iter().any(|k| block.contains(k));
    if !has_keyword {
        return false;
    }
    let n = lines.len().max(1);
    let terminator_lines = lines
        .iter()
        .filter(|l| {
            let t = l.trim_end();
            t.ends_with(';') || t.ends_with('{') || t.ends_with('}') || t.ends_with(':')
        })
        .count();
    let opens = block.matches('{').count();
    let closes = block.matches('}').count();
    let braces_balanced = opens > 0 && opens == closes;
    // Either a solid fraction of lines end like statements, or the braces
    // balance out — both are strong code signals once a keyword is present.
    terminator_lines * 100 >= n * 30 || braces_balanced
}

/// Prose fallback: a large block dominated by alphabetic characters with natural
/// word spacing. Deliberately conservative — a large blob that is neither
/// structured nor sentence-shaped returns `None` rather than a false `Prose`.
fn is_prose(block: &str) -> bool {
    let len = block.len();
    if len < MIN_BLOB_CHARS {
        return false;
    }
    let alpha = block.chars().filter(|c| c.is_alphabetic()).count();
    let spaces = block.bytes().filter(|&b| b == b' ').count();
    // Majority-alphabetic and word-spaced (at least ~1 space per 12 chars).
    alpha * 2 >= len && spaces * 12 >= len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_block() {
        assert_eq!(
            classify(&format!("{{{}}}", "\"k\":1,".repeat(20))),
            Some(ContentKind::Json)
        );
    }

    #[test]
    fn json_array_block() {
        let arr = format!("[{}]", "{\"id\":1,\"v\":\"x\"},".repeat(10));
        assert_eq!(classify(&arr), Some(ContentKind::Json));
    }

    #[test]
    fn csv_block() {
        let mut s = String::from("id,name,value,ts\n");
        for i in 0..40 {
            s.push_str(&format!("{i},row{i},{},2026-01-01\n", i * 7));
        }
        assert_eq!(classify(&s), Some(ContentKind::Csv));
    }

    #[test]
    fn log_block() {
        let l = "2026-07-03 10:00:00 INFO x\n".repeat(12);
        assert_eq!(classify(&l), Some(ContentKind::Log));
    }

    #[test]
    fn log_block_level_only() {
        let l = "ERROR something failed in the handler today\n".repeat(8);
        assert_eq!(classify(&l), Some(ContentKind::Log));
    }

    #[test]
    fn diff_block() {
        let d = "@@ -1 +1 @@\n-old line here\n+new line here\n".repeat(6);
        assert_eq!(classify(&d), Some(ContentKind::Diff));
    }

    #[test]
    fn code_block() {
        let c = "fn a() {\n  let x = 1;\n}\n".repeat(20);
        assert_eq!(classify(&c), Some(ContentKind::Code));
    }

    #[test]
    fn fenced_code_block() {
        let c = format!("Here is code:\n```\n{}\n```\n", "print(x)\n".repeat(20));
        assert_eq!(classify(&c), Some(ContentKind::Code));
    }

    #[test]
    fn prose_block() {
        let p = "The quick brown fox jumps over the lazy dog. ".repeat(40);
        assert_eq!(classify(&p), Some(ContentKind::Prose));
    }

    #[test]
    fn tiny_is_none() {
        assert_eq!(classify("hi"), None);
    }

    #[test]
    fn markdown_bullet_list_is_not_diff() {
        // Lines start with `- ` but there is no diff header → must not be Diff.
        let list = "- first bullet point about a topic here\n\
                    - second bullet point about a topic here\n\
                    - third bullet point about a topic here\n\
                    - fourth bullet point about a topic here\n\
                    - fifth bullet point about a topic here\n";
        assert_ne!(classify(list), Some(ContentKind::Diff));
    }

    #[test]
    fn as_str_round_trips_kinds() {
        for (k, s) in [
            (ContentKind::Json, "json"),
            (ContentKind::Csv, "csv"),
            (ContentKind::Log, "log"),
            (ContentKind::Code, "code"),
            (ContentKind::Diff, "diff"),
            (ContentKind::Prose, "prose"),
        ] {
            assert_eq!(k.as_str(), s);
            // serde uses the same lowercase token.
            assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{s}\""));
        }
    }
}
