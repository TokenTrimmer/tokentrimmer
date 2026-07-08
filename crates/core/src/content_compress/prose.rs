//! P1b — the PROSE extractive compressor backend (LOSSY, judge-gated).
//!
//! TT's headline `compression` pass is "content-lossless trim of *non-prose*"
//! (its own doc-comment) — it moves ~0 billed tokens on a big prose blob. This
//! module closes that gap with a deterministic (NO ML — that is Phase 2)
//! EXTRACTIVE compressor: it segments the prose into sentences, scores each by
//! `recency + keyword-salience − near-duplicate-penalty`, and drops the
//! lowest-scoring segments down to a target keep-ratio, subject to a
//! **must-keep hard-override allowlist** that always retains load-bearing tokens
//! (numbers, hex, ISO dates, paths, URLs, CLI flags, CamelCase/snake_case
//! identifiers, backtick/code spans) regardless of score.
//!
//! # Moat-wrap (reused, NOT reinvented)
//! Extractive compression is LOSSY, so — unlike the P1a JSON/CSV/log structural
//! backend (content-preserving, token-true gate alone) — it commits ONLY behind
//! the same quality gate the lossy `summarize` lever rides
//! ([`crate::passes::agentic_budget::summarize_judge`]):
//!
//! - The dispatcher reads the shared [`SummaryGate`] (the synchronous projection
//!   of the ~2% paired recall-of-baseline judge, [`crate::quality_sample`]) for
//!   the [`PROSE_CLASS`] key. A committable class → compress; an untrusted /
//!   closed / unavailable gate → **fail OPEN to the verbatim bytes** (no
//!   compression, no saving booked), exactly like
//!   [`SummarizeStep::apply`](crate::passes::agentic_budget::summarize_judge::SummarizeStep)
//!   leaves an untrusted class un-summarized.
//! - The production gate is [`RatchetSummaryGate`](crate::passes::agentic_budget::summarize_judge::RatchetSummaryGate):
//!   an operator allowlist opens the `"prose"` class and a windowed pass-rate dip
//!   below the sticky **0.90 floor** (the same `route_autopause` floor the
//!   down-route judge uses) shuts it — sustained sub-0.90 recall auto-pauses
//!   prose compression per lever. The default [`NeverCommitGate`](crate::passes::agentic_budget::summarize_judge::NeverCommitGate)
//!   trusts nothing, so prose stays verbatim until the judge proves the lever.
//! - The compacted result still rides the pipeline's TOKEN-TRUE GATE: a block
//!   that tokenizes larger than the input is discarded → verbatim.
//!
//! # Attribution
//! The must-keep hard-override is TT's Rust reimplementation of the "must-keep"
//! primitive popularized by headroom's `TextCrusher` (headroom is Apache-2.0).
//! The regex allowlist and scoring here are original TT code written from the
//! documented behavior — NONE of headroom's source was copied.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;

/// The [`SummaryGate`](crate::passes::agentic_budget::summarize_judge::SummaryGate)
/// class key for prose compression. An operator opens the lever by adding this
/// to `TT_SUMMARIZE_TRUSTED_CLASSES` (the shared lossy-lever allowlist); the
/// 0.90-floor ratchet then auto-pauses it on sustained sub-0.90 recall. Distinct
/// from the summarize lever's tool-name classes, so the two never collide.
pub const PROSE_CLASS: &str = "prose";

/// Prose blocks below this many (trimmed) bytes are left verbatim — extractive
/// dropping is not worth the lossy risk on a short block. Above the classifier's
/// [`MIN_BLOB_CHARS`](tt_shared::content_kind::MIN_BLOB_CHARS) so a merely
/// classifiable block is not automatically compressed.
pub const PROSE_MIN_CHARS: usize = 600;

/// A block with fewer than this many sentence segments is left verbatim (too
/// few to safely drop any without risking meaning).
const MIN_SEGMENTS: usize = 5;

/// The documented default fraction of segments to KEEP. Dropping ~40% of the
/// lowest-scoring, non-must-keep, non-duplicate segments is a conservative
/// extractive ratio. Must-keep overrides can push the kept fraction higher.
pub const DEFAULT_KEEP_RATIO: f64 = 0.60;

/// A candidate segment whose word-shingle Jaccard overlap with an already-kept
/// segment exceeds this is dropped as a near-duplicate (redundancy suppression).
const DEDUP_SIMILARITY: f64 = 0.6;

/// Minimum content-token length (shorter tokens are ignored for salience).
const MIN_TOKEN_LEN: usize = 3;

/// Salience vs recency mixing weights for the segment score.
const SALIENCE_W: f64 = 0.7;
const RECENCY_W: f64 = 0.3;

/// Compress a prose block extractively. Returns `Some((compacted, est_tokens_removed))`
/// on a strict shrink, or `None` when the block is too small / too few segments
/// / nothing safely droppable / the result is not shorter (the verbatim
/// fail-safe). The caller (the dispatcher) applies the gate BEFORE calling this
/// and the pipeline's token-true gate measures the AUTHORITATIVE token delta;
/// the returned `est_tokens_removed` is an informational word-delta estimate.
#[must_use]
pub fn compress(text: &str) -> Option<(String, usize)> {
    if text.trim().len() < PROSE_MIN_CHARS {
        return None;
    }
    let segs = segments(text);
    let n = segs.len();
    if n < MIN_SEGMENTS {
        return None;
    }

    // Content tokens + shingles per segment (single pass over each segment).
    let seg_text: Vec<&str> = segs.iter().map(|&(a, b)| &text[a..b]).collect();
    let seg_tokens: Vec<Vec<String>> = seg_text.iter().map(|s| content_tokens(s)).collect();

    // Global term frequency (topical importance) over the whole block.
    let mut gf: HashMap<&str, u32> = HashMap::new();
    for toks in &seg_tokens {
        for t in toks {
            *gf.entry(t.as_str()).or_insert(0) += 1;
        }
    }

    // Raw keyword-salience per segment: sum of the global frequency of its UNIQUE
    // content tokens, length-normalized (sqrt) to dampen long-sentence bias.
    let raw_salience: Vec<f64> = seg_tokens
        .iter()
        .map(|toks| {
            let uniq: HashSet<&str> = toks.iter().map(String::as_str).collect();
            let sum: f64 = uniq.iter().map(|t| f64::from(gf[t])).sum();
            let norm = (uniq.len() as f64).max(1.0).sqrt();
            sum / norm
        })
        .collect();

    // Min-max normalize salience to [0,1] so it composes with the recency term.
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &s in &raw_salience {
        lo = lo.min(s);
        hi = hi.max(s);
    }
    let span = (hi - lo).max(f64::EPSILON);
    let score: Vec<f64> = (0..n)
        .map(|i| {
            let salience_norm = (raw_salience[i] - lo) / span;
            let recency = if n > 1 {
                i as f64 / (n - 1) as f64
            } else {
                1.0
            };
            SALIENCE_W * salience_norm + RECENCY_W * recency
        })
        .collect();

    // Word-shingle sets for near-duplicate suppression.
    let shingles: Vec<HashSet<u64>> = seg_tokens.iter().map(|t| word_shingles(t)).collect();

    // Must-keep hard overrides + the lead segment ALWAYS survive.
    let mut kept = vec![false; n];
    let mut kept_count = 0usize;
    for i in 0..n {
        if i == 0 || is_must_keep(seg_text[i]) {
            kept[i] = true;
            kept_count += 1;
        }
    }

    // Target keep-count (~DEFAULT_KEEP_RATIO of segments). Greedily add the
    // highest-scoring non-kept segments, skipping near-duplicates of an
    // already-kept segment, until the target is met.
    let target_keep = (((n as f64) * DEFAULT_KEEP_RATIO).ceil() as usize).max(1);
    let mut ranked: Vec<usize> = (0..n).filter(|&i| !kept[i]).collect();
    // Highest score first; ties broken by earlier position (deterministic).
    ranked.sort_by(|&a, &b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    for i in ranked {
        if kept_count >= target_keep {
            break;
        }
        let is_dup =
            (0..n).any(|j| kept[j] && jaccard(&shingles[i], &shingles[j]) > DEDUP_SIMILARITY);
        if is_dup {
            continue;
        }
        kept[i] = true;
        kept_count += 1;
    }

    // Nothing dropped → no compression (fail-safe, avoids a same-length rewrite).
    if kept_count >= n {
        return None;
    }

    // Reassemble the kept segments in original order.
    let mut out = String::with_capacity(text.len());
    for i in 0..n {
        if kept[i] {
            out.push_str(seg_text[i]);
        }
    }
    // Strict-shrink guard (the pipeline token-true gate is the final arbiter).
    if out.len() >= text.len() {
        return None;
    }
    let est_removed = word_count(text).saturating_sub(word_count(&out));
    Some((out, est_removed))
}

/// Split `text` into sentence-ish segments as byte ranges. A segment ends after
/// a terminal `.`/`!`/`?` followed by whitespace (so decimals like `3.14` do NOT
/// split — the following char is a digit, not whitespace) or after a newline
/// (list items / lines become segments). The trailing run of whitespace is kept
/// with the segment so a subset of segments re-joins as readable text.
pub(crate) fn segments(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        let cut_here = if c == '.' || c == '!' || c == '?' {
            matches!(chars.peek(), Some((_, nc)) if nc.is_whitespace()) || chars.peek().is_none()
        } else {
            c == '\n'
        };
        if cut_here {
            let mut end = idx + c.len_utf8();
            while let Some(&(widx, wc)) = chars.peek() {
                if wc.is_whitespace() {
                    end = widx + wc.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            out.push((start, end));
            start = end;
        }
    }
    if start < text.len() {
        out.push((start, text.len()));
    }
    out
}

/// Lowercased alphanumeric content tokens of a segment, filtering short tokens
/// and stopwords (the salience vocabulary).
pub(crate) fn content_tokens(seg: &str) -> Vec<String> {
    seg.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= MIN_TOKEN_LEN)
        .map(str::to_ascii_lowercase)
        .filter(|w| !is_stopword(w))
        .collect()
}

/// Word 2-gram shingles (hashed) of a segment's content tokens, for the
/// near-duplicate Jaccard test. A single-token segment falls back to its
/// 1-gram; a token-less segment has no shingles (never a duplicate).
pub(crate) fn word_shingles(tokens: &[String]) -> HashSet<u64> {
    use std::hash::{Hash, Hasher};
    let mut set = HashSet::new();
    let hash = |a: &str, b: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut h);
        b.hash(&mut h);
        h.finish()
    };
    match tokens.len() {
        0 => {}
        1 => {
            set.insert(hash(&tokens[0], ""));
        }
        _ => {
            for w in tokens.windows(2) {
                set.insert(hash(&w[0], &w[1]));
            }
        }
    }
    set
}

/// Jaccard similarity of two shingle sets (0 when either is empty).
pub(crate) fn jaccard(a: &HashSet<u64>, b: &HashSet<u64>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    inter as f64 / union as f64
}

pub(crate) fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

/// The must-keep hard-override detector: a segment matching ANY of these ALWAYS
/// survives regardless of score (headroom `TextCrusher` primitive, reimplemented).
/// One compiled alternation, checked with `is_match` — a single pass per segment:
/// - `\d` — any digit (subsumes numbers, hex `0x1f`, ISO dates `2026-07-03`,
///   versions);
/// - `https?://` / `www\.` — URLs;
/// - a slash between word chars — file paths (`src/main.rs`);
/// - a `-x`/`--xyz` after whitespace/start — CLI flags;
/// - a lower→upper transition — CamelCase identifiers (`getUserName`);
/// - an underscore between word chars — snake_case identifiers (`file_path`);
/// - a backtick — inline code / code-fence spans.
pub(crate) fn is_must_keep(seg: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\d|https?://|www\.|`|[\w.-]+/[\w./-]+|(?:^|\s)-{1,2}[A-Za-z]|[a-z][A-Z]|\w_\w")
            .expect("must-keep allowlist regex is valid")
    });
    re.is_match(seg)
}

/// A small English stopword set — ubiquitous function words carry no salience.
fn is_stopword(w: &str) -> bool {
    const STOP: &[&str] = &[
        "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "had", "her", "was",
        "one", "our", "out", "day", "get", "has", "him", "his", "how", "man", "new", "now", "old",
        "see", "two", "way", "who", "boy", "did", "its", "let", "put", "say", "she", "too", "use",
        "that", "this", "with", "from", "they", "will", "would", "there", "their", "what", "about",
        "which", "when", "make", "like", "time", "just", "know", "take", "into", "your", "some",
        "them", "than", "then", "look", "only", "come", "over", "also", "back", "after", "use",
        "how", "our", "well", "even", "want", "because", "these", "give", "most", "been", "were",
        "has", "have", "each", "such", "very", "much", "many", "more", "other", "same", "here",
    ];
    STOP.contains(&w)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block of DISTINCT prose sentences (no digits, so the must-keep override
    /// does not protect everything) with recurring topical vocabulary, so the
    /// keyword-salience scoring has signal. Comfortably over `PROSE_MIN_CHARS`.
    fn distinct_prose() -> String {
        [
            "The gateway routes each request to the cheapest capable model.",
            "Routing decisions weigh model capability against the expected cost.",
            "A cheaper model often answers a simple question just as well.",
            "Quality regressions would erode user trust in the routing layer.",
            "The judge samples a small fraction of traffic to catch regressions.",
            "When quality drops the lever pauses itself automatically.",
            "Caching avoids paying twice for an identical repeated request.",
            "Semantic caching matches requests that mean the same thing.",
            "Compression trims redundant scaffolding before dispatch.",
            "Prose blocks were historically left almost entirely untouched.",
            "Extractive selection keeps the most salient sentences of a block.",
            "The remaining sentences are dropped to reach a target ratio.",
            "Every lossy transform stays behind a strict quality gate.",
            "The gate fails open to the verbatim text whenever unsure.",
        ]
        .join(" ")
    }

    #[test]
    fn large_prose_compresses_to_target_ratio() {
        let text = distinct_prose();
        let (out, est) = compress(&text).expect("a large distinct prose block compresses");
        assert!(out.len() < text.len(), "output is strictly shorter");
        assert!(est > 0, "an informational token-delta is reported");
        // ~60% keep-ratio: fewer sentences than the input, but not gutted.
        let n_in = segments(&text).len();
        let n_out = segments(&out).len();
        assert!(
            n_out < n_in,
            "some segments were dropped ({n_out} of {n_in})"
        );
        assert!(
            n_out >= n_in / 2,
            "the extractive keep is near the target, not over-aggressive ({n_out} of {n_in})"
        );
    }

    #[test]
    fn must_keep_tokens_all_survive_even_when_low_scored() {
        // Filler sentences (high count → the dropped bulk) plus a few SHORT,
        // low-salience sentences each carrying exactly one must-keep token.
        let mut parts: Vec<String> = Vec::new();
        for _ in 0..10 {
            parts.push("Some generic filler sentence about ordinary matters.".to_string());
        }
        // Each of these would score LOW (short, off-topic) but must-keep survives.
        parts.push("Retry limit is 42.".to_string()); // number
        parts.push("Edit the file src/main.rs now.".to_string()); // path
        parts.push("Pass the flag --verbose please.".to_string()); // CLI flag
        parts.push("Call getUserName soon.".to_string()); // CamelCase
        parts.push("Wrap it in `backticks` here.".to_string()); // backtick span
        let text = parts.join(" ");
        let (out, _) = compress(&text).expect("compresses");
        for needle in [
            "42",
            "src/main.rs",
            "--verbose",
            "getUserName",
            "`backticks`",
        ] {
            assert!(
                out.contains(needle),
                "must-keep token {needle:?} must survive: {out}"
            );
        }
        // And the lossy transform still dropped some filler.
        assert!(out.len() < text.len(), "filler was still dropped");
    }

    #[test]
    fn small_prose_block_is_untouched() {
        // Below PROSE_MIN_CHARS and MIN_SEGMENTS → the prose backend leaves it be
        // (even though the classifier would tag it Prose).
        let text = "This is short. Only a couple sentences here. Nothing to drop.";
        assert!(text.len() < PROSE_MIN_CHARS);
        assert_eq!(compress(text), None, "a small prose block is left verbatim");
    }

    #[test]
    fn all_must_keep_yields_verbatim_no_shrink() {
        // Every segment carries a must-keep token (a digit) → nothing droppable →
        // None (the strict-shrink / token-true fail-safe: never a same-length or
        // longer rewrite).
        let mut parts = Vec::new();
        for i in 0..12 {
            parts.push(format!("Sentence number {i} carries a load bearing value."));
        }
        let text = parts.join(" ");
        assert_eq!(
            compress(&text),
            None,
            "an all-must-keep block cannot shrink → verbatim"
        );
    }

    #[test]
    fn near_duplicate_sentences_are_deduped() {
        // Many identical sentences + a few distinct ones: the duplicates collapse
        // (redundancy suppression), so the output is much shorter than the input.
        let mut parts = Vec::new();
        for _ in 0..12 {
            parts.push("The quick brown fox jumps over the lazy dog today.".to_string());
        }
        parts.push("An entirely different observation about migratory birds.".to_string());
        parts.push("Yet another unrelated remark concerning ocean currents.".to_string());
        let text = parts.join(" ");
        let (out, _) = compress(&text).expect("compresses");
        let dup_count = out.matches("quick brown fox").count();
        assert!(
            dup_count <= 2,
            "near-duplicate sentences collapse (kept {dup_count})"
        );
    }

    #[test]
    fn segmentation_does_not_split_decimals() {
        let segs = segments("The value is 3.14 exactly. Then a second sentence follows here.");
        // Two sentences, not split on the decimal point.
        assert_eq!(segs.len(), 2, "decimal '3.14' did not split the sentence");
    }
}
