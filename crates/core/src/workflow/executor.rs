//! Workflow node executor — bridges the workflow engine to the gateway agent path.
//!
//! `NodeExecutor` is the trait the engine calls to run Model/Agent nodes.
//! `GatewayNodeExecutor` is the production impl that delegates each node to
//! `agent_run::drive_workflow_node` (= the proven W0a/W0b per-turn loop).
//!
use async_trait::async_trait;
use serde_json::json;
use uuid::Uuid;

use tt_shared::messages::{Message, MessageContent, Tool};

use crate::{
    error::ApiError,
    routes::agent_run::{self, LoopOutcome},
    workflow::types::{ModelSelection, NodeOutput, WorkflowDefinition},
    AppState,
};

// ---------------------------------------------------------------------------
// IntelligenceSpec — caller-assembled, fully-substituted node invocation spec
// ---------------------------------------------------------------------------

/// All inputs the executor needs to run one Model or Agent node.  The `prompt`
/// is the already-substituted user message; `selection` drives model/route
/// resolution.
pub(crate) struct IntelligenceSpec {
    pub selection: ModelSelection,
    /// Already-substituted user prompt (engine handles `{{var}}` expansion).
    pub prompt: String,
    pub tools: Vec<Tool>,
    /// Turn cap (1 for Model nodes, N for Agent nodes).
    pub max_turns: u32,
    pub max_cost_usd: Option<f64>,
}

// ---------------------------------------------------------------------------
// NodeExecutor trait
// ---------------------------------------------------------------------------

#[async_trait]
pub(crate) trait NodeExecutor: Send + Sync {
    async fn run_intelligence(
        &self,
        node_id: &str,
        spec: &IntelligenceSpec,
    ) -> Result<NodeOutput, ApiError>;

    /// Load the latest version of a child workflow definition scoped to the
    /// same org as this executor.  Returns [`ApiError::NotFound`] if no
    /// workflow with that id exists for the org, or
    /// [`ApiError::ServiceUnavailable`] if the backing store is unavailable.
    #[allow(dead_code)]
    async fn load_subworkflow(&self, id: uuid::Uuid) -> Result<WorkflowDefinition, ApiError>;
}

// ---------------------------------------------------------------------------
// GatewayNodeExecutor — production impl over drive_workflow_node
// ---------------------------------------------------------------------------

/// Production `NodeExecutor` that routes each node through the real per-turn
/// gateway loop (`agent_run::drive_workflow_node`).  Holds the run-level caller
/// identity; the node-level fields (prompt, tools, etc.) come in via
/// `IntelligenceSpec`.
pub(crate) struct GatewayNodeExecutor<'a> {
    pub state: &'a AppState,
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub caller_tier: Option<tt_shared::CallerTier>,
    pub l2_allowed: bool,
    pub raw_bearer: String,
    pub run_id: Uuid,
}

// ---------------------------------------------------------------------------
// selection → (model, route_ref) — pure, unit-testable mapping
// ---------------------------------------------------------------------------

/// Map a `ModelSelection` to the `(model, route_ref)` pair expected by
/// `drive_workflow_node`.
///
/// - `Model{model}` → (`model`, `None`)  — explicit model id, no routing override
/// - `Route{route_ref}` → (`""`, `Some(route_ref)`) — empty model; routing is
///   driven by `forced_route` inside `RunIdentity`, which resolves the actual
///   model per turn
/// - `Auto` → (`""`, `None`) — let the gateway pick automatically
fn selection_to_model_route(selection: &ModelSelection) -> (String, Option<String>) {
    match selection {
        ModelSelection::Model { model } => (model.clone(), None),
        ModelSelection::Route { route_ref } => (String::new(), Some(route_ref.clone())),
        ModelSelection::Auto => (String::new(), None),
    }
}

#[async_trait]
impl NodeExecutor for GatewayNodeExecutor<'_> {
    async fn load_subworkflow(&self, id: uuid::Uuid) -> Result<WorkflowDefinition, ApiError> {
        let pool = self
            .state
            .db_pool
            .as_ref()
            .ok_or_else(|| ApiError::ServiceUnavailable("workflow store unavailable".into()))?;
        crate::workflow::store::get_definition(pool, self.org_id, id)
            .await
            .map(|(def, _version)| def)
            .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {id}")))
    }

    async fn run_intelligence(
        &self,
        node_id: &str,
        spec: &IntelligenceSpec,
    ) -> Result<NodeOutput, ApiError> {
        let (model, route_ref) = selection_to_model_route(&spec.selection);

        // Build a single-element transcript from the substituted prompt.
        let messages = vec![Message::User {
            content: MessageContent::Text(spec.prompt.clone()),
            name: None,
        }];

        let outcome = agent_run::drive_workflow_node(
            self.state,
            self.org_id,
            self.api_key_id,
            self.caller_tier,
            self.l2_allowed,
            self.raw_bearer.clone(),
            self.run_id,
            node_id.to_string(),
            model,
            messages,
            spec.tools.clone(),
            spec.max_turns,
            spec.max_cost_usd,
            route_ref,
            None, // workflow-level tag threading deferred to Task 6+
        )
        .await;

        match outcome {
            LoopOutcome::Terminal(run) => {
                // Extract the last assistant text from the transcript.
                let last_text = run
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| {
                        if let Message::Assistant {
                            content: Some(MessageContent::Text(t)),
                            ..
                        } = m
                        {
                            Some(t.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                // `model_used`: not tracked on `Run` in W1a (the request_logs row
                // carries the per-turn model; surfacing it here is deferred to W2).
                Ok(NodeOutput {
                    content: json!(last_text),
                    cost_usd: run.usage.cost_usd,
                    baseline_cost_usd: run.usage.baseline_cost_usd,
                    model_used: None,
                })
            }
            // Workflows don't use client (non-gateway) tools in W1a — the agent
            // loop is expected to terminate.  Treat an unexpected pause as an
            // internal error so the engine can surface it clearly.
            LoopOutcome::Paused { .. } => Err(ApiError::Internal(
                "workflow node unexpectedly paused on a client tool (not supported in W1a)".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::types::ModelSelection;

    // ---- Task 3: NodeOutput.baseline_cost_usd is threaded from RunUsage -----

    /// Verify the `baseline_cost_usd` field exists on `NodeOutput` and round-trips
    /// through serde correctly (incl. `#[serde(default)]` back-compat).
    #[test]
    fn node_output_baseline_cost_usd_field_and_roundtrip() {
        // Direct construction with a non-zero baseline.
        let out = NodeOutput {
            content: serde_json::json!("hello"),
            cost_usd: 0.10,
            baseline_cost_usd: 0.15,
            model_used: None,
        };
        assert!(
            (out.baseline_cost_usd - 0.15).abs() < 1e-9,
            "field must carry the assigned value"
        );

        // Serde round-trip preserves the value.
        let json = serde_json::to_string(&out).unwrap();
        let back: NodeOutput = serde_json::from_str(&json).unwrap();
        assert!((back.baseline_cost_usd - 0.15).abs() < 1e-9);

        // Old JSON without the field deserializes to 0.0 (serde default).
        let old_json = r#"{"content":"hi","cost_usd":0.05,"model_used":null}"#;
        let old: NodeOutput = serde_json::from_str(old_json).unwrap();
        assert_eq!(
            old.baseline_cost_usd, 0.0,
            "missing field must default to 0"
        );
    }

    #[test]
    fn selection_to_model_route_explicit_model() {
        let (model, route) = selection_to_model_route(&ModelSelection::Model {
            model: "claude-3-5-haiku-20241022".into(),
        });
        assert_eq!(model, "claude-3-5-haiku-20241022");
        assert_eq!(route, None);
    }

    #[test]
    fn selection_to_model_route_named_route() {
        let (model, route) = selection_to_model_route(&ModelSelection::Route {
            route_ref: "my-route".into(),
        });
        // Route-based selection: model is empty; forced_route carries the route.
        assert_eq!(model, "");
        assert_eq!(route, Some("my-route".to_string()));
    }

    #[test]
    fn selection_to_model_route_auto() {
        let (model, route) = selection_to_model_route(&ModelSelection::Auto);
        // Auto: let the gateway decide; no model pin, no forced route.
        assert_eq!(model, "");
        assert_eq!(route, None);
    }
}
