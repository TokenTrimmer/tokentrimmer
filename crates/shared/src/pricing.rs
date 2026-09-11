//! Pricing tables per model. Values are a **manually-curated snapshot** taken
//! from provider pricing pages; they are NOT refreshed automatically.
//! `effective_at` records when each rate took effect and lets us replay
//! historical telemetry against the correct rate. To refresh rates, edit
//! `data/pricing.toml` and append new entries — see `scripts/refresh-pricing.sh`
//! for the manual workflow. See also `docs/02-provider-adapter-guide.md`.
//!
//! Rates live in a versioned data file (`data/pricing.toml`), embedded at build
//! time and parsed once into a [`PricingCatalog`]. Provider adapters delegate
//! to [`catalog`] instead of hardcoding rate tables, so a price refresh is a
//! data edit — decoupled from a Rust release. The catalog keeps a per-model
//! price *history*, enabling [`PricingCatalog::at`] to price historical
//! telemetry against the rate that was in effect at request time.

use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct ModelPricing {
    /// USD per 1M input tokens.
    pub input_per_million: f64,
    /// USD per 1M output tokens.
    pub output_per_million: f64,
    /// USD per 1M cached input tokens (Anthropic 10%, OpenAI 10%, Gemini 10%).
    pub cached_input_per_million: Option<f64>,
    /// USD per 1M cache-creation (cache-write) input tokens. Anthropic charges
    /// ~1.25× the base input rate for tokens written to the prompt cache.
    /// `None` for providers with no documented write premium (cost path unchanged).
    pub cache_write_per_million: Option<f64>,
    /// USD per 1M batch (async) input tokens. Providers with a batch tier
    /// (OpenAI / Anthropic / Gemini) bill async requests at ~50% of standard
    /// input. `None` for providers with no batch tier.
    pub batch_input_per_million: Option<f64>,
    /// USD per 1M batch (async) output tokens (~50% of standard output).
    /// `None` for providers with no batch tier.
    pub batch_output_per_million: Option<f64>,
    /// USD per 1M input tokens under OpenAI's **Flex** service tier
    /// (`service_tier: "flex"`) — a synchronous-but-slower tier billed at Batch
    /// API rates (~50% of standard). `None` for models/providers with no Flex
    /// tier; **presence is the eligibility gate** (only models that carry a Flex
    /// rate may be opted into `service_tier=flex`). See
    /// developers.openai.com/api/docs/guides/flex-processing.
    pub flex_input_per_million: Option<f64>,
    /// USD per 1M output tokens under the Flex service tier (~50% of standard
    /// output). `None` when the model has no Flex tier.
    pub flex_output_per_million: Option<f64>,
    /// Provider minimum prefix length, in tokens, before a `cache_control`
    /// breakpoint actually caches (shorter prefixes silently don't cache).
    /// Anthropic varies this by model (2048–4096); `None` when not documented.
    pub prompt_cache_min_tokens: Option<u32>,
    /// When this pricing took effect (for historical replay).
    pub effective_at: DateTime<Utc>,
    /// C08: when this rate's correctness was last checked against the
    /// provider's published pricing. Defaults to `effective_at` when no
    /// explicit verification date is recorded. A new catalog entry does NOT
    /// update this field for unrelated rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<DateTime<Utc>>,
}

/// Which cache-write TTL tier a prompt-cache write was billed at.
///
/// Anthropic bills cache *writes* at a per-TTL premium over the base input rate:
/// the default 5-minute ephemeral tier is ~1.25× base input, and the opt-in
/// 1-hour tier (`cache_control: {"type": "ephemeral", "ttl": "1h"}`) is ~2×
/// (platform.claude.com/docs/en/build-with-claude/prompt-caching § Economics).
/// [`ModelPricing::cache_write_per_million`] is the 5-minute rate;
/// [`ModelPricing::cache_write_rate_per_million`] resolves either tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheWriteTier {
    /// The default ephemeral TTL — `cache_control` with no `ttl` field. ~1.25×.
    #[default]
    FiveMin,
    /// The opt-in 1-hour TTL — `cache_control` with `"ttl": "1h"`. ~2×.
    OneHour,
}

/// Ratio of the 1-hour cache-write rate to the base input rate (Anthropic's
/// documented 2× one-hour-TTL premium). The 5-minute rate is carried directly
/// in the catalog as `cache_write_per_million` (~1.25× base); the 1-hour rate
/// follows the same documented base-input relationship, so we derive it rather
/// than carrying a second column.
const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;

impl ModelPricing {
    /// USD per 1M cache-write (creation) tokens for the given TTL `tier`.
    ///
    /// - `FiveMin` → the catalog's [`cache_write_per_million`](Self::cache_write_per_million)
    ///   (the 5-minute/1.25× rate Anthropic applies to bare `ephemeral` writes).
    /// - `OneHour` → the documented 2× base-input rate, but **only when a 5-min
    ///   write premium is documented** (i.e. the provider tiers cache writes at
    ///   all). Providers with no write premium return `None` for both tiers so
    ///   the caller falls back to the plain input rate, unchanged.
    ///
    /// Returns `None` when no write premium applies, so callers price the
    /// remaining tokens at `input_per_million`.
    #[must_use]
    pub fn cache_write_rate_per_million(&self, tier: CacheWriteTier) -> Option<f64> {
        match tier {
            CacheWriteTier::FiveMin => self.cache_write_per_million,
            // Only tier up when the provider documents a 5-min write premium;
            // otherwise there is no premium to scale and we leave it absent.
            CacheWriteTier::OneHour => self
                .cache_write_per_million
                .map(|_| self.input_per_million * CACHE_WRITE_1H_MULTIPLIER),
        }
    }

    /// Whether this model is eligible for OpenAI's Flex service tier
    /// (`service_tier: "flex"`). Eligibility is **catalog-driven**: a model is
    /// flex-eligible iff it carries a Flex input rate. OpenAI lists Flex prices
    /// only for supported models (gpt-5.x family); o3 / o4-mini are batch-only
    /// "specialized models" and therefore carry no Flex rate.
    #[must_use]
    pub fn flex_eligible(&self) -> bool {
        self.flex_input_per_million.is_some()
    }

    /// The Flex `(input, output)` per-million rates when this model is
    /// flex-eligible, else `None`. Both are present together for an eligible
    /// row (the catalog carries the pair); a missing output rate falls back to
    /// the standard output rate so a partially-populated row stays priceable.
    #[must_use]
    pub fn flex_rates_per_million(&self) -> Option<(f64, f64)> {
        let input = self.flex_input_per_million?;
        let output = self
            .flex_output_per_million
            .unwrap_or(self.output_per_million);
        Some((input, output))
    }

    /// Whether this model has a Batch API tier in the catalog (presence of a
    /// batch input rate). Drives the advisory batch-eligibility route action.
    #[must_use]
    pub fn batch_eligible(&self) -> bool {
        self.batch_input_per_million.is_some()
    }

    /// `(batch_input_per_million, batch_output_per_million)` when the model is
    /// batch-eligible, else `None`. Both must be present together — a
    /// half-populated row prices nothing (no real rate, no claim).
    #[must_use]
    pub fn batch_rates_per_million(&self) -> Option<(f64, f64)> {
        let input = self.batch_input_per_million?;
        self.batch_output_per_million.map(|output| (input, output))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub capabilities: Vec<Capability>,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Text,
    Vision,
    Audio,
    Tools,
    JsonMode,
    /// The model honors `response_format: json_schema` (structured outputs)
    /// up to the provider's strict-schema semantics — distinct from
    /// [`Capability::JsonMode`], which covers the looser `json_object` shape.
    /// Catalog maintainers list this ONLY where the provider documents
    /// structured-output support for that model; a strict-schema request
    /// routed to a `json_object`-only model is suppressed at selection
    /// (S02) instead of silently downgraded at dispatch.
    StrictJsonSchema,
    Streaming,
    Reasoning,
    PromptCaching,
}

/// Embedded versioned rate catalog. The source of truth for token rates;
/// edited as data (`data/pricing.toml`), not Rust source.
const PRICING_TOML: &str = include_str!("../data/pricing.toml");

/// One row of the catalog as it appears in `pricing.toml`.
#[derive(Debug, Deserialize)]
struct RawEntry {
    provider: String,
    model: String,
    input_per_million: f64,
    output_per_million: f64,
    #[serde(default)]
    cached_input_per_million: Option<f64>,
    #[serde(default)]
    cache_write_per_million: Option<f64>,
    #[serde(default)]
    batch_input_per_million: Option<f64>,
    #[serde(default)]
    batch_output_per_million: Option<f64>,
    #[serde(default)]
    flex_input_per_million: Option<f64>,
    #[serde(default)]
    flex_output_per_million: Option<f64>,
    #[serde(default)]
    prompt_cache_min_tokens: Option<u32>,
    effective_at: DateTime<Utc>,
    /// C08: when this rate's correctness was last checked against the
    /// provider's published pricing. SEPARATE from `effective_at` (when the
    /// rate took effect). An absent `verified_at` defaults to `effective_at`
    /// (the catalog-entry introduction date) — it does NOT inherit freshness
    /// from newer entries. A rate is fresh only when THIS row's `verified_at`
    /// is recent, so a new catalog entry cannot make unrelated unverified
    /// rates appear fresh.
    #[serde(default)]
    verified_at: Option<DateTime<Utc>>,
    /// C08: provenance of this rate — the pricing page (URL) or a stable
    /// label (e.g. "2026-05 catalog snapshot") the rate was taken from.
    /// Absent = no recorded provenance. Resolved per (provider, model) via
    /// [`PricingCatalog::source_for`] on the LATEST entry; `verified_at`
    /// answers "when", this answers "where".
    #[serde(default)]
    source: Option<String>,
    /// C08: the provider marked this model deprecated (off the current
    /// pricing page / announced shutdown). The row stays for historical
    /// replay; resolvable via [`PricingCatalog::is_deprecated`].
    #[serde(default)]
    deprecated: bool,
}

#[derive(Debug, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    entry: Vec<RawEntry>,
}

/// In-memory pricing catalog: per `(provider, model)`, a price history sorted
/// ascending by `effective_at`. Built once from the embedded TOML.
/// Per-model catalog provenance for the LATEST-effective entry (C08):
/// where the rate came from and whether the provider deprecated the model.
#[derive(Debug, Clone)]
struct CatalogProvenance {
    effective_at: DateTime<Utc>,
    /// Where the rate was taken from (pricing page URL or stable snapshot
    /// label). `None` = no recorded provenance — never guessed.
    source: Option<String>,
    /// The provider marked this model deprecated (off the current pricing
    /// page / announced shutdown). Resolved per-entry; un-deprecating is an
    /// explicit newer entry without the flag (no implicit inheritance).
    deprecated: bool,
}

#[derive(Debug)]
pub struct PricingCatalog {
    by_model: HashMap<(String, String), Vec<ModelPricing>>,
    /// C08 provenance: the LATEST-effective entry's source + deprecation
    /// per (provider, model) — latest by EFFECTIVE DATE, not file order (the
    /// price history is sorted; provenance must resolve identically).
    provenance: HashMap<(String, String), CatalogProvenance>,
}

impl PricingCatalog {
    /// Parse a catalog from TOML text. Used by [`catalog`] over the embedded
    /// file; exposed for tests that want to parse a synthetic catalog.
    pub fn parse(toml_text: &str) -> Result<Self, toml::de::Error> {
        let raw: RawCatalog = toml::from_str(toml_text)?;
        let mut by_model: HashMap<(String, String), Vec<ModelPricing>> = HashMap::new();
        let mut provenance: HashMap<(String, String), CatalogProvenance> = HashMap::new();
        for e in raw.entry {
            // C08: keep the provenance of the entry with the LATEST
            // effective_at (mirrors the price-history sort — file order is
            // not authoritative). A newer entry REPLACES the whole provenance
            // (source and deprecation): the current rate's own metadata is
            // what resolves.
            provenance
                .entry((e.provider.clone(), e.model.clone()))
                .and_modify(|slot| {
                    if e.effective_at > slot.effective_at {
                        *slot = CatalogProvenance {
                            effective_at: e.effective_at,
                            source: e.source.clone(),
                            deprecated: e.deprecated,
                        };
                    }
                })
                .or_insert_with(|| CatalogProvenance {
                    effective_at: e.effective_at,
                    source: e.source.clone(),
                    deprecated: e.deprecated,
                });
            by_model
                .entry((e.provider, e.model))
                .or_default()
                .push(ModelPricing {
                    input_per_million: e.input_per_million,
                    output_per_million: e.output_per_million,
                    cached_input_per_million: e.cached_input_per_million,
                    cache_write_per_million: e.cache_write_per_million,
                    batch_input_per_million: e.batch_input_per_million,
                    batch_output_per_million: e.batch_output_per_million,
                    flex_input_per_million: e.flex_input_per_million,
                    flex_output_per_million: e.flex_output_per_million,
                    prompt_cache_min_tokens: e.prompt_cache_min_tokens,
                    effective_at: e.effective_at,
                    verified_at: e.verified_at.or(Some(e.effective_at)),
                });
        }
        // Sort each model's history ascending by effective_at so `latest` is
        // the last element and `at` can scan from newest backward.
        for history in by_model.values_mut() {
            history.sort_by_key(|p| p.effective_at);
        }
        Ok(Self {
            by_model,
            provenance,
        })
    }

    /// The current (most recently effective) rate for `(provider, model)`,
    /// or `None` if the model is not in the catalog.
    pub fn latest(&self, provider: &str, model: &str) -> Option<ModelPricing> {
        self.by_model
            .get(&(provider.to_string(), model.to_string()))?
            .last()
            .cloned()
    }

    /// C08: the recorded provenance (pricing page URL or stable snapshot
    /// label) of the CURRENT rate for `(provider, model)` — the source of the
    /// LATEST entry, NOT inherited from other models or older entries.
    /// `None` when the model is unknown or no source was recorded (honest:
    /// provenance absent, never guessed).
    pub fn source_for(&self, provider: &str, model: &str) -> Option<&str> {
        self.provenance
            .get(&(provider.to_string(), model.to_string()))
            .and_then(|slot| slot.source.as_deref())
    }

    /// C08: whether the provider marked this model deprecated, resolved on
    /// the LATEST-effective entry. Un-deprecating is an explicit newer entry
    /// WITHOUT the flag — no implicit inheritance in either direction.
    /// `false` for unknown models (absence of a catalog row is not evidence
    /// of deprecation, and live local models are catalog-absent by design).
    pub fn is_deprecated(&self, provider: &str, model: &str) -> bool {
        self.provenance
            .get(&(provider.to_string(), model.to_string()))
            .is_some_and(|slot| slot.deprecated)
    }

    /// The rate that was in effect at `at` for `(provider, model)` — the most
    /// recent entry whose `effective_at <= at`. If `at` predates every known
    /// entry, falls back to the earliest entry (best-effort historical replay
    /// rather than reporting no price). `None` only when the model is unknown.
    pub fn at(&self, provider: &str, model: &str, at: DateTime<Utc>) -> Option<ModelPricing> {
        let history = self
            .by_model
            .get(&(provider.to_string(), model.to_string()))?;
        history
            .iter()
            .rev()
            .find(|p| p.effective_at <= at)
            .or_else(|| history.first())
            .cloned()
    }

    /// Every model's current rate for `provider`, as `(model, pricing)` pairs.
    /// Order is unspecified. Used by adapters that build a model→rate map at
    /// construction time (the OpenAI-compatible providers).
    pub fn latest_for_provider(&self, provider: &str) -> Vec<(String, ModelPricing)> {
        self.by_model
            .iter()
            .filter(|((p, _), _)| p == provider)
            .filter_map(|((_, model), history)| history.last().map(|p| (model.clone(), p.clone())))
            .collect()
    }

    /// Every `(provider, model)` pair in the catalog. Order is unspecified.
    /// Pair with [`latest`](Self::latest) / [`at`](Self::at) to materialize a
    /// full rate table (e.g. for the Plan replay engine).
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.by_model.keys().cloned().collect()
    }

    /// Number of distinct `(provider, model)` pairs in the catalog.
    pub fn len(&self) -> usize {
        self.by_model.len()
    }

    /// Whether the catalog has no entries.
    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty()
    }

    /// The provider's prompt-cache minimum prefix length, in tokens, for
    /// `(provider, model)` — `prompt_cache_min_tokens` from the latest catalog
    /// entry. Returns `None` when the model is unknown or carries no documented
    /// minimum.
    ///
    /// Model ids sent on the wire may carry a date suffix (e.g.
    /// `claude-sonnet-4-6-20260101`) while the catalog keys on the bare id, so
    /// after an exact-id miss this falls back to the **longest catalog id that
    /// is a prefix of `model`** before giving up. Shared by the Anthropic
    /// adapter's `cache_control` injection gate and the request-pass
    /// stable-prefix split (`tt-core::passes`), so the two can never disagree
    /// about which prefix length a model needs to cache.
    pub fn prompt_cache_min_tokens(&self, provider: &str, model: &str) -> Option<u32> {
        let lookup = self.latest(provider, model).or_else(|| {
            self.pairs()
                .into_iter()
                .filter(|(p, id)| p == provider && model.starts_with(id.as_str()))
                .max_by_key(|(_, id)| id.len())
                .and_then(|(_, id)| self.latest(provider, &id))
        });
        lookup.and_then(|p| p.prompt_cache_min_tokens)
    }

    /// The newest `effective_at` across every entry in the catalog — i.e. the
    /// date of the most recent manual rate snapshot. Returns `None` only when
    /// the catalog is empty (a build-time error in practice, because the
    /// embedded file is non-empty and the parse is guarded by a unit test).
    ///
    /// Use this as a freshness signal: if the returned date is far in the past
    /// it means pricing.toml has not been updated in a while.
    pub fn catalog_max_effective_at(&self) -> Option<DateTime<Utc>> {
        self.by_model
            .values()
            .filter_map(|history| history.last().map(|p| p.effective_at))
            .max()
    }
}

/// The process-wide pricing catalog, parsed once from the embedded
/// `data/pricing.toml`. Panics at first use only if that bundled file is
/// malformed — which a unit test guards against, so it cannot reach a release.
pub fn catalog() -> &'static PricingCatalog {
    static CATALOG: OnceLock<PricingCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        PricingCatalog::parse(PRICING_TOML).expect("embedded data/pricing.toml must be valid")
    })
}

/// Whether `newest` (the catalog's max `effective_at`) is more than `max_days`
/// before `now`. An empty catalog (`None`) is treated as not stale.
#[must_use]
pub fn is_stale(newest: Option<DateTime<Utc>>, now: DateTime<Utc>, max_days: i64) -> bool {
    match newest {
        Some(d) => (now - d).num_days() > max_days,
        None => false,
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn is_stale_thresholds() {
        use chrono::Duration;
        let now: DateTime<Utc> = "2026-06-05T00:00:00Z".parse().unwrap();
        assert!(!is_stale(None, now, 90)); // empty catalog: not stale
        assert!(!is_stale(Some(now - Duration::days(10)), now, 90));
        assert!(is_stale(Some(now - Duration::days(100)), now, 90));
    }

    #[test]
    fn embedded_catalog_parses_and_is_populated() {
        let c = catalog();
        assert!(!c.is_empty(), "embedded catalog should not be empty");
        // 63 models across 7 paid providers (updated with latest 2026 frontier models:
        // gpt-5.6 family, gpt-4.1 family, claude-5 family, gemini-3.6/3.7,
        // Qwen 3.8/3.7, GLM 5.2/5, Moonshot Kimi K3, DeepSeek V4, Llama 4).
        assert_eq!(
            c.len(),
            63,
            "unexpected catalog size — update if intentional"
        );
    }

    /// C08 source tracking: the verified flagship entries carry BOTH a
    /// machine-readable verification date and a provenance source, and
    /// `source_for` resolves the CURRENT entry's source. The May-2026
    /// baseline snapshot rows stay unsourced (honest absence) — this test
    /// pins the presence of provenance on the rows the header notes
    /// documented as verified.
    #[test]
    fn verified_entries_carry_source_and_resolve() {
        let c = catalog();
        // The 2026-05-31 flagship verification pass (per the catalog header).
        assert_eq!(
            c.source_for("openai", "gpt-5.5"),
            Some("developers.openai.com/api/docs/pricing")
        );
        assert_eq!(
            c.source_for("anthropic", "claude-haiku-4-5"),
            Some("platform.claude.com/docs/en/about-claude/pricing")
        );
        assert_eq!(
            c.source_for("gemini", "gemini-3.1-pro"),
            Some("ai.google.dev/gemini-api/docs/pricing")
        );
        // The 2026-08-16 refresh (OpenRouter list-price mirror convention).
        assert_eq!(
            c.source_for("openrouter", "qwen/qwen3.8-max"),
            Some("openrouter.ai/models")
        );
        // Snapshot rows without recorded provenance resolve honestly to None
        // — never a guessed source.
        assert_eq!(c.source_for("groq", "llama-3.3-70b-versatile"), None);
        // Unknown model: None.
        assert_eq!(c.source_for("openai", "no-such-model"), None);
        // verified_at was populated alongside source (C08 schema #414).
        let pricing = c.latest("openai", "gpt-5.5").expect("gpt-5.5 priced");
        assert_eq!(
            pricing.verified_at.map(|v| v.to_rfc3339()),
            Some("2026-05-31T00:00:00+00:00".to_string())
        );
    }

    /// C08 deprecation flags: the legacy rows the header documents as off the
    /// provider's current pricing page resolve deprecated; current flagships
    /// and unknown models resolve false (absence is not evidence).
    #[test]
    fn deprecated_flags_resolve() {
        let c = catalog();
        // The 2026-05-31 note: "gpt-4o/o3/o4-mini/text-embedding-3-* are off
        // OpenAI's current pricing page (legacy rows retained for replay)".
        assert!(
            c.is_deprecated("openai", "gpt-4o"),
            "gpt-4o is documented legacy"
        );
        assert!(c.is_deprecated("openai", "o3"), "o3 is documented legacy");
        assert!(
            c.is_deprecated("openai", "o4-mini"),
            "o4-mini is documented legacy"
        );
        assert!(
            c.is_deprecated("openai", "text-embedding-3-large"),
            "text-embedding-3-large is documented legacy"
        );
        assert!(
            c.is_deprecated("openai", "text-embedding-3-small"),
            "text-embedding-3-small is documented legacy"
        );
        // Current flagships: not deprecated.
        assert!(!c.is_deprecated("openai", "gpt-5.5"));
        assert!(!c.is_deprecated("anthropic", "claude-sonnet-4-6"));
        // A newer rate refresh on a deprecated model does NOT implicitly
        // un-deprecate (the o3 05-31 price-cut row is flagged too).
        assert!(c.is_deprecated("openai", "o3"));
        // Unknown model: absence of a row is not deprecation evidence.
        assert!(!c.is_deprecated("openai", "no-such-model"));
        assert!(!c.is_deprecated("local", "anything"));
    }

    /// Deprecation resolves on the LATEST-effective entry: a NEWER entry
    /// without the flag explicitly un-deprecates (rate refresh + provider
    /// re-listing), and a NEWER entry WITH the flag deprecates a model whose
    /// older row was fine.
    #[test]
    fn deprecation_resolves_on_latest_effective_entry() {
        let toml_un = r#"
            [[entry]]
            provider = "x"
            model = "m"
            input_per_million = 1.0
            output_per_million = 2.0
            effective_at = "2026-05-01T00:00:00Z"
            deprecated = true

            [[entry]]
            provider = "x"
            model = "m"
            input_per_million = 1.5
            output_per_million = 3.0
            effective_at = "2026-08-01T00:00:00Z"
        "#;
        let c = PricingCatalog::parse(toml_un).unwrap();
        assert!(
            !c.is_deprecated("x", "m"),
            "newer entry without the flag un-deprecates"
        );

        let toml_dep = r#"
            [[entry]]
            provider = "y"
            model = "m"
            input_per_million = 1.0
            output_per_million = 2.0
            effective_at = "2026-05-01T00:00:00Z"

            [[entry]]
            provider = "y"
            model = "m"
            input_per_million = 1.5
            output_per_million = 3.0
            effective_at = "2026-08-01T00:00:00Z"
            deprecated = true
        "#;
        let c2 = PricingCatalog::parse(toml_dep).unwrap();
        assert!(
            c2.is_deprecated("y", "m"),
            "newer entry with the flag deprecates"
        );
    }

    /// `source_for` resolves by the LATEST effective date, not file order: a
    /// model whose rate was refreshed must report the NEW entry's provenance.
    #[test]
    fn source_for_prefers_the_latest_effective_entry() {
        let toml = r#"
            [[entry]]
            provider = "x"
            model = "m"
            input_per_million = 1.0
            output_per_million = 2.0
            effective_at = "2026-05-01T00:00:00Z"
            source = "oldest"

            [[entry]]
            provider = "x"
            model = "m"
            input_per_million = 1.5
            output_per_million = 3.0
            effective_at = "2026-08-01T00:00:00Z"
            source = "newest"
        "#;
        let c = PricingCatalog::parse(toml).unwrap();
        assert_eq!(c.source_for("x", "m"), Some("newest"));
        // A file-ordered-last but DATE-OLDER entry must NOT win — like the
        // price history, resolution is by effective date: the 08-01 entry
        // (file-first) is current, so its "newest" source resolves.
        let toml_reversed = r#"
            [[entry]]
            provider = "y"
            model = "m"
            input_per_million = 1.5
            output_per_million = 3.0
            effective_at = "2026-08-01T00:00:00Z"
            source = "newest"

            [[entry]]
            provider = "y"
            model = "m"
            input_per_million = 1.0
            output_per_million = 2.0
            effective_at = "2026-05-01T00:00:00Z"
            source = ""
        "#;
        let c2 = PricingCatalog::parse(toml_reversed).unwrap();
        assert_eq!(c2.source_for("y", "m"), Some("newest"));
        // An ENTRY ORDER check: newest effective wins even when listed first
        // with an EMPTY source (a re-effecting without provenance marks the
        // CURRENT rate unprovenanced — it does not inherit the old source).
        let toml_unprovenanced = r#"
            [[entry]]
            provider = "z"
            model = "m"
            input_per_million = 1.5
            output_per_million = 3.0
            effective_at = "2026-08-01T00:00:00Z"

            [[entry]]
            provider = "z"
            model = "m"
            input_per_million = 1.0
            output_per_million = 2.0
            effective_at = "2026-05-01T00:00:00Z"
            source = "old"
        "#;
        let c3 = PricingCatalog::parse(toml_unprovenanced).unwrap();
        assert_eq!(c3.source_for("z", "m"), None);
    }

    /// The embedded catalog must carry at least one `effective_at` date and it
    /// must be parseable (which `catalog_max_effective_at` returning `Some`
    /// proves). This test is NOT time-sensitive: we assert presence only, never
    /// a hardcoded "must be within N days of today", so it will never fail
    /// merely because time has passed.
    #[test]
    fn catalog_max_effective_at_is_present() {
        let c = catalog();
        let max_date = c
            .catalog_max_effective_at()
            .expect("non-empty catalog must have a max effective_at");
        // Sanity: the catalog was first created in 2026; the date must be at
        // least 2026-01-01 to confirm we aren't reading a zero/epoch value.
        let floor = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert!(
            max_date >= floor,
            "catalog_max_effective_at = {max_date} is older than expected floor {floor}"
        );
    }

    /// Staleness helper works on a synthetic catalog with known dates.
    #[test]
    fn catalog_max_effective_at_picks_newest() {
        let toml = r#"
            [[entry]]
            provider = "p"
            model = "m1"
            input_per_million = 1.0
            output_per_million = 2.0
            effective_at = "2026-03-01T00:00:00Z"

            [[entry]]
            provider = "p"
            model = "m2"
            input_per_million = 3.0
            output_per_million = 4.0
            effective_at = "2026-05-01T00:00:00Z"
        "#;
        let c = PricingCatalog::parse(toml).expect("valid");
        let max = c.catalog_max_effective_at().expect("present");
        assert_eq!(
            max,
            Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            "should return the newest effective_at across all models"
        );
    }

    /// Empty catalog returns None (not a panic).
    #[test]
    fn catalog_max_effective_at_empty_catalog() {
        let c = PricingCatalog::parse("").expect("empty TOML is valid");
        assert!(c.catalog_max_effective_at().is_none());
    }

    #[test]
    fn latest_returns_known_rates() {
        let c = catalog();
        let p = c.latest("openai", "gpt-4o").expect("gpt-4o present");
        assert_eq!(p.input_per_million, 2.50);
        assert_eq!(p.output_per_million, 10.00);
        assert_eq!(p.cached_input_per_million, Some(1.25));

        // A model whose cached rate is omitted in TOML → None, not 0.0.
        let g = c.latest("groq", "llama-3.1-8b-instant").expect("present");
        assert_eq!(g.cached_input_per_million, None);
    }

    /// Anthropic models must carry a cache_write_per_million at ~1.25× base input.
    /// Non-Anthropic models must have None (no write premium documented).
    #[test]
    fn anthropic_models_have_cache_write_rate() {
        let c = catalog();

        let haiku = c.latest("anthropic", "claude-haiku-4-5").expect("present");
        assert_eq!(
            haiku.cache_write_per_million,
            Some(1.25),
            "haiku write rate = 1.25× base input (1.00)"
        );

        let sonnet = c.latest("anthropic", "claude-sonnet-4-6").expect("present");
        assert_eq!(
            sonnet.cache_write_per_million,
            Some(3.75),
            "sonnet write rate = 1.25× base input (3.00)"
        );

        let opus = c.latest("anthropic", "claude-opus-4-7").expect("present");
        assert_eq!(
            opus.cache_write_per_million,
            Some(6.25),
            "opus write rate = 1.25× base input (5.00)"
        );

        // Non-Anthropic models have no documented write premium.
        let gpt4o = c.latest("openai", "gpt-4o").expect("gpt-4o present");
        assert_eq!(
            gpt4o.cache_write_per_million, None,
            "OpenAI has no cache-write premium"
        );

        let groq_llama = c.latest("groq", "llama-3.1-8b-instant").expect("present");
        assert_eq!(
            groq_llama.cache_write_per_million, None,
            "Groq has no cache-write premium"
        );
    }

    /// The new schema fields (batch rates + prompt-cache minimum) parse and
    /// carry the documented values on the current Anthropic flagships, and are
    /// `None` for providers without a batch tier / documented cache minimum.
    #[test]
    fn batch_and_cache_min_fields_parse() {
        let c = catalog();

        // Anthropic batch = flat 50% of standard; cache minimum is model-specific.
        let opus = c.latest("anthropic", "claude-opus-4-8").expect("present");
        assert_eq!(opus.batch_input_per_million, Some(2.50), "50% of 5.00");
        assert_eq!(opus.batch_output_per_million, Some(12.50), "50% of 25.00");
        assert_eq!(opus.prompt_cache_min_tokens, Some(4096), "Opus 4.x: 4096");

        let sonnet = c.latest("anthropic", "claude-sonnet-4-6").expect("present");
        assert_eq!(sonnet.batch_input_per_million, Some(1.50));
        assert_eq!(sonnet.batch_output_per_million, Some(7.50));
        assert_eq!(
            sonnet.prompt_cache_min_tokens,
            Some(2048),
            "Sonnet 4.6: 2048"
        );

        // OpenAI flagship: batch tier present, 1024-token auto-cache minimum.
        let gpt = c.latest("openai", "gpt-5.5").expect("present");
        assert_eq!(gpt.batch_input_per_million, Some(2.50));
        assert_eq!(gpt.prompt_cache_min_tokens, Some(1024));

        // Gemini: batch present, cache minimum intentionally unset (None).
        let gem = c.latest("gemini", "gemini-3.1-pro").expect("present");
        assert_eq!(gem.batch_input_per_million, Some(1.00));
        assert_eq!(gem.prompt_cache_min_tokens, None);

        // A provider with no batch tier → both batch fields None.
        let groq = c.latest("groq", "llama-3.1-8b-instant").expect("present");
        assert_eq!(groq.batch_input_per_million, None);
        assert_eq!(groq.batch_output_per_million, None);
        assert_eq!(groq.prompt_cache_min_tokens, None);
    }

    /// The batch helpers mirror the flex helpers: eligibility is catalog-driven
    /// (presence of a batch input rate) and `batch_rates_per_million` returns
    /// the real catalog pair — never a fabricated 0.5× of standard.
    #[test]
    fn batch_rates_and_eligibility_from_catalog() {
        let c = catalog();

        let gpt = c.latest("openai", "gpt-5.5").expect("present");
        assert!(gpt.batch_eligible(), "gpt-5.5 carries a batch tier");
        assert_eq!(
            gpt.batch_rates_per_million(),
            Some((2.50, 15.00)),
            "gpt-5.5 batch = 50% of $5/$30"
        );

        let opus = c.latest("anthropic", "claude-opus-4-8").expect("present");
        assert!(opus.batch_eligible());
        assert_eq!(
            opus.batch_rates_per_million(),
            Some((2.50, 12.50)),
            "opus batch = 50% of $5/$25"
        );

        // No batch tier in the catalog → ineligible, no rates, no claim.
        let groq = c.latest("groq", "llama-3.1-8b-instant").expect("present");
        assert!(!groq.batch_eligible(), "Groq has no batch tier");
        assert_eq!(groq.batch_rates_per_million(), None);
    }

    /// `cache_write_rate_per_million` resolves the documented per-TTL premium:
    /// 5-min from the catalog column, 1-hour as 2× base input — but only when a
    /// 5-min write premium is documented (providers without one stay at None so
    /// the caller falls back to the plain input rate).
    #[test]
    fn cache_write_rate_resolves_per_ttl_tier() {
        let c = catalog();

        // Anthropic Sonnet 4.6: base input 3.00, 5-min write 3.75 (=1.25×).
        let sonnet = c.latest("anthropic", "claude-sonnet-4-6").expect("present");
        assert_eq!(
            sonnet.cache_write_rate_per_million(CacheWriteTier::FiveMin),
            Some(3.75),
            "5-min tier = catalog cache_write_per_million (1.25× input)"
        );
        assert_eq!(
            sonnet.cache_write_rate_per_million(CacheWriteTier::OneHour),
            Some(6.00),
            "1-hour tier = 2× base input (3.00)"
        );

        // Opus 4.8: base input 5.00 → 1-hour write 10.00.
        let opus = c.latest("anthropic", "claude-opus-4-8").expect("present");
        assert_eq!(
            opus.cache_write_rate_per_million(CacheWriteTier::FiveMin),
            Some(6.25)
        );
        assert_eq!(
            opus.cache_write_rate_per_million(CacheWriteTier::OneHour),
            Some(10.00),
            "1-hour tier = 2× base input (5.00)"
        );

        // A provider with no documented write premium: both tiers are None so
        // the caller prices these tokens at the plain input rate (unchanged).
        let groq = c.latest("groq", "llama-3.1-8b-instant").expect("present");
        assert_eq!(
            groq.cache_write_rate_per_million(CacheWriteTier::FiveMin),
            None
        );
        assert_eq!(
            groq.cache_write_rate_per_million(CacheWriteTier::OneHour),
            None,
            "no 5-min premium → no 1-hour premium either"
        );
    }

    /// Default tier is the 5-minute tier — Anthropic's default for a bare
    /// `cache_control: {"type": "ephemeral"}` (no `ttl`), which is the only
    /// breakpoint the gateway's Anthropic adapter emits.
    #[test]
    fn cache_write_tier_defaults_to_five_min() {
        assert_eq!(CacheWriteTier::default(), CacheWriteTier::FiveMin);
    }

    /// Flex eligibility is catalog-driven: the supported gpt-5.x models carry a
    /// Flex rate (== batch, 50% of standard) and report eligible; o3 / o4-mini
    /// are batch-only "specialized models" and carry no Flex rate, so they are
    /// NOT flex-eligible. Verified vs developers.openai.com Flex docs/pricing.
    #[test]
    fn flex_rates_and_eligibility_match_openai_docs() {
        let c = catalog();

        // gpt-5.5: standard $5/$30 → flex $2.50/$15 (== batch, 50% off).
        let gpt55 = c.latest("openai", "gpt-5.5").expect("present");
        assert!(gpt55.flex_eligible(), "gpt-5.5 is flex-eligible");
        assert_eq!(gpt55.flex_rates_per_million(), Some((2.50, 15.00)));
        assert_eq!(gpt55.flex_input_per_million, gpt55.batch_input_per_million);
        assert_eq!(
            gpt55.flex_output_per_million,
            gpt55.batch_output_per_million
        );

        // gpt-5.4: standard $2.50/$15 → flex $1.25/$7.50.
        let gpt54 = c.latest("openai", "gpt-5.4").expect("present");
        assert!(gpt54.flex_eligible());
        assert_eq!(gpt54.flex_rates_per_million(), Some((1.25, 7.50)));

        // o3 / o4-mini are batch-only → no flex rate → ineligible.
        let o3 = c.latest("openai", "o3").expect("present");
        assert!(!o3.flex_eligible(), "o3 is batch-only, not flex-eligible");
        assert_eq!(o3.flex_rates_per_million(), None);
        let o4 = c.latest("openai", "o4-mini").expect("present");
        assert!(!o4.flex_eligible());

        // A non-OpenAI model never carries a flex rate.
        let haiku = c.latest("anthropic", "claude-haiku-4-5").expect("present");
        assert!(!haiku.flex_eligible());
    }

    #[test]
    fn unknown_provider_or_model_is_none() {
        let c = catalog();
        assert!(c.latest("openai", "no-such-model").is_none());
        assert!(c.latest("no-such-provider", "gpt-4o").is_none());
    }

    /// `prompt_cache_min_tokens` resolves an exact id, longest-prefix-resolves a
    /// dated Anthropic id to the bare catalog id, and returns None for an
    /// unknown model — parity with the Anthropic adapter's historical lookup.
    #[test]
    fn prompt_cache_min_tokens_prefix_match() {
        let c = catalog();

        // Exact ids resolve to their documented minimums.
        assert_eq!(
            c.prompt_cache_min_tokens("anthropic", "claude-sonnet-4-6"),
            Some(2048)
        );
        assert_eq!(
            c.prompt_cache_min_tokens("anthropic", "claude-opus-4-8"),
            Some(4096)
        );
        assert_eq!(c.prompt_cache_min_tokens("openai", "gpt-5.5"), Some(1024));

        // A dated wire id longest-prefix-resolves to the bare catalog id.
        assert_eq!(
            c.prompt_cache_min_tokens("anthropic", "claude-sonnet-4-6-20260101"),
            Some(2048)
        );

        // Unknown model / provider → None (caller decides the fallback).
        assert_eq!(
            c.prompt_cache_min_tokens("anthropic", "no-such-model"),
            None
        );
        assert_eq!(
            c.prompt_cache_min_tokens("no-such-provider", "gpt-5.5"),
            None
        );

        // A known model with no documented minimum → None, even though the
        // pricing row exists (gemini intentionally carries no minimum).
        assert_eq!(c.prompt_cache_min_tokens("gemini", "gemini-3.1-pro"), None);
    }

    #[test]
    fn at_selects_rate_effective_at_timestamp() {
        // Two-entry history: $1/$2 from 2026-01-01, $3/$4 from 2026-06-01.
        let toml = r#"
            [[entry]]
            provider = "p"
            model = "m"
            input_per_million = 1.0
            output_per_million = 2.0
            effective_at = "2026-01-01T00:00:00Z"

            [[entry]]
            provider = "p"
            model = "m"
            input_per_million = 3.0
            output_per_million = 4.0
            effective_at = "2026-06-01T00:00:00Z"
        "#;
        let c = PricingCatalog::parse(toml).expect("valid");

        // Before either entry → earliest (best-effort).
        let before = c
            .at("p", "m", Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap())
            .unwrap();
        assert_eq!(before.input_per_million, 1.0);

        // Between the two → first (older) rate.
        let mid = c
            .at("p", "m", Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap())
            .unwrap();
        assert_eq!(mid.input_per_million, 1.0);

        // After the second → newest rate.
        let after = c
            .at("p", "m", Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap())
            .unwrap();
        assert_eq!(after.input_per_million, 3.0);

        // `latest` is always the newest regardless of time.
        assert_eq!(c.latest("p", "m").unwrap().input_per_million, 3.0);
    }
}
