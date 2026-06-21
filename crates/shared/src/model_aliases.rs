//! Asserted-identical model aliases (dated-snapshot → floating-alias), embedded
//! from `data/model_aliases.toml`.
//!
//! Collapsing a dated snapshot onto its floating alias lets the L1/L2 cache
//! share one entry across the two instead of fragmenting — a pure hit-rate win.
//! It is correct ONLY for pairs asserted to produce identical outputs, and a
//! floating alias advances over time, so the map ships **empty by default** and
//! is operator-curated (see the correctness contract in the TOML). The gateway
//! builds an `AliasMapCanonicalizer` from this map for cache-key derivation;
//! with no pairs it is byte-for-byte identical to the no-op key derivation.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

const MODEL_ALIASES_TOML: &str = include_str!("../data/model_aliases.toml");

#[derive(Debug, Deserialize)]
struct AliasFile {
    #[serde(default)]
    alias: Vec<AliasEntry>,
}

#[derive(Debug, Deserialize)]
struct AliasEntry {
    /// The dated snapshot id a request may pin (e.g. `gpt-4o-2024-08-06`).
    snapshot: String,
    /// The floating alias it is asserted-identical to (e.g. `gpt-4o`); must be a
    /// model present in `models.toml`.
    canonical: String,
}

/// The process-wide `dated-snapshot → canonical-alias` map, parsed once from the
/// embedded `data/model_aliases.toml`. Empty when no pairs are configured.
pub fn model_aliases() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let parsed: AliasFile = toml::from_str(MODEL_ALIASES_TOML)
            .expect("embedded data/model_aliases.toml must be valid");
        parsed
            .alias
            .into_iter()
            .map(|e| (e.snapshot, e.canonical))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_catalog::model_catalog;

    #[test]
    fn embedded_aliases_parse() {
        // Default ships empty; this proves the TOML parses and the accessor
        // initializes without panicking.
        let _ = model_aliases();
    }

    #[test]
    fn every_alias_is_valid_and_canonical_is_a_known_model() {
        let cat = model_catalog();
        for (snapshot, canonical) in model_aliases() {
            assert!(
                !snapshot.is_empty() && !canonical.is_empty(),
                "alias ids must be non-empty"
            );
            assert_ne!(
                snapshot, canonical,
                "an alias must not map a model id to itself (a no-op pair is a mistake)"
            );
            // Canonicalizing to a model the catalog doesn't know would mis-key the
            // cache; require the target to be a real model in models.toml.
            assert!(
                cat.all().iter().any(|m| m.id == *canonical),
                "alias canonical target {canonical:?} (from snapshot {snapshot:?}) must be a \
                 model present in models.toml"
            );
        }
    }
}
