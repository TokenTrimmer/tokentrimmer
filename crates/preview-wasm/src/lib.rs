//! Browser (wasm) cost-preview projector for the no-signup `/playground`.
//!
//! Paste a prompt + pick a model → token count (real tiktoken via `tt-tokenize`),
//! list-price input cost, the safe same-family down-route suggestion, and the
//! projected savings — computed **entirely client-side**, no API key, no network.
//!
//! Self-contained on purpose: it depends only on `tt-tokenize` (which wasm-
//! compiles) and embeds the same `pricing.toml` the gateway uses (via
//! `include_str!`) + the down-route catalog's curated mappings. It deliberately
//! does NOT depend on `tt-shared`/`tt-routing` (both drag wasm-hostile deps —
//! `uuid` without a wasm RNG, `mio`), so the wasm graph stays minimal + buildable.
//!
//! The pure projection (`project`) is `#[cfg(test)]`-covered natively; the
//! `#[wasm_bindgen]` wrappers are thin JSON adapters.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::wasm_bindgen;

/// The same pricing catalog the gateway embeds — kept in sync at BUILD time by
/// reading the one source of truth. A playground figure can never silently use
/// a rate the gateway doesn't.
const PRICING_TOML: &str = include_str!("../../shared/data/pricing.toml");

/// Curated same-provider down-routes — a verbatim copy of the gateway's
/// `tt_routing::catalog` MAPPINGS (which can't be imported here: `tt-routing`
/// pulls `mio`). A drift test (`downroutes_match_catalog`-style) is impractical
/// across the wasm boundary, so these are documented as a manual mirror.
const DOWNROUTES: &[(&str, &str)] = &[
    ("gpt-5.5", "gpt-5.4"),
    ("gpt-5.4", "gpt-5.4-mini"),
    ("gpt-4o", "gpt-4o-mini"),
    ("claude-opus-4-7", "claude-haiku-4-5"),
    ("claude-opus-4-8", "claude-haiku-4-5"),
    ("claude-sonnet-4-6", "claude-haiku-4-5"),
    ("gemini-3.1-pro", "gemini-3.1-flash-lite"),
    ("gemini-3.5-flash", "gemini-3.1-flash-lite"),
];

#[derive(Deserialize)]
struct PricingFile {
    #[serde(default)]
    entry: Vec<PricingEntry>,
}

#[derive(Deserialize)]
struct PricingEntry {
    provider: String,
    model: String,
    input_per_million: f64,
    /// RFC3339; lexicographic compare picks the latest rate for a (provider,model).
    effective_at: String,
    // (output_per_million in the TOML is ignored — the playground prices only
    // the input tokens of the pasted prompt; output is unknown ahead of the call.)
}

/// Resolved per-model input rate (latest effective_at).
#[derive(Clone, Copy)]
struct Rate {
    input_per_million: f64,
}

struct Catalog {
    /// (provider, model) → latest rate.
    rates: HashMap<(String, String), (Rate, String)>, // value: (rate, effective_at)
    /// model → provider (for tokenizer + lookups when only a model id is known).
    provider_of: HashMap<String, String>,
}

fn catalog() -> &'static Catalog {
    static CAT: OnceLock<Catalog> = OnceLock::new();
    CAT.get_or_init(|| {
        let parsed: PricingFile =
            toml::from_str(PRICING_TOML).expect("embedded pricing.toml parses");
        let mut rates: HashMap<(String, String), (Rate, String)> = HashMap::new();
        let mut provider_of: HashMap<String, String> = HashMap::new();
        for e in parsed.entry {
            let key = (e.provider.clone(), e.model.clone());
            let rate = Rate {
                input_per_million: e.input_per_million,
            };
            // Keep the entry with the latest effective_at (RFC3339 sorts lexically).
            match rates.get(&key) {
                Some((_, prev_eff)) if prev_eff.as_str() >= e.effective_at.as_str() => {}
                _ => {
                    rates.insert(key, (rate, e.effective_at.clone()));
                }
            }
            provider_of.entry(e.model).or_insert(e.provider);
        }
        Catalog { rates, provider_of }
    })
}

/// One model's cost on a given input-token count.
#[derive(Serialize)]
struct ModelCost {
    model: String,
    provider: String,
    input_per_million: f64,
    /// Input-token cost in USD (the deterministic part — output is unknown ahead
    /// of the call, so the headline prices what the prompt costs to *send*).
    input_cost_usd: f64,
}

/// The full projection returned to the page.
#[derive(Serialize)]
struct Projection {
    input_tokens: u32,
    base: ModelCost,
    /// The safe same-family down-route, when one exists for `base.model`.
    suggested: Option<ModelCost>,
    /// `base.input_cost_usd - suggested.input_cost_usd` (>= 0), 0 when no suggestion.
    savings_usd: f64,
    /// `savings_usd / base.input_cost_usd` (0..1), 0 when base cost is 0.
    savings_pct: f64,
}

#[derive(Serialize)]
struct ProjectionError {
    error: String,
}

/// Pure projection (natively unit-tested). `Err` when the model isn't priced.
fn project(prompt: &str, model: &str) -> Result<Projection, String> {
    let cat = catalog();
    let provider = cat
        .provider_of
        .get(model)
        .ok_or_else(|| format!("unknown / unpriced model: {model}"))?
        .clone();
    let base_rate = cat
        .rates
        .get(&(provider.clone(), model.to_string()))
        .map(|(r, _)| *r)
        .ok_or_else(|| format!("no rate for {provider}/{model}"))?;

    let input_tokens = tt_tokenize::estimate_tokens_for_model(&provider, model, prompt);
    let input_cost = f64::from(input_tokens) * base_rate.input_per_million / 1_000_000.0;

    let base = ModelCost {
        model: model.to_string(),
        provider: provider.clone(),
        input_per_million: base_rate.input_per_million,
        input_cost_usd: input_cost,
    };

    // Down-route suggestion: the curated cheaper same-family sibling, if priced.
    let suggested = DOWNROUTES
        .iter()
        .find(|(src, _)| *src == model)
        .and_then(|(_, target)| {
            let tprov = cat.provider_of.get(*target)?;
            let (trate, _) = cat.rates.get(&(tprov.clone(), (*target).to_string()))?;
            let tcost = f64::from(input_tokens) * trate.input_per_million / 1_000_000.0;
            Some(ModelCost {
                model: (*target).to_string(),
                provider: tprov.clone(),
                input_per_million: trate.input_per_million,
                input_cost_usd: tcost,
            })
        });

    let savings_usd = suggested
        .as_ref()
        .map(|s| (base.input_cost_usd - s.input_cost_usd).max(0.0))
        .unwrap_or(0.0);
    let savings_pct = if base.input_cost_usd > 0.0 {
        savings_usd / base.input_cost_usd
    } else {
        0.0
    };

    Ok(Projection {
        input_tokens,
        base,
        suggested,
        savings_usd,
        savings_pct,
    })
}

// ----------------------------------------------------------------------------
// wasm-bindgen JSON adapters
// ----------------------------------------------------------------------------

/// Project the cost of `prompt` on `model`. Returns a JSON string: a
/// `Projection` on success, or `{"error": "..."}` for an unpriced model.
#[wasm_bindgen]
#[must_use]
pub fn preview(prompt: &str, model: &str) -> String {
    match project(prompt, model) {
        Ok(p) => serde_json::to_string(&p).unwrap_or_else(|e| err_json(&e.to_string())),
        Err(e) => err_json(&e),
    }
}

fn err_json(msg: &str) -> String {
    serde_json::to_string(&ProjectionError {
        error: msg.to_string(),
    })
    .unwrap_or_else(|_| "{\"error\":\"serialize\"}".to_string())
}

/// JSON array of the priceable model ids that the playground can offer, sorted,
/// so the page can build its model picker without hardcoding the catalog.
/// Down-routeable flagships (those with a suggestion) come first.
#[wasm_bindgen]
#[must_use]
pub fn models() -> String {
    let cat = catalog();
    let mut ids: Vec<&String> = cat.provider_of.keys().collect();
    ids.sort();
    let flagships: Vec<&str> = DOWNROUTES.iter().map(|(s, _)| *s).collect();
    // Flagships first (the interesting savings demo), then the rest.
    let mut ordered: Vec<String> = Vec::with_capacity(ids.len());
    for f in &flagships {
        if cat.provider_of.contains_key(*f) {
            ordered.push((*f).to_string());
        }
    }
    for id in ids {
        if !flagships.contains(&id.as_str()) {
            ordered.push(id.clone());
        }
    }
    serde_json::to_string(&ordered).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_pricing_parses_and_has_flagships() {
        let cat = catalog();
        assert!(!cat.rates.is_empty(), "pricing.toml must parse to >0 rates");
        // The down-route flagships must all be priced (else the demo can't show savings).
        for (src, target) in DOWNROUTES {
            assert!(
                cat.provider_of.contains_key(*src),
                "flagship {src} must be priced"
            );
            assert!(
                cat.provider_of.contains_key(*target),
                "target {target} must be priced"
            );
        }
    }

    #[test]
    fn project_flagship_shows_positive_savings() {
        // gpt-5.5 → gpt-5.4 is cheaper, so a non-empty prompt must project a saving.
        let p = project(
            "Summarize the quarterly earnings call in three bullet points.",
            "gpt-5.5",
        )
        .expect("gpt-5.5 is priced");
        assert!(p.input_tokens > 0, "a non-empty prompt has tokens");
        assert!(p.base.input_cost_usd > 0.0);
        let s = p.suggested.expect("gpt-5.5 has a down-route");
        assert_eq!(s.model, "gpt-5.4");
        assert!(
            s.input_cost_usd < p.base.input_cost_usd,
            "down-route must be cheaper"
        );
        assert!(p.savings_usd > 0.0 && p.savings_pct > 0.0);
    }

    #[test]
    fn project_unknown_model_errors() {
        assert!(project("hi", "totally-made-up-model").is_err());
    }

    #[test]
    fn no_downroute_model_has_no_suggestion_and_zero_savings() {
        // A priced model with no curated down-route (e.g. a mini) projects cost
        // but no suggestion — never a fabricated saving.
        let p = project("hello world", "gpt-5.4-mini").expect("mini is priced");
        assert!(p.suggested.is_none());
        assert_eq!(p.savings_usd, 0.0);
    }

    #[test]
    fn preview_json_roundtrips_to_a_projection() {
        let json = preview("Write a haiku about latency.", "claude-opus-4-8");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["input_tokens"].as_u64().unwrap() > 0);
        assert_eq!(v["suggested"]["model"], "claude-haiku-4-5");
        assert!(v["savings_usd"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn models_lists_flagships_first() {
        let json = models();
        let list: Vec<String> = serde_json::from_str(&json).unwrap();
        assert!(!list.is_empty());
        // The first entries are down-routeable flagships.
        assert!(DOWNROUTES
            .iter()
            .any(|(s, _)| list.first().map(|f| f == s).unwrap_or(false)));
    }
}
