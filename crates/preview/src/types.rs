//! Request + response shapes for the preview engine.
//!
//! The request is a small subset of the OpenAI chat-completion body —
//! exactly what we need to estimate. The response is the documented
//! `PreviewResponse` shape from the spec.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::document_projection::DocProjection;

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Newer OpenAI output cap. Panel preview gives this precedence over
    /// `max_tokens`, matching Fusion admission and provider dispatch.
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    /// Number of completions requested per member for a Fusion panel. The
    /// base preview remains its existing single-completion catalog projection.
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    /// Honored for tier accounting but ignored for the preview calc itself.
    #[serde(default)]
    pub stream: Option<bool>,
    /// TokenTrimmer-internal extras (panel overrides, etc.) that augment the
    /// dry-run estimate. Absent/empty → defaults/env-var config only.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tt_extras: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: serde_json::Value, // string OR array-of-parts
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewResponse {
    pub current: CurrentEstimate,
    pub cache_projections: CacheProjections,
    pub route_suggestions: Vec<RouteSuggestion>,
    pub warnings: Vec<String>,
    pub trace_id: String,
    /// Present only when the request carries image parts: a directional
    /// image/document-token projection (raw image cost vs a projected distilled
    /// text-token cost). Absent for text-only requests. See
    /// [`crate::document_projection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_projection: Option<DocProjection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentEstimate {
    pub model: String,
    pub provider: String,
    pub input_tokens_estimated: u32,
    pub output_tokens_estimated: u32,
    pub cost_usd: f64,
    pub estimation_confidence: EstimationConfidence,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EstimationConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheProjections {
    pub l1_hit_savings_usd: f64,
    pub l1_hit_probability: f32,
    pub l2_hit_savings_usd: f64,
    pub l2_hit_probability: f32,
    pub weighted_savings_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteSuggestion {
    pub route: String,
    pub model: String,
    pub cost_usd: f64,
    pub savings_usd: f64,
    pub quality_risk_band: QualityRiskBand,
    pub rationale: String,
    pub applicable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum QualityRiskBand {
    Low,
    Medium,
    High,
    Unknown,
}

/// Convenience alias for the legacy name in the spec.
pub type Suggestion = RouteSuggestion;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_minimal_request() {
        let json = r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#;
        let req: PreviewRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "x");
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn serializes_response_shape_matches_spec() {
        let r = PreviewResponse {
            current: CurrentEstimate {
                model: "claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
                input_tokens_estimated: 47,
                output_tokens_estimated: 12,
                cost_usd: 0.000189,
                estimation_confidence: EstimationConfidence::High,
            },
            cache_projections: CacheProjections {
                l1_hit_savings_usd: 0.000189,
                l1_hit_probability: 0.34,
                l2_hit_savings_usd: 0.000189,
                l2_hit_probability: 0.18,
                weighted_savings_usd: 0.000098,
            },
            route_suggestions: vec![],
            warnings: vec![],
            trace_id: "trace".into(),
            doc_projection: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"estimation_confidence\":\"high\""));
        assert!(json.contains("\"weighted_savings_usd\":0.000098"));
        // Absent for a text-only response.
        assert!(!json.contains("doc_projection"));
    }

    #[test]
    fn doc_projection_serializes_when_present() {
        let r = PreviewResponse {
            current: CurrentEstimate {
                model: "gpt-4o".into(),
                provider: "openai".into(),
                input_tokens_estimated: 765,
                output_tokens_estimated: 16,
                cost_usd: 0.001,
                estimation_confidence: EstimationConfidence::High,
            },
            cache_projections: CacheProjections {
                l1_hit_savings_usd: 0.0,
                l1_hit_probability: 0.2,
                l2_hit_savings_usd: 0.0,
                l2_hit_probability: 0.1,
                weighted_savings_usd: 0.0,
            },
            route_suggestions: vec![],
            warnings: vec![],
            trace_id: "trace".into(),
            doc_projection: Some(crate::document_projection::project(765, 153, 5.0, "gpt-4o")),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"doc_projection\""));
        assert!(json.contains("\"basis\":\"estimate\""));
    }
}
