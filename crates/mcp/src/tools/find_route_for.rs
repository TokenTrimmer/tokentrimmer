//! find_route_for — keyword-based heuristic model router.
//!
//! This tool assigns a model based on task-class keywords. It is a
//! static heuristic only — it does NOT consult your organisation's
//! request logs or telemetry. Confidence claims and "historically handles"
//! language are intentionally absent; see the rationale field for the
//! actual basis of each recommendation.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct FindRouteForTool;

#[derive(Deserialize)]
struct Input {
    task_description: String,
}

/// Task class assigned by keyword matching.
#[derive(Debug, PartialEq)]
pub enum TaskClass {
    Classification,
    Extraction,
    Code,
    Reasoning,
    General,
}

/// Classify a task description into a [`TaskClass`] by keyword heuristic.
pub fn classify_task(lower: &str) -> TaskClass {
    if lower.contains("classif")
        || lower.contains("yes or no")
        || lower.contains("boolean")
        || lower.contains("is it a")
        || lower.contains("label")
        || lower.contains("categorize")
        || lower.contains("categorise")
        || lower.contains("spam")
        || lower.contains("sentiment")
    {
        TaskClass::Classification
    } else if lower.contains("extract")
        || lower.contains("parse")
        || lower.contains("json")
        || lower.contains("structured output")
        || lower.contains("schema")
        || lower.contains("fill in")
    {
        TaskClass::Extraction
    } else if lower.contains("code")
        || lower.contains("function")
        || lower.contains("refactor")
        || lower.contains("debug")
        || lower.contains("implement")
        || lower.contains("unit test")
        || lower.contains("diff")
        || lower.contains("compile")
    {
        TaskClass::Code
    } else if lower.contains("reason")
        || lower.contains("analyz")
        || lower.contains("analys")
        || lower.contains("compare")
        || lower.contains("evaluate")
        || lower.contains("explain in depth")
        || lower.contains("step by step")
        || lower.contains("summarize")
        || lower.contains("summarise")
        || lower.contains("complex")
    {
        TaskClass::Reasoning
    } else {
        TaskClass::General
    }
}

/// Return the (model_id, rationale) heuristic pair for a task class.
///
/// Model IDs are cross-referenced against `crates/shared/data/pricing.toml`.
/// This is a static heuristic — not backed by per-organisation telemetry.
pub fn route(class: &TaskClass) -> (&'static str, &'static str) {
    match class {
        TaskClass::Classification => (
            "claude-haiku-4-5",
            "classification / boolean — keyword heuristic: short yes/no \
             or label tasks typically need minimal reasoning; \
             claude-haiku-4-5 is the cheapest Anthropic model in the catalog \
             ($1/$5 per M tokens). Not based on your telemetry.",
        ),
        TaskClass::Extraction => (
            "claude-haiku-4-5",
            "extraction / parsing — keyword heuristic: structured-output tasks \
             with an explicit schema are reliably handled by small models when \
             given a clear prompt; claude-haiku-4-5 is the cheapest option \
             in the catalog ($1/$5 per M tokens). Not based on your telemetry.",
        ),
        TaskClass::Code => (
            "claude-sonnet-4-6",
            "code / refactoring — keyword heuristic: code generation, debugging \
             and multi-step refactors benefit from stronger reasoning; \
             claude-sonnet-4-6 is the mid-tier Anthropic model \
             ($3/$15 per M tokens). Not based on your telemetry.",
        ),
        TaskClass::Reasoning => (
            "claude-sonnet-4-6",
            "reasoning / analysis — keyword heuristic: tasks involving \
             comparison, evaluation or detailed explanation benefit from \
             claude-sonnet-4-6's stronger reasoning \
             ($3/$15 per M tokens). Not based on your telemetry.",
        ),
        TaskClass::General => (
            "claude-haiku-4-5",
            "general / chat — keyword heuristic: no strong signal detected; \
             defaulting to claude-haiku-4-5 as the lowest-cost option \
             in the catalog ($1/$5 per M tokens). \
             Escalate to claude-sonnet-4-6 or claude-opus-4-7 if quality \
             is insufficient. Not based on your telemetry.",
        ),
    }
}

#[async_trait]
impl Tool for FindRouteForTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "find_route_for",
            description: "Given a task description in plain English, return a \
                suggested model based on a static keyword heuristic over task \
                class (classification, extraction, code, reasoning, general). \
                This is NOT backed by your organisation's historical telemetry — \
                it is a cost-discipline starting point. Check the rationale field \
                for the basis of the recommendation.",
            input_schema: json!({
                "type": "object",
                "properties": { "task_description": { "type": "string" } },
                "required": ["task_description"]
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let inp: Input =
            serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let lower = inp.task_description.to_lowercase();
        let class = classify_task(&lower);
        let (model, rationale) = route(&class);
        Ok(json!({ "model": model, "rationale": rationale }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── honest-language tests ────────────────────────────────────────────────

    #[test]
    fn description_contains_no_historical_language() {
        let tool = FindRouteForTool;
        let desc = tool.def().description;
        let lower = desc.to_lowercase();
        assert!(
            !lower.contains("historically"),
            "description must not claim historical data: {desc}"
        );
        assert!(
            !lower.contains("high quality confidence"),
            "description must not claim quality confidence: {desc}"
        );
        assert!(
            !lower.contains("evidence"),
            "description must not claim evidence: {desc}"
        );
    }

    #[test]
    fn all_rationales_contain_no_historical_language() {
        let classes = [
            TaskClass::Classification,
            TaskClass::Extraction,
            TaskClass::Code,
            TaskClass::Reasoning,
            TaskClass::General,
        ];
        for class in &classes {
            let (_, rationale) = route(class);
            let lower = rationale.to_lowercase();
            assert!(
                !lower.contains("historically handles"),
                "rationale for {class:?} must not say 'historically handles': {rationale}"
            );
            assert!(
                !lower.contains("high quality confidence"),
                "rationale for {class:?} must not claim quality confidence: {rationale}"
            );
        }
    }

    #[test]
    fn all_rationales_disclose_heuristic_basis() {
        let classes = [
            TaskClass::Classification,
            TaskClass::Extraction,
            TaskClass::Code,
            TaskClass::Reasoning,
            TaskClass::General,
        ];
        for class in &classes {
            let (_, rationale) = route(class);
            assert!(
                rationale.contains("heuristic") || rationale.contains("Not based on your telemetry"),
                "rationale for {class:?} must disclose heuristic basis: {rationale}"
            );
        }
    }

    // ── varied model mapping tests ───────────────────────────────────────────

    #[test]
    fn classification_and_code_map_to_different_models() {
        let (classify_model, _) = route(&TaskClass::Classification);
        let (code_model, _) = route(&TaskClass::Code);
        assert_ne!(
            classify_model, code_model,
            "classification and code tasks should route to different models"
        );
    }

    #[test]
    fn code_tasks_route_to_sonnet() {
        let (model, _) = route(&TaskClass::Code);
        assert_eq!(model, "claude-sonnet-4-6");
    }

    #[test]
    fn reasoning_tasks_route_to_sonnet() {
        let (model, _) = route(&TaskClass::Reasoning);
        assert_eq!(model, "claude-sonnet-4-6");
    }

    #[test]
    fn classification_routes_to_haiku() {
        let (model, _) = route(&TaskClass::Classification);
        assert_eq!(model, "claude-haiku-4-5");
    }

    #[test]
    fn extraction_routes_to_haiku() {
        let (model, _) = route(&TaskClass::Extraction);
        assert_eq!(model, "claude-haiku-4-5");
    }

    #[test]
    fn general_routes_to_haiku() {
        let (model, _) = route(&TaskClass::General);
        assert_eq!(model, "claude-haiku-4-5");
    }

    // ── catalog-valid model IDs ──────────────────────────────────────────────

    /// All model IDs emitted by the router must appear in pricing.toml.
    #[test]
    fn all_routed_models_are_in_catalog() {
        // Extracted from crates/shared/data/pricing.toml (anthropic provider rows).
        const CATALOG_MODELS: &[&str] = &[
            "claude-haiku-4-5",
            "claude-sonnet-4-6",
            "claude-opus-4-7",
            // OpenAI
            "gpt-5.5",
            "gpt-5.4",
            "gpt-4o",
            "gpt-4o-mini",
            "o3",
            "o4-mini",
            // Gemini
            "gemini-3.1-flash-lite",
            "gemini-3.5-flash",
            "gemini-3.1-pro",
            // Groq
            "llama-3.3-70b-versatile",
            "llama-3.1-8b-instant",
            "deepseek-r1-distill-llama-70b",
            "mixtral-8x7b-32768",
            // Mistral
            "mistral-large-latest",
            "mistral-medium-latest",
            "mistral-small-latest",
            "codestral-latest",
            "pixtral-large-latest",
        ];

        let classes = [
            TaskClass::Classification,
            TaskClass::Extraction,
            TaskClass::Code,
            TaskClass::Reasoning,
            TaskClass::General,
        ];
        for class in &classes {
            let (model, _) = route(class);
            assert!(
                CATALOG_MODELS.contains(&model),
                "model '{model}' for {class:?} is not in pricing.toml catalog"
            );
        }
    }

    // ── keyword classification tests ─────────────────────────────────────────

    #[test]
    fn classify_keywords_hit_expected_classes() {
        assert_eq!(classify_task("classify this email as spam"), TaskClass::Classification);
        assert_eq!(classify_task("yes or no: is this valid?"), TaskClass::Classification);
        assert_eq!(classify_task("extract the name and date as json"), TaskClass::Extraction);
        assert_eq!(classify_task("refactor the function to use iterators"), TaskClass::Code);
        assert_eq!(classify_task("debug this compile error"), TaskClass::Code);
        assert_eq!(classify_task("analyze the tradeoffs between A and B"), TaskClass::Reasoning);
        assert_eq!(classify_task("tell me a joke"), TaskClass::General);
    }

    // ── async tool call tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn tool_call_classify_returns_haiku() {
        let tool = FindRouteForTool;
        let result = tool
            .call(serde_json::json!({ "task_description": "classify this email as spam" }))
            .await
            .unwrap();
        assert_eq!(result["model"], "claude-haiku-4-5");
        let rationale = result["rationale"].as_str().unwrap();
        assert!(!rationale.to_lowercase().contains("historically handles"));
    }

    #[tokio::test]
    async fn tool_call_code_returns_sonnet() {
        let tool = FindRouteForTool;
        let result = tool
            .call(serde_json::json!({ "task_description": "refactor this function to reduce allocations" }))
            .await
            .unwrap();
        assert_eq!(result["model"], "claude-sonnet-4-6");
        let rationale = result["rationale"].as_str().unwrap();
        assert!(rationale.contains("Not based on your telemetry"));
    }

    #[tokio::test]
    async fn tool_call_reasoning_returns_sonnet() {
        let tool = FindRouteForTool;
        let result = tool
            .call(serde_json::json!({ "task_description": "analyze the tradeoffs between microservices and monoliths" }))
            .await
            .unwrap();
        assert_eq!(result["model"], "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn tool_call_invalid_params_returns_error() {
        let tool = FindRouteForTool;
        let result = tool.call(serde_json::json!({ "wrong_key": 42 })).await;
        assert!(result.is_err());
    }
}
