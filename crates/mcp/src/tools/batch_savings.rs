//! batch_savings — flag batch-eligible (latency-insensitive) request-log
//! traffic and project the Batch-API savings (Batch/Flex phase 1, ADVISORY).
//!
//! The OpenAI / Anthropic / Gemini Batch APIs price async (≤24h) traffic at
//! ~50% of standard. This tool takes request-log aggregates grouped by
//! `(provider, model, tag)` — the same shape the advisor already reasons over —
//! detects the segments whose tag is non-interactive (background / offline /
//! nightly / bulk, or a caller-supplied set) AND that carry a Batch rate in the
//! catalog, and returns the projected savings of moving them to the Batch API.
//!
//! Savings come from the **real per-model catalog rates** (`pricing.toml`'s
//! `batch_{input,output}_per_million`) — a model with no batch tier is not
//! flagged, so we never fabricate a 50% discount where the catalog lacks data.
//! This tool is read-only and submits nothing to any batch API; it only
//! surfaces the projection so the advisor can ground a finding in real numbers.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use tt_shared::batch_advisor::{
    project_batch_savings, project_batch_savings_with_tags, RequestAggregate,
};
use tt_shared::pricing::catalog;

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct BatchSavingsTool;

#[derive(Deserialize)]
struct Input {
    /// Request-log aggregates, one row per `(provider, model, tag)` segment.
    aggregates: Vec<RequestAggregate>,
    /// Optional override for the non-interactive tag set. When omitted, the
    /// default vocabulary (background / offline / nightly / batch / bulk /
    /// async) is used.
    #[serde(default)]
    eligible_tags: Option<Vec<String>>,
}

#[async_trait]
impl Tool for BatchSavingsTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "batch_savings",
            description: "Given request-log aggregates grouped by (provider, model, tag), \
                flag traffic that is batch-eligible — requests tagged as non-interactive / \
                background / offline / nightly / bulk (or a caller-supplied tag set) — and \
                project the savings of moving it to the provider's Batch API (async ≤24h, \
                ~50% off). Savings use the real per-model batch rates from the pricing \
                catalog, not a flat discount; a model with no batch tier is not flagged. \
                Returns one finding per eligible tag with eligible spend, projected batch \
                cost, projected savings, and the tag's share of total spend. Advisory only \
                — it does NOT submit anything to a batch API.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "aggregates": {
                        "type": "array",
                        "description": "Per-(provider, model, tag) request-log aggregates.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "provider": { "type": "string" },
                                "model": { "type": "string" },
                                "tag": { "type": ["string", "null"] },
                                "input_tokens": { "type": "integer" },
                                "output_tokens": { "type": "integer" },
                                "cost_usd": { "type": "number" },
                                "request_count": { "type": "integer" }
                            },
                            "required": [
                                "provider", "model", "input_tokens",
                                "output_tokens", "cost_usd", "request_count"
                            ]
                        }
                    },
                    "eligible_tags": {
                        "type": "array",
                        "description": "Optional override for the non-interactive tag set.",
                        "items": { "type": "string" }
                    }
                },
                "required": ["aggregates"]
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let inp: Input =
            serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;

        let findings = match &inp.eligible_tags {
            Some(tags) => {
                let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                project_batch_savings_with_tags(&inp.aggregates, catalog(), &refs)
            }
            None => project_batch_savings(&inp.aggregates, catalog()),
        };

        let total_savings: f64 = findings.iter().map(|f| f.projected_savings_usd).sum();
        let findings_json: Vec<Value> = findings
            .iter()
            .map(|f| {
                json!({
                    "tag": f.tag,
                    "eligible_spend_usd": f.eligible_spend_usd,
                    "projected_batch_cost_usd": f.projected_batch_cost_usd,
                    "projected_savings_usd": f.projected_savings_usd,
                    "share_of_spend_pct": f.share_of_spend_pct,
                    "discount_pct": f.discount_pct(),
                    "request_count": f.request_count,
                    "summary": f.summary(),
                })
            })
            .collect();

        Ok(json!({
            "findings": findings_json,
            "total_projected_savings_usd": total_savings,
            "note": "Advisory projection over request_logs — savings use real catalog \
                Batch-API rates. Nothing was submitted to a batch API. Building the \
                durable batch queue is deferred.",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn def_requires_aggregates() {
        let d = BatchSavingsTool.def();
        assert_eq!(d.name, "batch_savings");
        let req = d
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(req.iter().any(|v| v.as_str() == Some("aggregates")));
    }

    /// The advertised description discloses it is advisory and does not submit
    /// anything — the spec forbids actually batching here.
    #[test]
    fn def_discloses_advisory_only() {
        let desc = BatchSavingsTool.def().description.to_lowercase();
        assert!(desc.contains("advisory only"), "{desc}");
        assert!(
            desc.contains("does not submit") || desc.contains("not submit"),
            "{desc}"
        );
    }

    /// End-to-end through the tool: an eligible tagged segment yields a finding
    /// with the catalog −50% savings; an untagged segment is ignored.
    #[tokio::test]
    async fn call_projects_savings_for_eligible_segment() {
        let result = BatchSavingsTool
            .call(json!({
                "aggregates": [
                    {
                        "provider": "openai", "model": "gpt-5.5", "tag": "nightly",
                        "input_tokens": 1_000_000, "output_tokens": 1_000_000,
                        "cost_usd": 35.0, "request_count": 10
                    },
                    {
                        "provider": "openai", "model": "gpt-5.5", "tag": null,
                        "input_tokens": 1_000_000, "output_tokens": 1_000_000,
                        "cost_usd": 35.0, "request_count": 10
                    }
                ]
            }))
            .await
            .unwrap();
        let findings = result["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "only the tagged segment is flagged");
        let f = &findings[0];
        assert_eq!(f["tag"], "nightly");
        // batch $2.50/$15 → $17.50 → savings $17.50.
        assert!((f["projected_savings_usd"].as_f64().unwrap() - 17.50).abs() < 1e-9);
        assert!((f["share_of_spend_pct"].as_f64().unwrap() - 50.0).abs() < 1e-9);
        assert!(f["summary"].as_str().unwrap().contains("tag=nightly"));
        assert!((result["total_projected_savings_usd"].as_f64().unwrap() - 17.50).abs() < 1e-9);
    }

    /// A caller-supplied tag set is honored.
    #[tokio::test]
    async fn call_honors_custom_eligible_tags() {
        let result = BatchSavingsTool
            .call(json!({
                "eligible_tags": ["nightly-evals"],
                "aggregates": [
                    {
                        "provider": "openai", "model": "gpt-5.5", "tag": "nightly-evals",
                        "input_tokens": 1_000_000, "output_tokens": 0,
                        "cost_usd": 5.0, "request_count": 1
                    },
                    {
                        "provider": "openai", "model": "gpt-5.5", "tag": "background",
                        "input_tokens": 1_000_000, "output_tokens": 0,
                        "cost_usd": 5.0, "request_count": 1
                    }
                ]
            }))
            .await
            .unwrap();
        let findings = result["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "only the custom tag matches");
        assert_eq!(findings[0]["tag"], "nightly-evals");
    }

    #[tokio::test]
    async fn call_invalid_params_errors() {
        let r = BatchSavingsTool.call(json!({ "wrong": 1 })).await;
        assert!(matches!(r, Err(McpError::InvalidParams(_))));
    }

    #[tokio::test]
    async fn call_empty_aggregates_returns_no_findings() {
        let result = BatchSavingsTool
            .call(json!({ "aggregates": [] }))
            .await
            .unwrap();
        assert!(result["findings"].as_array().unwrap().is_empty());
        assert_eq!(result["total_projected_savings_usd"].as_f64().unwrap(), 0.0);
    }
}
