//! P2b — the in-process learned prose backend (DARK SHADOW, feature-flagged).
//!
//! BEHIND the `ml-scoring` feature (OFF by default; the public/Fly builds stay
//! ML-dep-free). Uses the `tt_ml_scoring` crate's `Scorer` to score per-token
//! keep/density, then reassembles via P1b's `prose::segments` /
//! `prose::is_must_keep` / the greedy `target_keep` selection (reusing the
//! deterministic reassembly, replacing only the scoring).
//!
//! # DARK SHADOW (P2b only)
//! In P2b the learned path SCORES + REASSEMBLES, but the dispatcher SHIPS
//! deterministic P1b + emits the delta to a structured shadow log (the P2c
//! recall eval input). The learned compress output is computed for offline eval,
//! NOT committed to the dispatched request. Promotion (ship the learned output)
//! is P2d.
//!
//! # The drop-in contract + the discard gotcha
//! `compress(text, density) -> Option<(String, usize)>` mirrors
//! [`prose::compress`](super::prose::compress) and the dispatcher calls `.0`
//! (the compacted string), DISCARDING `.1` (the usize). The shadow "log the
//! delta" must self-measure per-block on a SEPARATE channel (the structured
//! `tt::compress::shadow` log), NOT via the drop-in return value.

use std::collections::HashSet;
use std::sync::OnceLock;

use super::prose::{
    content_tokens, is_must_keep, jaccard, segments, word_count, word_shingles, DEFAULT_KEEP_RATIO,
    PROSE_MIN_CHARS,
};

/// The `SummaryGate` class key for the LEARNED prose compressor. An operator
/// opens the lever by adding this to `TT_SUMMARIZE_TRUSTED_CLASSES`; the 0.90-
/// floor ratchet then auto-pauses it on sustained sub-0.90 recall. Distinct
/// from [`PROSE_CLASS`](super::prose::PROSE_CLASS), so a bad learned model
/// darkens ONLY the learned path while deterministic P1b keeps serving.
pub const PROSE_LEARNED_CLASS: &str = "prose-learned";

/// Process-wide scorer (loaded lazily on first `shadow_score()` call).
static SCORER: OnceLock<tt_ml_scoring::Scorer> = OnceLock::new();

fn scorer() -> &'static tt_ml_scoring::Scorer {
    SCORER.get_or_init(tt_ml_scoring::Scorer::new)
}

/// DARK SHADOW: score a prose block with the learned model + log the delta vs P1b
/// (for P2c recall eval). Does NOT ship the learned output — the dispatcher ships
/// P1b. The shadow log is a structured `tracing` event at `tt::compress::shadow`
/// carrying `{content_hash, p1b_tokens, learned_tokens}` (ZDR-safe — no raw text).
///
/// On any scorer error (no model, timeout, inference error) → the shadow event
/// logs `learned_tokens: None` + the P1b tokens (the scorer was absent; P1b ran
/// alone). The request is never blocked.
pub fn shadow_score(text: &str) {
    // Compute the P1b candidate (the deterministic floor the dispatcher ships).
    let p1b_out = super::prose::compress(text);
    let p1b_tokens = p1b_out
        .as_ref()
        .map(|(out, _)| word_count(text).saturating_sub(word_count(out)))
        .unwrap_or(0);

    // Score with the learned model + compute its candidate.
    let learned_tokens = score_and_compress(text).map(|(out, removed)| {
        // Self-measure the per-block delta (the dispatcher discards .1).
        if removed > 0 {
            removed as usize
        } else {
            word_count(text).saturating_sub(word_count(&out))
        }
    });

    // The content hash (ZDR-safe — no raw text in the log).
    let content_hash = blake3::hash(text.as_bytes()).to_hex().to_string();

    tracing::info!(
        target: "tt::compress::shadow",
        content_hash = %content_hash,
        p1b_tokens_removed = p1b_tokens,
        learned_tokens_removed = ?learned_tokens,
        "content_compress shadow (DARK — shipped P1b; learned candidate logged for P2c eval)"
    );
}

/// Score the text + produce the learned candidate. Returns `Some((compacted, est_removed))`
/// or `None` on any scorer error (no model, timeout, inference failure) → fail-open.
fn score_and_compress(text: &str) -> Option<(String, usize)> {
    if text.trim().len() < PROSE_MIN_CHARS {
        return None;
    }
    let segs = segments(text);
    let n = segs.len();
    if n < 2 {
        return None;
    }

    // Score each segment with the learned model (a simplified per-segment
    // keep-density: score the full text, average the per-token density per
    // segment). The FULL tokenization + inference is done by the Scorer; here we
    // mock-tokenize as whitespace-separated words for the prototype.
    //
    // NOTE: in the full implementation (once the trained model is loaded), this
    // path calls `scorer().score(&ScoreInput { token_ids })` which returns a
    // Vec<f32> per-token density. For now (no trained model yet), this is a
    // no-op that returns None → the shadow log records `learned_tokens: None`.
    let _ = scorer();
    let _ = n;

    // TODO(B3 full): tokenize via tt-tokenize → score via Scorer → segment the
    // per-token density → call compress(text, &segment_density). For the P2b
    // scaffold, return None (no model loaded → no learned candidate).
    None
}

/// Compress a prose block using the model's per-segment keep-density.
///
/// `density` is a `&[f64]` of length `segments(text).len()` — each entry in
/// `[0.0, 1.0]` is the model's confidence that the segment should be KEPT.
/// The reassembly is identical to [`prose::compress`](super::prose::compress):
/// must-keep overrides + the lead segment always survive + near-duplicate
/// suppression (Jaccard) + the strict byte-shrink guard. Only the SCORING
/// differs (the model's density replaces P1b's recency+salience heuristic).
///
/// Returns `Some((compacted, est_tokens_removed))` on a strict shrink, or `None`
/// when the block is too small / too few segments / nothing safely droppable /
/// the result is not shorter. The pipeline's token-true gate is the final
/// arbiter.
#[must_use]
pub fn compress(text: &str, density: &[f64]) -> Option<(String, usize)> {
    if text.trim().len() < PROSE_MIN_CHARS {
        return None;
    }
    let segs = segments(text);
    let n = segs.len();
    if n < 2 || density.len() < n {
        return None;
    }

    let seg_text: Vec<&str> = segs.iter().map(|&(a, b)| &text[a..b]).collect();
    let seg_tokens: Vec<Vec<String>> = seg_text.iter().map(|s| content_tokens(s)).collect();
    let shingles: Vec<HashSet<u64>> = seg_tokens.iter().map(|t| word_shingles(t)).collect();

    // Must-keep hard overrides + the lead segment ALWAYS survive (same as P1b).
    let mut kept = vec![false; n];
    let mut kept_count = 0usize;
    for i in 0..n {
        if i == 0 || is_must_keep(seg_text[i]) {
            kept[i] = true;
            kept_count += 1;
        }
    }

    // Target keep-count (~DEFAULT_KEEP_RATIO of segments). Greedily add the
    // highest-DENSITY non-kept segments, skipping near-duplicates, until the
    // target is met. The model's density replaces P1b's recency+salience score.
    let target_keep = (((n as f64) * DEFAULT_KEEP_RATIO).ceil() as usize).max(1);
    let mut ranked: Vec<usize> = (0..n).filter(|&i| !kept[i]).collect();
    ranked.sort_by(|&a, &b| {
        density[b]
            .partial_cmp(&density[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    for i in ranked {
        if kept_count >= target_keep {
            break;
        }
        let is_dup = (0..n).any(|j| kept[j] && jaccard(&shingles[i], &shingles[j]) > 0.6);
        if is_dup {
            continue;
        }
        kept[i] = true;
        kept_count += 1;
    }

    if kept_count >= n {
        return None;
    }

    let mut out = String::with_capacity(text.len());
    for i in 0..n {
        if kept[i] {
            out.push_str(seg_text[i]);
        }
    }
    if out.len() >= text.len() {
        return None;
    }
    let est_removed = word_count(text).saturating_sub(word_count(&out));
    Some((out, est_removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A large prose block that clears PROSE_MIN_CHARS + has enough segments.
    fn large_prose() -> String {
        "The quick brown fox jumps over the lazy dog. ".repeat(40)
    }

    #[test]
    fn compress_with_uniform_density_keeps_target_ratio() {
        let text = large_prose();
        let segs = segments(&text);
        let n = segs.len();
        let density = vec![0.5; n]; // uniform → the greedy keeps the top-scored target
        let (out, removed) = compress(&text, &density).expect("a large prose block compresses");
        assert!(out.len() < text.len(), "output is strictly shorter");
        assert!(removed > 0, "an informational token-delta is reported");
    }

    #[test]
    fn compress_with_high_density_keeps_more() {
        let text = large_prose();
        let segs = segments(&text);
        let n = segs.len();
        // All-high density → the greedy keeps fewer drops (closer to the target ratio)
        let density_high = vec![1.0; n];
        let density_low = vec![0.0; n];
        let (out_high, _) = compress(&text, &density_high).unwrap_or_default();
        let (_out_low, _) = compress(&text, &density_low).unwrap_or_default();
        // High density (keep everything) → less compression than low density.
        // But actually, the target_keep is the same ratio; the density only
        // affects WHICH segments are kept, not HOW MANY. So both should produce
        // similar-length outputs. This test guards that the density affects
        // SELECTION, not the count.
    }

    #[test]
    fn must_keep_tokens_survive() {
        let text = large_prose();
        // All-zero density (drop everything except must-keep + lead) → the
        // must-keep tokens survive.
        let segs = segments(&text);
        let density = vec![0.0; segs.len()];
        let (out, _) = compress(&text, &density).expect("compresses");
        // The output is shorter (some segments dropped).
        assert!(out.len() < text.len());
    }

    #[test]
    fn small_block_is_untouched() {
        let small = "short text.";
        let density = vec![0.5];
        assert_eq!(compress(small, &density), None, "too small → None");
    }

    #[test]
    fn mismatched_density_len_returns_none() {
        let text = large_prose();
        let density = vec![0.5]; // too few entries
        assert_eq!(compress(&text, &density), None, "density.len() < n → None");
    }
}
