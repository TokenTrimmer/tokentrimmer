//! Lexical signatures for the L2 verify gate (research Phase 2.2).
//!
//! The L2 false-positive gate needs a *zero-cost, deterministic* agreement
//! check between the incoming query text and the text that produced a cached
//! entry — WITHOUT storing prompt text (the privacy invariant of migration
//! 0002: `cache_entries` holds vectors + responses, never source prompts).
//!
//! The answer is a one-way 64-bit **SimHash** over 3-token shingles of the
//! canonicalized L2 context text ([`crate::l2_context_text`]):
//!
//! - Tokens are lowercased maximal alphanumeric runs.
//! - Shingles are 3 consecutive tokens joined by a single space; texts with
//!   fewer than 3 tokens contribute one shingle of all tokens joined.
//! - Each shingle is hashed with **SHA-256** (truncated to its first 8 bytes,
//!   read big-endian as a `u64`). SHA-256 is specified byte-for-byte, so the
//!   persisted signature is stable across Rust releases and architectures —
//!   `std::collections::hash_map::DefaultHasher` is NOT (its algorithm is
//!   explicitly unspecified) and must never be persisted.
//! - The classic SimHash fold: for each of the 64 bit positions, sum +1/−1
//!   across shingle hashes and keep the sign.
//!
//! Agreement between two signatures is `1 − hamming/64` — near-identical
//! texts share most shingles and land within a few bits; unrelated texts land
//! ~32 bits apart (agreement ≈ 0.5). No `rand` anywhere, by design.

use sha2::{Digest, Sha256};

/// Default minimum agreement for an ambiguous-band L2 hit to be served.
///
/// Texts that are paraphrases / near-duplicates of each other agree well above
/// this; topically-shifted texts (the false-positive shape the gate exists to
/// stop) land near the ~0.5 random baseline.
pub const DEFAULT_LEXICAL_MIN_AGREEMENT: f32 = 0.75;

/// Lowercased maximal alphanumeric runs of `text`.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// First 8 bytes of `SHA-256(shingle)`, big-endian. Build-stable by
/// construction (SHA-256 is fully specified); never swap this for
/// `DefaultHasher`, whose output may change between Rust releases.
fn shingle_hash(shingle: &str) -> u64 {
    let digest = Sha256::digest(shingle.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 yields >= 8 bytes"))
}

/// 64-bit SimHash over 3-token shingles of the canonicalized L2 context text.
///
/// Returned as `i64` (bit-cast) to fit a Postgres `BIGINT` column
/// (`cache_entries.lexical_sig`, migration 0018). The signature is one-way:
/// the source text cannot be recovered from it, so persisting it never leaks
/// prompt content. Empty / non-alphanumeric text hashes to `0`.
#[must_use]
pub fn lexical_sig(text: &str) -> i64 {
    let toks = tokens(text);
    if toks.is_empty() {
        return 0;
    }
    let mut counts = [0i32; 64];
    let mut fold = |h: u64| {
        for (bit, count) in counts.iter_mut().enumerate() {
            if (h >> bit) & 1 == 1 {
                *count += 1;
            } else {
                *count -= 1;
            }
        }
    };
    if toks.len() < 3 {
        fold(shingle_hash(&toks.join(" ")));
    } else {
        for window in toks.windows(3) {
            fold(shingle_hash(&window.join(" ")));
        }
    }
    let mut sig = 0u64;
    for (bit, count) in counts.iter().enumerate() {
        if *count > 0 {
            sig |= 1 << bit;
        }
    }
    sig as i64
}

/// Agreement in `[0, 1]` between two signatures: `1 − hamming(a, b)/64`.
/// Identical signatures agree at `1.0`; bitwise complements at `0.0`;
/// unrelated texts land near `0.5` (random 64-bit hashes differ in ~32 bits).
#[must_use]
pub fn lexical_agreement(a: i64, b: i64) -> f32 {
    let hamming = (a ^ b).count_ones();
    1.0 - hamming as f32 / 64.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE persisted-hash stability contract: the signature for a fixed input
    /// is pinned to a golden value. If this test ever fails, the hash function
    /// drifted — which would silently invalidate every persisted
    /// `cache_entries.lexical_sig` row. Do NOT update the golden value without
    /// a migration story for existing rows.
    #[test]
    fn lexical_sig_is_deterministic_and_build_stable() {
        let text = "[system] You are a helpful assistant.\n[user] what is the capital of France";
        let sig = lexical_sig(text);
        assert_eq!(sig, lexical_sig(text), "same input → same signature");
        assert_eq!(
            sig, -7887712269103857164_i64,
            "golden signature drifted — persisted lexical_sig rows would be invalidated"
        );
        // Empty / non-alphanumeric input is the zero signature.
        assert_eq!(lexical_sig(""), 0);
        assert_eq!(lexical_sig("  …—! "), 0);
        // Short texts (< 3 tokens) still produce a stable non-zero signature.
        assert_eq!(lexical_sig("hello"), lexical_sig("HELLO"));
        assert_ne!(lexical_sig("hello"), 0);
    }

    #[test]
    fn lexical_agreement_bounds() {
        let sig = lexical_sig("some fixed text for the agreement bounds test");
        assert_eq!(lexical_agreement(sig, sig), 1.0, "identical sigs agree 1.0");
        assert_eq!(
            lexical_agreement(sig, !sig),
            0.0,
            "bitwise-complement sigs agree 0.0"
        );
        for (a, b) in [
            (
                lexical_sig("alpha beta gamma"),
                lexical_sig("delta epsilon"),
            ),
            (0, i64::MAX),
            (i64::MIN, i64::MAX),
        ] {
            let agreement = lexical_agreement(a, b);
            assert!(
                (0.0..=1.0).contains(&agreement),
                "agreement must stay in [0,1]; got {agreement}"
            );
        }
    }

    /// The gate's separating power: a light paraphrase of the same question
    /// agrees ≥ 0.75 (servable), while a topic shift — the silent-wrong-answer
    /// shape — agrees < 0.75 (rejected).
    #[test]
    fn lexical_sig_separates_paraphrase_from_topic_shift() {
        let original = "[user] how do i configure the retry policy for the payments api client \
                        in the production environment";
        let paraphrase = "[user] how do i configure the retry policy for the payments api client \
                          in the staging environment";
        let topic_shift = "[user] please summarize the quarterly marketing report and list the \
                           three biggest growth opportunities";

        let a = lexical_sig(original);
        let b = lexical_sig(paraphrase);
        let c = lexical_sig(topic_shift);

        let near = lexical_agreement(a, b);
        assert!(
            near >= DEFAULT_LEXICAL_MIN_AGREEMENT,
            "near-identical texts must agree >= {DEFAULT_LEXICAL_MIN_AGREEMENT}; got {near}"
        );
        let far = lexical_agreement(a, c);
        assert!(
            far < DEFAULT_LEXICAL_MIN_AGREEMENT,
            "unrelated texts must agree < {DEFAULT_LEXICAL_MIN_AGREEMENT}; got {far}"
        );
    }
}
