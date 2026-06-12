//! Volatility-class TTL for L2 semantic-cache inserts (research Phase 2.2).
//!
//! Some queries go stale fast: "what's the latest release of X", "today's
//! headlines", "current price of Y". A semantic-cache answer to one of those is
//! correct *now* and silently wrong tomorrow — long before the base 24h/7d/30d
//! tier TTL expires it. This module classifies the canonicalized L2 context
//! text into a coarse volatility class and SHORTENS the L2 TTL for volatile
//! entries.
//!
//! # Conservative by construction
//!
//! - **Shorten-only.** The combinator's ceiling is the base TTL itself; a
//!   volatile classification can never extend a TTL, and the floor
//!   (`min(floor_secs, base)`) keeps a shortened TTL from collapsing to zero.
//! - **Misclassification is safe in both directions.** False-Stable keeps the
//!   base TTL (today's behavior); false-Volatile only shortens a TTL (an
//!   earlier re-dispatch, never a wrong answer).
//! - **Explicit per-request TTL overrides always win.** A caller who set
//!   `tt_extras.cache.ttl_secs` said exactly what they want.
//! - **Deterministic.** Whole-word keyword matching + the SAME literal
//!   ISO-timestamp detector the cache classifier pass uses
//!   ([`crate::passes::cache_classifier`]) — no model calls, no RNG.
//! - **L2-scoped.** L1 exact-match TTLs are untouched (an exact replay of a
//!   volatile prompt within the L1 window is the same bytes either way; the
//!   near-miss-paraphrase risk this lane addresses is an L2 phenomenon).
//!
//! OFF BY DEFAULT: wired only when `L2Config.volatility_ttl` is `Some`
//! (`TT_L2_VOLATILITY_TTL=1`).

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::state::L2VolatilityTtl;

/// Coarse volatility class of an L2 query text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolatilityClass {
    /// No time-sensitive / realtime / version-ish markers — base TTL applies.
    Stable,
    /// The answer plausibly changes on a news/market/release cadence — the
    /// shortened TTL applies.
    Volatile,
}

/// Single-word volatile markers, matched whole-word and case-insensitively.
/// Three families (research brief): time-sensitive, realtime-data, and
/// code-version-ish. A version keyword alone marks the text volatile, which
/// subsumes the "semver literal adjacent to a version-ish keyword" rule — a
/// bare semver with no version-ish context deliberately does NOT fire (it is
/// routinely a stable identifier in prose).
const VOLATILE_KEYWORDS: &[&str] = &[
    // time-sensitive
    "today",
    "tonight",
    "yesterday",
    "tomorrow",
    "latest",
    "current",
    "currently",
    "breaking",
    "news",
    "headlines",
    "recent",
    "recently",
    // realtime data
    "price",
    "prices",
    "stock",
    "forecast",
    "weather",
    "score",
    "scores",
    "standings",
    // code-version-ish
    "version",
    "release",
    "changelog",
    "deprecated",
];

/// Multi-word volatile phrases, matched with word boundaries (a plain
/// `contains` would let "b**right now**here" fire).
fn volatile_phrase_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(right now|this week|this month|this year|exchange rate)\b")
            .expect("volatile-phrase regex is valid")
    })
}

/// Deterministic, conservative volatility classification of the canonicalized
/// L2 context text. Volatile iff ANY of:
/// - a time-sensitive / realtime-data / code-version-ish keyword
///   ([`VOLATILE_KEYWORDS`], whole-word, case-insensitive) or phrase
///   (`right now`, `this week/month/year`, `exchange rate`),
/// - a literal ISO timestamp anywhere in the text (the exact detector the
///   cache classifier pass uses).
///
/// Everything else is `Stable`. False-Stable keeps the base TTL;
/// false-Volatile only shortens one — both safe.
#[must_use]
pub fn classify_volatility(query_text: &str) -> VolatilityClass {
    static KEYWORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    let keywords = KEYWORDS.get_or_init(|| VOLATILE_KEYWORDS.iter().copied().collect());

    let lower = query_text.to_lowercase();
    let has_keyword = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| keywords.contains(t));
    if has_keyword
        || volatile_phrase_regex().is_match(query_text)
        || crate::passes::cache_classifier::literal_iso_timestamp_regex().is_match(query_text)
    {
        VolatilityClass::Volatile
    } else {
        VolatilityClass::Stable
    }
}

/// Pure TTL combinator for the L2 insert path.
///
/// - An explicit per-request override present (`tt_extras.cache.ttl_secs`) →
///   `base_secs` unchanged (the override already won inside
///   `effective_ttl_secs`; volatility never second-guesses it).
/// - `cfg` `None` (feature off) or the text classifies `Stable` → `base_secs`.
/// - `Volatile` → `clamp((base * multiplier) as u64, min(floor_secs, base),
///   base)`. The multiplier is defensively clamped to `[0, 1]` (shorten-only;
///   the floor keeps a degenerate 0 multiplier from zeroing the TTL).
#[must_use]
pub fn l2_ttl_with_volatility(
    base_secs: u64,
    explicit_override: bool,
    query_text: &str,
    cfg: Option<&L2VolatilityTtl>,
) -> u64 {
    let Some(cfg) = cfg else {
        return base_secs;
    };
    if explicit_override {
        return base_secs;
    }
    if classify_volatility(query_text) == VolatilityClass::Stable {
        return base_secs;
    }
    let multiplier = cfg.volatile_multiplier.clamp(0.0, 1.0);
    let shortened = (base_secs as f64 * multiplier) as u64;
    let floor = cfg.floor_secs.min(base_secs);
    shortened.clamp(floor, base_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(multiplier: f64, floor: u64) -> L2VolatilityTtl {
        L2VolatilityTtl {
            volatile_multiplier: multiplier,
            floor_secs: floor,
        }
    }

    /// Table-driven classification: each volatile family fires; neutral
    /// prompts stay Stable; an ISO timestamp fires; phrase matching respects
    /// word boundaries.
    #[test]
    fn classify_volatility_table() {
        let volatile = [
            "[user] what are today's headlines",
            "[user] what is the LATEST version of tokio",
            "[user] current price of bitcoin",
            "[user] weather forecast for berlin",
            "[user] what changed in release 1.42.0 of the sdk",
            "[user] is this api deprecated",
            "[user] euro to usd exchange rate",
            "[user] what is happening right now in the markets",
            "[user] best movies released this year",
            "[system] knowledge as of 2026-06-11T09:30:00Z\n[user] summarize",
            "[user] premier league standings",
        ];
        for text in volatile {
            assert_eq!(
                classify_volatility(text),
                VolatilityClass::Volatile,
                "must classify Volatile: {text}"
            );
        }

        let stable = [
            "[user] explain the borrow checker in rust",
            "[user] write a haiku about the ocean",
            "[system] you are a helpful assistant\n[user] how do i reverse a linked list",
            // Whole-word matching: substrings of keywords must not fire.
            "[user] the newsworthy concurrent recentralization of scoreboards",
            // Phrase boundaries: "bright nowhere" contains "right now" as a
            // raw substring but not as words.
            "[user] the bright nowhere of the open sea",
            // A bare semver with no version-ish keyword stays stable.
            "[user] the constant is 3.14159 and the ratio is 1.5",
            // A bare date (no time component) does not fire — same
            // false-positive rationale as the cache classifier lint.
            "[user] the treaty was signed on 1994-01-01",
        ];
        for text in stable {
            assert_eq!(
                classify_volatility(text),
                VolatilityClass::Stable,
                "must classify Stable: {text}"
            );
        }
    }

    /// Volatile TTLs shorten by the multiplier, floor at min(floor, base),
    /// never exceed the base, and the multiplier clamps to (0, 1] behavior.
    #[test]
    fn volatile_ttl_shortened_within_floor_and_ceiling() {
        let volatile = "[user] what are today's headlines";
        let base = 86_400;

        // Plain shortening: 86400 * 0.25 = 21600.
        assert_eq!(
            l2_ttl_with_volatility(base, false, volatile, Some(&cfg(0.25, 300))),
            21_600
        );
        // Floor respected: a tiny multiplier can't drop below floor_secs.
        assert_eq!(
            l2_ttl_with_volatility(base, false, volatile, Some(&cfg(0.000_001, 300))),
            300
        );
        // Floor itself is bounded by base: floor > base never EXTENDS.
        assert_eq!(
            l2_ttl_with_volatility(100, false, volatile, Some(&cfg(0.25, 300))),
            100,
            "floor must clamp to the base, never extend past it"
        );
        // Multiplier clamps: > 1.0 behaves as 1.0 (no extension)…
        assert_eq!(
            l2_ttl_with_volatility(base, false, volatile, Some(&cfg(5.0, 300))),
            base
        );
        // …and <= 0.0 collapses to the floor, never to zero.
        assert_eq!(
            l2_ttl_with_volatility(base, false, volatile, Some(&cfg(0.0, 300))),
            300
        );

        // Stable text and feature-off both keep the base exactly.
        let stable = "[user] explain the borrow checker in rust";
        assert_eq!(
            l2_ttl_with_volatility(base, false, stable, Some(&cfg(0.25, 300))),
            base
        );
        assert_eq!(l2_ttl_with_volatility(base, false, volatile, None), base);
    }

    /// An explicit per-request TTL override wins over volatility shortening.
    #[test]
    fn explicit_request_ttl_override_wins_over_volatility() {
        let volatile = "[user] what are today's headlines";
        assert_eq!(
            l2_ttl_with_volatility(7_200, true, volatile, Some(&cfg(0.25, 300))),
            7_200,
            "an explicit tt_extras TTL override must never be shortened"
        );
    }
}
