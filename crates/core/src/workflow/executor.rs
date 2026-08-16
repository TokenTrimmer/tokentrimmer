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
    routes::agent_run::{self, LoopOutcome, RunStatus},
    routes::agent_run_budget::estimate_next_turn_cost,
    workflow::types::{ModelSelection, NodeOutput, WorkflowDefinition},
    AppState,
};

// ---------------------------------------------------------------------------
// IntelligenceSpec — caller-assembled, fully-substituted node invocation spec
// ---------------------------------------------------------------------------

/// All inputs the executor needs to run one Model or Agent node.  The `prompt`
/// is the already-substituted user message; `selection` drives model/route
/// resolution.
#[derive(Clone)]
pub(crate) struct IntelligenceSpec {
    pub selection: ModelSelection,
    /// Already-substituted user prompt (engine handles `{{var}}` expansion).
    pub prompt: String,
    pub tools: Vec<Tool>,
    /// Turn cap (1 for Model nodes, N for Agent nodes).
    pub max_turns: u32,
    /// Optional output ceiling applied to every model turn.
    pub max_output_tokens: Option<u32>,
    pub max_cost_usd: Option<f64>,
}

/// Project the single provider turn represented by a bounded workflow node.
///
/// The workflow admission gate currently permits capped Model nodes and
/// single-turn Agent nodes only when they use a pinned model, a declared output
/// cap, and no tools. Returning `None` for every other shape keeps this helper
/// honest if an internal caller bypasses that route-level gate. The engine uses
/// the result as an in-memory reservation before launch, then settles against
/// the executor's actual `NodeOutput.cost_usd`.
pub(crate) fn reservation_cost_usd(spec: &IntelligenceSpec) -> Option<f64> {
    if spec.max_turns != 1
        || !spec.tools.is_empty()
        || !matches!(spec.max_output_tokens, Some(value) if value > 0)
    {
        return None;
    }
    let ModelSelection::Model { model } = &spec.selection else {
        return None;
    };
    let messages = [Message::User {
        content: MessageContent::Text(spec.prompt.clone()),
        name: None,
    }];
    estimate_next_turn_cost(model, &messages, spec.max_output_tokens)
}

fn workflow_budget_dispatch_failure(status: RunStatus, note: Option<&str>) -> Option<String> {
    (status == RunStatus::Failed)
        .then_some(note)
        .flatten()
        .filter(|note| note.contains("workflow budget dispatch"))
        .map(str::to_string)
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
    async fn load_subworkflow(&self, id: uuid::Uuid) -> Result<WorkflowDefinition, ApiError>;

    /// Load a bounded set of latest child definitions. The default preserves
    /// simple test executors; the gateway implementation overrides it with one
    /// org-scoped batch query for whole-tree preflight.
    async fn load_subworkflows(
        &self,
        ids: &[uuid::Uuid],
    ) -> Result<std::collections::HashMap<uuid::Uuid, WorkflowDefinition>, ApiError> {
        let mut definitions = std::collections::HashMap::with_capacity(ids.len());
        for id in ids {
            definitions.insert(*id, self.load_subworkflow(*id).await?);
        }
        Ok(definitions)
    }
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

    async fn load_subworkflows(
        &self,
        ids: &[uuid::Uuid],
    ) -> Result<std::collections::HashMap<uuid::Uuid, WorkflowDefinition>, ApiError> {
        let pool = self
            .state
            .db_pool
            .as_ref()
            .ok_or_else(|| ApiError::ServiceUnavailable("workflow store unavailable".into()))?;
        crate::workflow::preflight::load_latest_definitions(pool, self.org_id, ids).await
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
            spec.max_output_tokens,
            spec.max_cost_usd,
            route_ref,
            None, // workflow-level tag threading deferred to Task 6+
        )
        .await;

        match outcome {
            LoopOutcome::Terminal(run) => {
                let capped = spec.max_cost_usd.is_some();
                match terminal_run_to_node_outcome(run, capped) {
                    NodeRunOutcome::Output(out) => Ok(out),
                    NodeRunOutcome::BudgetDispatchRejected(message) => {
                        Err(ApiError::InvalidRequest(message))
                    }
                    NodeRunOutcome::ProviderCallFailed {
                        accrued_cost_usd,
                        reason,
                    } => Err(ApiError::Internal(format!(
                        "node \"{node_id}\" provider call failed after accruing ${accrued_cost_usd:.4}: {reason}"
                    ))),
                }
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

/// Result of mapping a terminal agent-loop run onto the workflow node outcome
/// the engine sees. Pure + unit-testable — tests construct a [`agent_run::Run`]
/// directly (no provider, no DB).
#[derive(Debug)]
enum NodeRunOutcome {
    /// The node produced an output from its (partial) transcript.
    Output(NodeOutput),
    /// The per-node budget dispatch rejected the final routed request before
    /// provider work; the engine must propagate the rejection and stop the
    /// workflow (surfaced as `InvalidRequest`, unchanged semantics).
    BudgetDispatchRejected(String),
    /// On a CAPPED node, a started provider call failed. Surfaced so the
    /// engine's node journal records the failed call and the run's real accrued
    /// spend as evidence — instead of silently reporting a "completed" node
    /// with empty content and forgetting the failed call.
    ProviderCallFailed {
        accrued_cost_usd: f64,
        reason: String,
    },
}

/// Map a terminal agent-loop run to the node outcome for the engine.
///
/// - A budget-dispatch rejection (note carries the `"workflow budget dispatch"`
///   token) is surfaced as a stop-the-workflow rejection on every path.
/// - On a **capped** node, a `Failed` run — a started provider call that
///   errored — is surfaced as a recorded failure carrying the run's actual
///   accrued spend. Fail-closed + evidence: the workflow node fails instead of
///   silently completing with an empty answer, and the spend accrued before the
///   failure is preserved in the returned message.
/// - Every other terminal run (Completed, or a budget-control Incomplete stop —
///   `BudgetExhausted` / `BudgetBreach` / `MaxTurns` / runaway) yields the node
///   output from the partial transcript, exactly as before.
/// - **Uncapped nodes keep legacy behavior byte-for-byte**: a failed call still
///   produces a (partial/empty) node output rather than failing the workflow,
///   so ordinary uncapped workflow runs are unchanged.
fn terminal_run_to_node_outcome(run: agent_run::Run, capped: bool) -> NodeRunOutcome {
    if let Some(message) = workflow_budget_dispatch_failure(run.status, run.note.as_deref()) {
        return NodeRunOutcome::BudgetDispatchRejected(message);
    }
    if capped && run.status == RunStatus::Failed {
        return NodeRunOutcome::ProviderCallFailed {
            accrued_cost_usd: run.usage.cost_usd,
            reason: run
                .note
                .unwrap_or_else(|| "provider call failed".to_string()),
        };
    }
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

    // `model_used`: not tracked on `Run` in W1a (the request_logs row carries
    // the per-turn model; surfacing it here is deferred to W2).
    NodeRunOutcome::Output(NodeOutput {
        content: json!(last_text),
        cost_usd: run.usage.cost_usd,
        baseline_cost_usd: run.usage.baseline_cost_usd,
        model_used: None,
        doc_vision_saved_est_usd: 0.0,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        routes::agent_run::{Run, RunUsage},
        workflow::types::ModelSelection,
    };

    // ---- Make workflow spend truly bounded: failed-call recording ---------

    /// A terminal loop run produced by a fake agent-loop outcome. `messages`
    /// carries the partial transcript; `usage` the accrued cost so far.
    fn run_with(status: RunStatus, cost_usd: f64, note: Option<&str>) -> Run {
        Run {
            id: uuid::Uuid::nil(),
            status,
            messages: vec![Message::Assistant {
                content: Some(MessageContent::Text("partial answer".into())),
                tool_calls: vec![],
                name: None,
            }],
            turns: 1,
            usage: RunUsage {
                cost_usd,
                ..Default::default()
            },
            note: note.map(str::to_string),
            summarizer_tax_usd: None,
            stop_reason: None,
        }
    }

    /// A STARTED provider call that fails on a CAPPED node must be surfaced as
    /// a recorded failure carrying the run's real accrued spend — the workflow
    /// node journal then records the failed call (evidence) instead of silently
    /// reporting a "completed" node with empty content.
    #[test]
    fn capped_failed_run_surfaces_provider_call_with_accrued_spend() {
        let outcome = terminal_run_to_node_outcome(
            run_with(
                RunStatus::Failed,
                0.12,
                Some("turn 0 failed: upstream 500: login_expired"),
            ),
            /* capped */ true,
        );
        match outcome {
            NodeRunOutcome::ProviderCallFailed {
                accrued_cost_usd,
                reason,
            } => {
                assert!(
                    (accrued_cost_usd - 0.12).abs() < 1e-9,
                    "accrued spend before the failed call is preserved"
                );
                assert!(reason.contains("turn 0 failed"));
            }
            other => panic!("expected ProviderCallFailed, got {other:?}"),
        }
    }

    /// Uncapped nodes keep legacy behavior byte-for-byte: a failed provider
    /// call still yields a node output from the partial transcript (never a
    /// workflow failure) — ordinary uncapped workflow runs are unchanged.
    #[test]
    fn uncapped_failed_run_keeps_legacy_node_output_behavior() {
        let outcome = terminal_run_to_node_outcome(
            run_with(RunStatus::Failed, 0.12, Some("turn 0 failed: upstream 500")),
            /* capped */ false,
        );
        match outcome {
            NodeRunOutcome::Output(node_output) => {
                assert_eq!(node_output.content, serde_json::json!("partial answer"));
                assert!((node_output.cost_usd - 0.12).abs() < 1e-9);
            }
            other => panic!("expected Output, got {other:?}"),
        }
    }

    /// A budget-dispatch rejection (the run note carries the
    /// `"workflow budget dispatch"` token) must stop the workflow on capped AND
    /// uncapped nodes alike — unchanged propagation semantics.
    #[test]
    fn budget_dispatch_rejection_propagates_on_every_node() {
        for capped in [true, false] {
            let outcome = terminal_run_to_node_outcome(
                run_with(
                    RunStatus::Failed,
                    0.0,
                    Some("turn 0 failed: invalid request: workflow budget dispatch rejected the final routed request before provider work"),
                ),
                capped,
            );
            match outcome {
                NodeRunOutcome::BudgetDispatchRejected(message) => {
                    assert!(message.contains("workflow budget dispatch"));
                }
                other => panic!("expected BudgetDispatchRejected, got {other:?}"),
            }
        }
    }

    /// A capped node that stops on a budget-control signal (BudgetExhausted /
    /// BudgetBreach / MaxTurns) still yields its partial output — the breach is
    /// recorded on the run's stop_reason/note, and the engine's own run-level
    /// cap stops later nodes. Failing the whole workflow for a control stop
    /// would be over-aggressive.
    #[test]
    fn capped_budget_control_stop_still_produces_node_output() {
        let outcome = terminal_run_to_node_outcome(
            run_with(
                RunStatus::Incomplete,
                0.50,
                Some("run cost cap $0.40 breached"),
            ),
            /* capped */ true,
        );
        assert!(
            matches!(outcome, NodeRunOutcome::Output(_)),
            "budget-control Incomplete runs map to Output, not a workflow failure"
        );
    }

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
            doc_vision_saved_est_usd: 0.0,
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

    #[test]
    fn reservation_requires_the_exact_priceable_single_turn_shape() {
        let mut spec = IntelligenceSpec {
            selection: ModelSelection::Model {
                model: "gpt-4o-mini".into(),
            },
            prompt: "hello".into(),
            tools: vec![],
            max_turns: 1,
            max_output_tokens: Some(64),
            max_cost_usd: None,
        };

        assert!(reservation_cost_usd(&spec).is_some_and(|cost| cost > 0.0));
        spec.max_output_tokens = None;
        assert_eq!(reservation_cost_usd(&spec), None);
        spec.max_output_tokens = Some(64);
        spec.max_turns = 2;
        assert_eq!(reservation_cost_usd(&spec), None);
        spec.max_turns = 1;
        spec.selection = ModelSelection::Route {
            route_ref: "dynamic".into(),
        };
        assert_eq!(reservation_cost_usd(&spec), None);
    }

    #[test]
    fn post_route_budget_rejection_propagates_out_of_the_agent_loop() {
        assert_eq!(
            workflow_budget_dispatch_failure(
                RunStatus::Failed,
                Some(
                    "turn 0 failed: invalid request: workflow budget dispatch rejected the final routed request before provider work"
                ),
            ),
            Some(
                "turn 0 failed: invalid request: workflow budget dispatch rejected the final routed request before provider work"
                    .to_string()
            )
        );
        assert_eq!(
            workflow_budget_dispatch_failure(
                RunStatus::Failed,
                Some("turn 0 failed: provider unavailable"),
            ),
            None,
            "existing non-budget agent-loop failures retain their accounting behavior"
        );
    }
}
