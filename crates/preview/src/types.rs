//! Request + response shapes for the preview engine.
//!
//! The request is a small subset of the OpenAI chat-completion body —
//! exactly what we need to estimate. The response is the documented
//! `PreviewResponse` shape from the spec.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    /// Honored for tier accounting but ignored for the preview calc itself.
    #[serde(default)]
    pub stream: Option<bool>,
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
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"estimation_confidence\":\"high\""));
        assert!(json.contains("\"weighted_savings_usd\":0.000098"));
    }
}
