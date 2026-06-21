//! COST-1(U): opt-in flagship->mini down-route catalog. Same-provider only;
//! each route is auto_pause-protected + not_reasoning_class-guarded and carries
//! a deterministic reserved name so `tt route catalog disable` removes exactly
//! these routes (no DB marker). Chat flagships only — pure-reasoning o-series
//! models are intentionally excluded from v1.
use crate::store::NewRoute;
use crate::{RouteAction, RouteConditions};

pub const CATALOG_NAME_PREFIX: &str = "catalog:";
const CATALOG_PAUSE_FLOOR: f64 = 0.92;
const CATALOG_PAUSE_MIN_VERDICTS: u32 = 20;
const CATALOG_PRIORITY: u32 = 10; // low — user routes (default 100) win

struct Mapping {
    provider: &'static str,
    sources: &'static [&'static str],
    target: &'static str,
}

/// Curated flagship → cheaper same-family down-route mappings. All ids are
/// verified against BOTH the embedded ModelCatalog (models.toml) AND the
/// PricingCatalog (pricing.toml), and the target must be cheaper — every id is
/// asserted present in both catalogs and the target priced ≤ the source by the
/// `targets_exist_same_provider_and_cheaper` test. Pure-reasoning o-series
/// models (o3, o4-mini) are intentionally excluded — `not_reasoning_class: true`
/// would guard them anyway, but they're not chat flagships in the first place.
///
/// OpenAI — `gpt-5.5` → `gpt-5.4`: the current gpt-5.x flagship down to its
/// cheaper same-family sibling (both full chat models: 200K context, vision,
/// tools, json_mode, prompt_caching; $5/M → $2.50/M input). And `gpt-5.4` →
/// `gpt-5.4-mini` ($2.50/M → $0.75/M input) — the same-family mini, now present
/// in the ModelCatalog, so gpt-5.4 traffic gets a down-route too (one tier per
/// requested model: a gpt-5.5 request still steps only to gpt-5.4, never on to
/// the mini). Also `gpt-4o` → `gpt-4o-mini` for the legacy 4o flagship.
///
/// Anthropic — `claude-opus-4-7` / `claude-opus-4-8` / `claude-sonnet-4-6`
/// flagships → `claude-haiku-4-5`.
///
/// Gemini — `gemini-3.1-pro` → `gemini-3.1-flash-lite` (cheapest same-provider
/// variant) and `gemini-3.5-flash` → `gemini-3.1-flash-lite`
/// ($1.50/M → $0.25/M input).
const MAPPINGS: &[Mapping] = &[
    Mapping {
        provider: "openai",
        sources: &["gpt-5.5"],
        target: "gpt-5.4",
    },
    Mapping {
        provider: "openai",
        sources: &["gpt-5.4"],
        target: "gpt-5.4-mini",
    },
    Mapping {
        provider: "openai",
        sources: &["gpt-4o"],
        target: "gpt-4o-mini",
    },
    Mapping {
        provider: "anthropic",
        sources: &["claude-opus-4-7", "claude-opus-4-8", "claude-sonnet-4-6"],
        target: "claude-haiku-4-5",
    },
    Mapping {
        provider: "gemini",
        sources: &["gemini-3.1-pro", "gemini-3.5-flash"],
        target: "gemini-3.1-flash-lite",
    },
];

#[must_use]
pub fn catalog_route_name(provider: &str, target: &str) -> String {
    format!("{CATALOG_NAME_PREFIX}{provider}->{target}")
}

#[must_use]
pub fn is_catalog_route_name(name: &str) -> bool {
    name.starts_with(CATALOG_NAME_PREFIX)
}

#[must_use]
pub fn catalog_routes() -> Vec<NewRoute> {
    MAPPINGS
        .iter()
        .map(|m| NewRoute {
            name: catalog_route_name(m.provider, m.target),
            priority: CATALOG_PRIORITY,
            enabled: true,
            when: RouteConditions {
                model_in: m.sources.iter().map(|s| (*s).to_string()).collect(),
                not_reasoning_class: true,
                ..Default::default()
            },
            then: RouteAction {
                target_model: Some(m.target.to_string()),
                auto_pause: true,
                pause_floor_pass_rate: Some(CATALOG_PAUSE_FLOOR),
                pause_min_verdicts: Some(CATALOG_PAUSE_MIN_VERDICTS),
                ..Default::default()
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::model_catalog::model_catalog;
    use tt_shared::pricing::catalog as pricing_catalog;

    #[test]
    fn every_catalog_route_is_safe_and_named() {
        for r in catalog_routes() {
            assert!(is_catalog_route_name(&r.name), "{}", r.name);
            assert!(r.then.target_model.is_some());
            assert!(r.then.auto_pause);
            assert!(r.when.not_reasoning_class);
            assert!(!r.when.model_in.is_empty());
            assert_eq!(r.priority, CATALOG_PRIORITY);
        }
    }

    #[test]
    fn targets_exist_same_provider_and_cheaper() {
        let cat = model_catalog();
        let prices = pricing_catalog();
        for m in MAPPINGS {
            let target = cat.model_info(m.provider, m.target).unwrap_or_else(|| {
                panic!(
                    "catalog target {}/{} missing from ModelCatalog",
                    m.provider, m.target
                )
            });
            assert_eq!(target.provider, m.provider);

            let target_price = prices.latest(m.provider, m.target).unwrap_or_else(|| {
                panic!(
                    "catalog target {}/{} missing from PricingCatalog",
                    m.provider, m.target
                )
            });

            for s in m.sources {
                let src = cat.model_info(m.provider, s).unwrap_or_else(|| {
                    panic!(
                        "catalog source {}/{} missing from ModelCatalog",
                        m.provider, s
                    )
                });
                assert_eq!(src.provider, m.provider);

                let src_price = prices.latest(m.provider, s).unwrap_or_else(|| {
                    panic!(
                        "catalog source {}/{} missing from PricingCatalog",
                        m.provider, s
                    )
                });

                assert!(
                    target_price.input_per_million <= src_price.input_per_million,
                    "catalog target {}/{} (${}/M input) must be <= source {}/{} (${}/M input)",
                    m.provider,
                    m.target,
                    target_price.input_per_million,
                    m.provider,
                    s,
                    src_price.input_per_million,
                );
            }
        }
    }

    /// Pin the source → target pairs the catalog must offer, so the gpt-5.x and
    /// gemini-flash families can't silently regress out of coverage. Each pair
    /// is also exercised by `targets_exist_same_provider_and_cheaper` above.
    #[test]
    fn covers_current_flagship_families() {
        // (provider, source, expected target) — every pair MUST be present.
        let expected: &[(&str, &str, &str)] = &[
            // OpenAI: current gpt-5.x flagship, its mini step, + legacy 4o.
            ("openai", "gpt-5.5", "gpt-5.4"),
            ("openai", "gpt-5.4", "gpt-5.4-mini"),
            ("openai", "gpt-4o", "gpt-4o-mini"),
            // Anthropic: every current chat flagship → haiku.
            ("anthropic", "claude-opus-4-8", "claude-haiku-4-5"),
            ("anthropic", "claude-opus-4-7", "claude-haiku-4-5"),
            ("anthropic", "claude-sonnet-4-6", "claude-haiku-4-5"),
            // Gemini: pro + 3.5-flash → flash-lite.
            ("gemini", "gemini-3.1-pro", "gemini-3.1-flash-lite"),
            ("gemini", "gemini-3.5-flash", "gemini-3.1-flash-lite"),
        ];
        for (provider, source, target) in expected {
            let found = MAPPINGS.iter().any(|m| {
                m.provider == *provider && m.target == *target && m.sources.contains(source)
            });
            assert!(
                found,
                "expected catalog mapping {provider}: {source} -> {target} is missing",
            );
        }
    }

    /// Reasoning-only models (o3, o4-mini) must never appear as catalog
    /// sources or targets — the catalog is chat-flagship only.
    #[test]
    fn excludes_reasoning_only_models() {
        for m in MAPPINGS {
            for id in m.sources.iter().chain(std::iter::once(&m.target)) {
                assert!(
                    !matches!(*id, "o3" | "o4-mini"),
                    "reasoning-only model {id} must not be a catalog source/target",
                );
            }
        }
    }
}
