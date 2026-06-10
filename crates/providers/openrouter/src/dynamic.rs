//! Dynamic OpenRouter catalogue: parse + cache the live `GET /models` list.
//!
//! OpenRouter exposes `GET /api/v1/models`, which returns the full catalogue
//! (300+ models) with per-model **pricing**. We fetch it (at startup and on a
//! periodic refresh), parse the model list + pricing, and cache it so the
//! gateway can (a) advertise availability for any OpenRouter model and
//! (b) attribute cost/savings from OpenRouter's own returned rates rather than
//! a hardcoded subset.
//!
//! # Verified response shape (live endpoint + docs, 2026-06)
//!
//! ```jsonc
//! {
//!   "data": [
//!     {
//!       "id": "anthropic/claude-sonnet-4-6",
//!       "name": "Anthropic: Claude Sonnet 4.6",
//!       "context_length": 200000,
//!       "pricing": {
//!         "prompt": "0.000003",      // USD PER TOKEN ($3 / 1M)
//!         "completion": "0.000015",  // USD PER TOKEN ($15 / 1M)
//!         "request": "0", "image": "0",
//!         "input_cache_read": "0.0000003",
//!         "input_cache_write": "0.00000375"
//!       },
//!       "top_provider": { "max_completion_tokens": 64000 }
//!     }
//!   ]
//! }
//! ```
//!
//! Pricing values are USD **per token**. The documented type is a string
//! (`BigNumberUnion`), but the live endpoint currently serialises bare numbers,
//! so [`PriceValue`] accepts either. `-1` and empty strings denote "no fixed
//! price" (e.g. dynamic routers) and are treated as absent.

use std::collections::HashMap;

use serde::Deserialize;
use tt_shared::pricing::{Capability, ModelInfo, ModelPricing};

/// The OpenRouter provider id used for the parsed [`ModelInfo::provider`] field.
const PROVIDER_ID: &str = "openrouter";

/// Top-level `GET /models` envelope: `{ "data": [ ... ] }`.
#[derive(Debug, Deserialize)]
pub struct ModelsResponse {
    #[serde(default)]
    pub data: Vec<RawModel>,
}

/// One model object from the catalogue. Only the fields we consume are named;
/// everything else is ignored (the real payload is much larger).
#[derive(Debug, Deserialize)]
pub struct RawModel {
    pub id: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: Option<RawPricing>,
    #[serde(default)]
    pub top_provider: Option<TopProvider>,
}

/// The per-model `top_provider` block — we read the completion-token cap.
#[derive(Debug, Deserialize)]
pub struct TopProvider {
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
}

/// The per-model `pricing` object. All money fields are per-token (see module
/// docs). We currently consume `prompt`, `completion`, and the cache-read rate.
#[derive(Debug, Deserialize)]
pub struct RawPricing {
    #[serde(default)]
    pub prompt: Option<PriceValue>,
    #[serde(default)]
    pub completion: Option<PriceValue>,
    #[serde(default)]
    pub input_cache_read: Option<PriceValue>,
}

/// A price that may arrive as a JSON string (`"0.000003"`, the documented
/// `BigNumberUnion`) or a bare JSON number (`0.000003`, what the live endpoint
/// serialises today). Parsed into a per-token `f64`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PriceValue {
    Str(String),
    Num(f64),
}

impl PriceValue {
    /// Per-token USD value, or `None` when the field is absent/unparseable or a
    /// sentinel (`-1`, empty string) meaning "no fixed price".
    fn per_token(&self) -> Option<f64> {
        let v = match self {
            PriceValue::Num(n) => *n,
            PriceValue::Str(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return None;
                }
                t.parse::<f64>().ok()?
            }
        };
        if v.is_finite() && v >= 0.0 {
            Some(v)
        } else {
            None // negative (-1) sentinel or NaN/inf → treat as absent
        }
    }
}

/// USD per token → USD per 1M tokens (the gateway's pricing unit).
fn per_million(per_token: f64) -> f64 {
    per_token * 1_000_000.0
}

/// A parsed, cached OpenRouter catalogue: the model metadata list plus a
/// `model id → pricing` map, both derived from one `GET /models` response.
#[derive(Debug, Default, Clone)]
pub struct DynamicCatalog {
    models: Vec<ModelInfo>,
    pricing: HashMap<String, ModelPricing>,
}

impl DynamicCatalog {
    /// Parse a raw `GET /models` JSON body into a [`DynamicCatalog`].
    ///
    /// Tolerant by design: a model with no usable `prompt`/`completion` price
    /// still contributes to the model list (so it dispatches) but is omitted
    /// from the pricing map (so the cost path falls back to "unknown price",
    /// which it already tolerates). Returns a JSON error only when the envelope
    /// itself is malformed.
    pub fn parse(
        body: &str,
        effective_at: chrono::DateTime<chrono::Utc>,
    ) -> serde_json::Result<Self> {
        let resp: ModelsResponse = serde_json::from_str(body)?;
        let mut models = Vec::with_capacity(resp.data.len());
        let mut pricing = HashMap::with_capacity(resp.data.len());

        for m in resp.data {
            let max_output_tokens = m
                .top_provider
                .as_ref()
                .and_then(|tp| tp.max_completion_tokens)
                .unwrap_or(0);
            models.push(ModelInfo {
                id: m.id.clone(),
                provider: PROVIDER_ID.to_string(),
                // OpenRouter is OpenAI-compatible chat + streaming; richer
                // capability detection (from `supported_parameters`) is out of
                // scope for this pass.
                capabilities: vec![Capability::Text, Capability::Streaming],
                max_input_tokens: m.context_length.unwrap_or(0),
                max_output_tokens,
            });

            if let Some(pr) = &m.pricing {
                // Both base rates must parse for the model to be priceable. A
                // free model reports "0"/"0" → priced at exactly zero.
                if let (Some(prompt), Some(completion)) = (
                    pr.prompt.as_ref().and_then(PriceValue::per_token),
                    pr.completion.as_ref().and_then(PriceValue::per_token),
                ) {
                    pricing.insert(
                        m.id,
                        ModelPricing {
                            input_per_million: per_million(prompt),
                            output_per_million: per_million(completion),
                            cached_input_per_million: pr
                                .input_cache_read
                                .as_ref()
                                .and_then(PriceValue::per_token)
                                .map(per_million),
                            cache_write_per_million: None,
                            batch_input_per_million: None,
                            batch_output_per_million: None,
                            prompt_cache_min_tokens: None,
                            // OpenRouter exposes no Flex service tier.
                            flex_input_per_million: None,
                            flex_output_per_million: None,
                            effective_at,
                        },
                    );
                }
            }
        }

        Ok(Self { models, pricing })
    }

    /// Number of models in the catalogue.
    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// Whether the catalogue is empty (no fetch has populated it yet).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// All model metadata, cloned.
    #[must_use]
    pub fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    /// Pricing for `model`, if the fetched catalogue priced it.
    #[must_use]
    pub fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.pricing.get(model).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn now() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn parses_numeric_and_string_prices_to_per_million() {
        let body = serde_json::json!({
            "data": [
                {
                    "id": "a/num",
                    "context_length": 200_000,
                    "pricing": { "prompt": 0.000003, "completion": 0.000015 },
                    "top_provider": { "max_completion_tokens": 64_000 }
                },
                {
                    "id": "b/str",
                    "context_length": 128_000,
                    "pricing": { "prompt": "0.00000055", "completion": "0.00000219" }
                }
            ]
        })
        .to_string();

        let cat = DynamicCatalog::parse(&body, now()).expect("valid");
        assert_eq!(cat.len(), 2);

        let num = cat.pricing("a/num").expect("priced");
        assert!((num.input_per_million - 3.0).abs() < 1e-6);
        assert!((num.output_per_million - 15.0).abs() < 1e-6);

        let s = cat.pricing("b/str").expect("priced");
        assert!((s.input_per_million - 0.55).abs() < 1e-4);
        assert!((s.output_per_million - 2.19).abs() < 1e-4);

        // context_length → max_input_tokens; top_provider cap → max_output.
        let info = cat.models().into_iter().find(|m| m.id == "a/num").unwrap();
        assert_eq!(info.max_input_tokens, 200_000);
        assert_eq!(info.max_output_tokens, 64_000);
        assert_eq!(info.provider, "openrouter");
    }

    #[test]
    fn free_model_prices_at_zero() {
        let body = serde_json::json!({
            "data": [{ "id": "free/m", "pricing": { "prompt": "0", "completion": "0" } }]
        })
        .to_string();
        let cat = DynamicCatalog::parse(&body, now()).expect("valid");
        let p = cat.pricing("free/m").expect("free model is priced (zero)");
        assert_eq!(p.input_per_million, 0.0);
        assert_eq!(p.output_per_million, 0.0);
    }

    #[test]
    fn sentinel_and_missing_prices_are_omitted_but_model_kept() {
        let body = serde_json::json!({
            "data": [
                // -1 sentinel ("no fixed price", e.g. a router) → unpriced.
                { "id": "router/x", "pricing": { "prompt": "-1", "completion": "-1" } },
                // pricing object absent entirely → unpriced.
                { "id": "no/pricing", "context_length": 1000 },
                // empty-string price → unpriced.
                { "id": "empty/x", "pricing": { "prompt": "", "completion": "" } }
            ]
        })
        .to_string();
        let cat = DynamicCatalog::parse(&body, now()).expect("valid");
        // All three appear in the model list (so they still dispatch).
        assert_eq!(cat.len(), 3);
        // None are priced.
        assert!(cat.pricing("router/x").is_none());
        assert!(cat.pricing("no/pricing").is_none());
        assert!(cat.pricing("empty/x").is_none());
    }

    #[test]
    fn empty_data_array_parses_to_empty_catalog() {
        let cat = DynamicCatalog::parse(r#"{"data":[]}"#, now()).expect("valid");
        assert!(cat.is_empty());
    }

    #[test]
    fn malformed_envelope_is_an_error() {
        assert!(DynamicCatalog::parse("not json", now()).is_err());
    }
}
