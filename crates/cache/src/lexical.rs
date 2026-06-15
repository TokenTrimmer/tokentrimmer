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

// ---------------------------------------------------------------------------
// Lexical verify gate — ambiguous-band cross-check (COST-5, default ON)
// ---------------------------------------------------------------------------

/// Default ambiguous-band half-width above the effective L2 threshold. A hit at
/// `similarity >= t_eff + epsilon` is confident (no verify step); a hit in
/// `[t_eff, t_eff + epsilon)` is the AMBIGUOUS BAND, cross-checked lexically.
/// 0.02 matches the gateway's historical verify-gate default.
pub const DEFAULT_VERIFY_EPSILON: f32 = 0.02;

/// Configuration for the lexical verify gate, defaulting to **ON** for the
/// ambiguous band.
///
/// The SimHash/lexical cross-check ([`lexical_agreement`]) catches the
/// meaning-flipped-paraphrase false positive: a `>= t_eff` cosine neighbor whose
/// text disagrees lexically (a topic shift the embedding scored close) is
/// rejected rather than served as Confident. [`LexicalVerifyConfig::default`]
/// turns this on for the ambiguous band at ~zero cost; it fails OPEN on entries
/// with no stored signature (pre-migration-0018 rows), so enabling it can never
/// turn a legacy row into a miss.
#[derive(Debug, Clone, Copy)]
pub struct LexicalVerifyConfig {
    /// Ambiguous-band half-width above the effective threshold. Clamped to
    /// `[0.0, 0.05]`.
    pub epsilon: f32,
    /// Minimum lexical agreement for an ambiguous-band hit to be served.
    /// Clamped to `[0.5, 1.0]`.
    pub min_agreement: f32,
}

impl Default for LexicalVerifyConfig {
    /// The default-ON gate: a 0.02 ambiguous band verified at
    /// [`DEFAULT_LEXICAL_MIN_AGREEMENT`].
    fn default() -> Self {
        Self {
            epsilon: DEFAULT_VERIFY_EPSILON,
            min_agreement: DEFAULT_LEXICAL_MIN_AGREEMENT,
        }
    }
}

impl LexicalVerifyConfig {
    /// Construct with the documented clamps (`epsilon ∈ [0.0, 0.05]`,
    /// `min_agreement ∈ [0.5, 1.0]`).
    #[must_use]
    pub fn new(epsilon: f32, min_agreement: f32) -> Self {
        Self {
            epsilon: epsilon.clamp(0.0, 0.05),
            min_agreement: min_agreement.clamp(0.5, 1.0),
        }
    }
}

/// The lexical verify gate's decision for a candidate L2 hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LexicalVerifyDecision {
    /// `similarity >= t_eff + epsilon` — above the ambiguous band; serve as
    /// today (no cross-check needed).
    Confident,
    /// In the ambiguous band with agreement `>= min_agreement` — serve (carries
    /// the measured agreement for telemetry).
    Verified(f32),
    /// In the ambiguous band but the entry has no signature (pre-0018 row) —
    /// fail OPEN and serve, counted distinctly.
    Unverifiable,
    /// In the ambiguous band with agreement `< min_agreement` — a likely
    /// meaning-flipped paraphrase; treat as a MISS (do not serve).
    Rejected(f32),
}

impl LexicalVerifyDecision {
    /// Whether the candidate hit should be SERVED. Everything except
    /// [`Rejected`](Self::Rejected) serves (fail-open by construction).
    #[must_use]
    pub fn should_serve(self) -> bool {
        !matches!(self, LexicalVerifyDecision::Rejected(_))
    }
}

/// Pure lexical verify decision for an L2 candidate hit (no I/O, no clock, no
/// RNG — fully deterministic).
///
/// A hit at `similarity >= effective_threshold + cfg.epsilon` is
/// [`Confident`](LexicalVerifyDecision::Confident). An in-band hit
/// (`effective_threshold <= similarity < + epsilon`) is cross-checked: the
/// entry's stored one-way signature (`entry_sig`) is compared against the
/// incoming query's signature (`query_sig`) via [`lexical_agreement`]; agreement
/// `>= cfg.min_agreement` is [`Verified`](LexicalVerifyDecision::Verified),
/// below it is [`Rejected`](LexicalVerifyDecision::Rejected). An entry with no
/// signature fails OPEN as [`Unverifiable`](LexicalVerifyDecision::Unverifiable)
/// — the gate must never turn a legacy row into an outage.
#[must_use]
pub fn lexical_verify_decision(
    cfg: &LexicalVerifyConfig,
    similarity: f32,
    effective_threshold: f32,
    entry_sig: Option<i64>,
    query_sig: i64,
) -> LexicalVerifyDecision {
    if similarity >= effective_threshold + cfg.epsilon {
        return LexicalVerifyDecision::Confident;
    }
    let Some(entry_sig) = entry_sig else {
        return LexicalVerifyDecision::Unverifiable;
    };
    let agreement = lexical_agreement(entry_sig, query_sig);
    if agreement >= cfg.min_agreement {
        LexicalVerifyDecision::Verified(agreement)
    } else {
        LexicalVerifyDecision::Rejected(agreement)
    }
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

    // ── Lexical verify gate (default ON for the ambiguous band) ──────────────

    /// The default config turns the gate ON: a 0.02 band verified at the
    /// default minimum agreement.
    #[test]
    fn verify_config_default_is_on() {
        let cfg = LexicalVerifyConfig::default();
        assert!((cfg.epsilon - DEFAULT_VERIFY_EPSILON).abs() < f32::EPSILON);
        assert!((cfg.min_agreement - DEFAULT_LEXICAL_MIN_AGREEMENT).abs() < f32::EPSILON);
    }

    #[test]
    fn verify_config_clamps_out_of_range() {
        let cfg = LexicalVerifyConfig::new(1.0, 0.0);
        assert!(
            (cfg.epsilon - 0.05).abs() < f32::EPSILON,
            "epsilon clamps to 0.05"
        );
        assert!(
            (cfg.min_agreement - 0.5).abs() < f32::EPSILON,
            "min_agreement clamps to 0.5"
        );
    }

    /// Above the ambiguous band, the gate is Confident regardless of signatures
    /// (no cross-check needed) — including a missing signature.
    #[test]
    fn verify_confident_above_band_skips_cross_check() {
        let cfg = LexicalVerifyConfig::default();
        let d = lexical_verify_decision(&cfg, 0.99, 0.92, None, 12345);
        assert_eq!(d, LexicalVerifyDecision::Confident);
        assert!(d.should_serve());
    }

    /// An in-band hit whose text lexically AGREES is Verified (served).
    #[test]
    fn verify_in_band_agreeing_is_verified() {
        let cfg = LexicalVerifyConfig::default();
        let entry = "[user] how do i configure the retry policy for the payments api client";
        let query = "[user] how do i configure the retry policy for the payments api client now";
        let d = lexical_verify_decision(
            &cfg,
            0.93, // inside [0.92, 0.94)
            0.92,
            Some(lexical_sig(entry)),
            lexical_sig(query),
        );
        assert!(
            matches!(d, LexicalVerifyDecision::Verified(_)),
            "near-identical text must agree → Verified: {d:?}"
        );
        assert!(d.should_serve());
    }

    /// An in-band hit whose text DISAGREES (a meaning-flipped/topic-shifted
    /// neighbor the embedding scored close) is Rejected — NOT served. This is
    /// the false positive the default-ON gate exists to stop.
    #[test]
    fn verify_in_band_disagreeing_is_rejected() {
        let cfg = LexicalVerifyConfig::default();
        let entry = "[user] how do i configure the retry policy for the payments api client";
        let topic_shift =
            "[user] summarize the quarterly marketing report and list the growth opportunities";
        let d = lexical_verify_decision(
            &cfg,
            0.93,
            0.92,
            Some(lexical_sig(entry)),
            lexical_sig(topic_shift),
        );
        assert!(
            matches!(d, LexicalVerifyDecision::Rejected(_)),
            "a meaning-flipped neighbor must be Rejected: {d:?}"
        );
        assert!(
            !d.should_serve(),
            "a Rejected ambiguous-band hit must NOT be served"
        );
    }

    /// A pre-0018 entry (no signature) in the ambiguous band fails OPEN —
    /// Unverifiable, served — so enabling the gate never turns a legacy row
    /// into a miss (~zero-cost default-on).
    #[test]
    fn verify_fails_open_on_missing_signature() {
        let cfg = LexicalVerifyConfig::default();
        let d = lexical_verify_decision(&cfg, 0.93, 0.92, None, lexical_sig("anything"));
        assert_eq!(d, LexicalVerifyDecision::Unverifiable);
        assert!(
            d.should_serve(),
            "pre-0018 rows must fail OPEN (served, never an outage)"
        );
    }
}
