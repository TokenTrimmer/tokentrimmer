//! Deep-research panel — caller-facing opt-in surface.
//!
//! # Off-by-default
//! A request with no `X-TokenTrimmer-Panel` header is completely untouched —
//! `panel_from_header` returns `None`. An unknown strategy value is also silently
//! treated as `None` (never an error at the header layer).
//!
//! # Types produced (consumed by Tasks 3–7)
//! - [`ArbiterStrategyKind`] — the three dispatch strategies
//! - [`ModelRef`]            — a model + optional provider pin
//! - [`PanelConfig`]         — resolved, complete panel configuration
//! - [`PanelDefaults`]       — gateway-level defaults sourced from env vars
//! - [`PanelExtras`]         — per-request overrides from `tt_extras.panel`

use axum::http::HeaderMap;

use tt_shared::messages::PanelExtras;

use crate::{ApiError, ApiResult};

// ---------------------------------------------------------------------------
// Strategy kind
// ---------------------------------------------------------------------------

/// Which arbitration algorithm the panel should run after collecting all legs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArbiterStrategyKind {
    /// Synthesize a new answer from all legs using an arbiter model.
    Synthesize,
    /// Pick the single best leg as judged by the arbiter model.
    BestOfN,
    /// Return the majority-vote answer (simple token-overlap majority).
    Majority,
}

impl ArbiterStrategyKind {
    /// Wire-format string (matches the header value and config serialization).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synthesize => "synthesize",
            Self::BestOfN => "best-of-n",
            Self::Majority => "majority",
        }
    }

    /// Parse a strategy from its wire-format string (case-insensitive).
    /// Returns `None` for unknown values — callers should treat that as "no panel".
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "synthesize" => Some(Self::Synthesize),
            "best-of-n" | "best_of_n" => Some(Self::BestOfN),
            "majority" => Some(Self::Majority),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ModelRef — a model + optional provider pin
// ---------------------------------------------------------------------------

/// A model reference with an optional explicit provider.
///
/// `"model-id"` without a provider → the gateway resolves the provider via
/// its standard registry. `"model-id"` with `provider = Some("openai")` →
/// dispatch is pinned to that provider.
#[derive(Clone, Debug, Default)]
pub struct ModelRef {
    pub model: String,
    pub provider: Option<String>,
}

// ---------------------------------------------------------------------------
// PanelConfig — resolved, complete panel configuration
// ---------------------------------------------------------------------------

/// Fully-resolved panel configuration for one request.
///
/// Constructed by [`PanelConfig::resolve`] from the header strategy + optional
/// per-request [`PanelExtras`] + gateway [`PanelDefaults`].
#[derive(Clone, Debug)]
pub struct PanelConfig {
    /// Which arbitration algorithm to run.
    pub strategy: ArbiterStrategyKind,
    /// Panel member models (at least one guaranteed after `resolve`).
    pub members: Vec<ModelRef>,
    /// The arbiter model used for Synthesize / BestOfN.
    pub arbiter_model: ModelRef,
    /// Minimum legs that must succeed for the panel to return a result.
    /// `None` → all members must succeed (implicit quorum = members.len()).
    pub quorum: Option<usize>,
    /// Hard cost ceiling in USD across all legs + arbitration. `None` → no cap.
    pub max_cost_usd: Option<f64>,
}

impl PanelConfig {
    /// Resolve a complete [`PanelConfig`] from its three input sources.
    ///
    /// Precedence (highest → lowest):
    /// 1. `extras` — per-request `tt_extras.panel` overrides
    /// 2. `defaults` — gateway-level defaults from env vars
    ///
    /// Returns [`ApiError::InvalidRequest`] when the merged member list is empty.
    pub fn resolve(
        strategy: ArbiterStrategyKind,
        extras: Option<&PanelExtras>,
        defaults: &PanelDefaults,
    ) -> ApiResult<PanelConfig> {
        // Members: extras override defaults entirely when non-empty.
        let members: Vec<ModelRef> = if let Some(e) = extras {
            if !e.members.is_empty() {
                e.members
                    .iter()
                    .map(|m| ModelRef {
                        model: m.clone(),
                        provider: None,
                    })
                    .collect()
            } else {
                defaults.members.clone()
            }
        } else {
            defaults.members.clone()
        };

        if members.is_empty() {
            return Err(ApiError::InvalidRequest(
                "panel requires at least one member model".to_string(),
            ));
        }

        // Arbiter: extras override defaults.
        let arbiter_model = if let Some(e) = extras {
            if let Some(ref am) = e.arbiter_model {
                ModelRef {
                    model: am.clone(),
                    provider: None,
                }
            } else {
                defaults.arbiter_model.clone()
            }
        } else {
            defaults.arbiter_model.clone()
        };

        let quorum = extras.and_then(|e| e.quorum);
        let max_cost_usd = extras.and_then(|e| e.max_cost_usd);

        Ok(PanelConfig {
            strategy,
            members,
            arbiter_model,
            quorum,
            max_cost_usd,
        })
    }
}

// ---------------------------------------------------------------------------
// PanelDefaults — gateway-level defaults from env vars
// ---------------------------------------------------------------------------

/// Gateway-level panel defaults, sourced from environment variables:
/// - `TT_PANEL_DEFAULT_MEMBERS` — comma-separated model ids
/// - `TT_PANEL_DEFAULT_ARBITER` — a single model id
///
/// Construct with [`PanelDefaults::from_env`] at server startup.
#[derive(Clone, Debug, Default)]
pub struct PanelDefaults {
    /// Default panel members when the request does not specify `tt_extras.panel.members`.
    pub members: Vec<ModelRef>,
    /// Default arbiter model when the request does not specify
    /// `tt_extras.panel.arbiter_model`.
    pub arbiter_model: ModelRef,
}

impl PanelDefaults {
    /// Build [`PanelDefaults`] from environment variables.
    ///
    /// - `TT_PANEL_DEFAULT_MEMBERS`: comma-separated model ids
    ///   (e.g. `"gpt-4o,claude-3-5-sonnet"`). Absent → empty list.
    /// - `TT_PANEL_DEFAULT_ARBITER`: a single model id used as the arbiter.
    ///   Absent → `""` (will cause `resolve` to fail unless extras provide one).
    pub fn from_env() -> Self {
        let members: Vec<ModelRef> = std::env::var("TT_PANEL_DEFAULT_MEMBERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|model| ModelRef {
                model,
                provider: None,
            })
            .collect();

        let arbiter_model = ModelRef {
            model: std::env::var("TT_PANEL_DEFAULT_ARBITER").unwrap_or_default(),
            provider: None,
        };

        PanelDefaults {
            members,
            arbiter_model,
        }
    }
}

// ---------------------------------------------------------------------------
// Header parser
// ---------------------------------------------------------------------------

/// Parse `X-TokenTrimmer-Panel` into an [`ArbiterStrategyKind`].
///
/// Returns `None` when the header is absent **or** when the value is not a
/// recognized strategy — treat both as "no panel requested". The caller should
/// never return an error for an unknown strategy value (off-by-default contract).
pub fn panel_from_header(headers: &HeaderMap) -> Option<ArbiterStrategyKind> {
    headers
        .get("x-tokentrimmer-panel")
        .and_then(|v| v.to_str().ok())
        .and_then(ArbiterStrategyKind::parse)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::PanelExtras;

    #[test]
    fn as_str_round_trips() {
        assert_eq!(ArbiterStrategyKind::Synthesize.as_str(), "synthesize");
        assert_eq!(ArbiterStrategyKind::BestOfN.as_str(), "best-of-n");
        assert_eq!(ArbiterStrategyKind::Majority.as_str(), "majority");
    }

    #[test]
    fn parse_case_insensitive() {
        assert!(matches!(
            ArbiterStrategyKind::parse("SYNTHESIZE"),
            Some(ArbiterStrategyKind::Synthesize)
        ));
        assert!(ArbiterStrategyKind::parse("bogus").is_none());
    }

    #[test]
    fn resolve_extras_override_defaults() {
        let extras = PanelExtras {
            members: vec!["m1".to_string()],
            arbiter_model: Some("arbiter-x".to_string()),
            quorum: Some(1),
            max_cost_usd: Some(0.10),
        };
        let defaults = PanelDefaults {
            members: vec![ModelRef {
                model: "fallback".to_string(),
                provider: None,
            }],
            arbiter_model: ModelRef {
                model: "default-arbiter".to_string(),
                provider: None,
            },
        };
        let cfg =
            PanelConfig::resolve(ArbiterStrategyKind::BestOfN, Some(&extras), &defaults).unwrap();
        assert_eq!(cfg.members.len(), 1);
        assert_eq!(cfg.members[0].model, "m1");
        assert_eq!(cfg.arbiter_model.model, "arbiter-x");
        assert_eq!(cfg.quorum, Some(1));
        assert_eq!(cfg.max_cost_usd, Some(0.10));
    }
}
