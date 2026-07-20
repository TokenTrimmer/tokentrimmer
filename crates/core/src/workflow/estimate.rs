//! Pre-run cost projection for a [`WorkflowDefinition`] (W1a Task 7).
//!
//! `estimate_workflow` walks all nodes in definition order and, for each
//! Model/Agent node with a pinned model, calls `tt_preview::preview` (via the
//! `estimate_next_turn_cost` wrapper) to project the per-node served cost.
//!
//! # Limitations (W1a MVP)
//! - Linear sum across nodes: loop/branch cardinality is NOT weighted.  A
//!   Branch workflow reports the cost of ALL branches, not just one arm.
//! - Route / Auto selections cannot be resolved statically; those nodes
//!   contribute `None` + a warning.
//! - Only `{{input}}` is substituted into prompts at estimate time (no prior
//!   node outputs are available before the run). Environment-bound estimates
//!   also substitute their exact accepted `{{variables.NAME}}` snapshot.

use std::collections::BTreeMap;

use crate::routes::agent_run_budget::estimate_next_turn_cost;
use crate::workflow::types::{ModelSelection, NodeKind, WorkflowDefinition};
use tt_shared::messages::{Message, MessageContent};

// ---------------------------------------------------------------------------
// Budget admission
// ---------------------------------------------------------------------------

/// A static workflow projection cannot support the requested budget admission.
///
/// This is deliberately an *admission estimate*, not a reservation or a
/// runtime spending guarantee. The executor still has to enforce its own
/// accrued-cost boundary while nodes run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WorkflowBudgetAdmissionError {
    /// A non-finite or negative cap cannot safely protect a caller.
    InvalidCap,
    /// A capped run needs a declared completion ceiling for every direct
    /// intelligence node before it can be projected safely.
    UnboundedOutputNodes { node_ids: Vec<String> },
    /// A capped run cannot project a prompt that consumes a prior node output
    /// or any other runtime template value.
    DynamicPromptNodes { node_ids: Vec<String> },
    /// The preview engine does not include tool schemas or gateway-tool work in
    /// its node price, so a tool-bearing agent cannot be reserved honestly.
    ToolBearingAgentNodes { node_ids: Vec<String> },
    /// At least one node cannot be safely priced before dispatch.
    UnpriceableNodes { node_ids: Vec<String> },
    /// The aggregate estimate itself is not a finite currency amount.
    InvalidProjection,
    /// The admitted static projection is already above the requested cap.
    ProjectedCostExceeds {
        projected_cost_usd: f64,
        max_cost_usd: f64,
    },
}

impl std::fmt::Display for WorkflowBudgetAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCap => {
                f.write_str("workflow max_cost_usd must be a finite non-negative value")
            }
            Self::UnboundedOutputNodes { node_ids } => write!(
                f,
                "workflow budget preflight requires explicit positive max_output_tokens for node(s): {}",
                node_ids.join(", ")
            ),
            Self::DynamicPromptNodes { node_ids } => write!(
                f,
                "workflow budget preflight supports only {{{{input}}}} and accepted {{{{variables.NAME}}}} prompt references; node(s) use other template references: {}",
                node_ids.join(", ")
            ),
            Self::ToolBearingAgentNodes { node_ids } => write!(
                f,
                "workflow budget preflight cannot reserve tool-bearing agent node(s): {}",
                node_ids.join(", ")
            ),
            Self::UnpriceableNodes { node_ids } => write!(
                f,
                "workflow budget preflight cannot price node(s): {}",
                node_ids.join(", ")
            ),
            Self::InvalidProjection => {
                f.write_str("workflow budget preflight produced an invalid cost projection")
            }
            Self::ProjectedCostExceeds {
                projected_cost_usd,
                max_cost_usd,
            } => write!(
                f,
                "workflow projects ${projected_cost_usd:.4}, exceeds budget admission estimate ${max_cost_usd:.4}"
            ),
        }
    }
}

/// Reject a budgeted run before any run record or provider dispatch when the
/// current static estimator cannot price every bounded direct execution path,
/// or when its projection is already above the cap.
///
/// A missing cap retains the legacy execution behavior. A successful result
/// only means the present estimator could admit the requested static graph; it
/// does **not** reserve provider spend or make a runtime ceiling claim.
pub(crate) fn admit_budgeted_workflow(
    def: &WorkflowDefinition,
    inputs: &serde_json::Value,
    max_cost_usd: Option<f64>,
) -> Result<(), WorkflowBudgetAdmissionError> {
    admit_budgeted_workflow_with_variables(def, inputs, &BTreeMap::new(), max_cost_usd)
}

pub(crate) fn admit_budgeted_workflow_with_variables(
    def: &WorkflowDefinition,
    inputs: &serde_json::Value,
    variables: &BTreeMap<String, String>,
    max_cost_usd: Option<f64>,
) -> Result<(), WorkflowBudgetAdmissionError> {
    let Some(max_cost_usd) = max_cost_usd else {
        return Ok(());
    };
    if !max_cost_usd.is_finite() || max_cost_usd < 0.0 {
        return Err(WorkflowBudgetAdmissionError::InvalidCap);
    }

    // An omitted output limit preserves the legacy provider-default behavior
    // for uncapped runs. Once a run asks for budget admission, however, every
    // direct intelligence node must declare a positive provider completion
    // ceiling. This is intentionally an admission precondition, not a claim
    // that a provider will settle at or below this cost projection.
    let unbounded_output_nodes: Vec<String> = def
        .nodes
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::Model {
                max_output_tokens, ..
            }
            | NodeKind::Agent {
                max_output_tokens, ..
            } if !matches!(max_output_tokens, Some(value) if *value > 0) => Some(node.id.clone()),
            _ => None,
        })
        .collect();
    if !unbounded_output_nodes.is_empty() {
        return Err(WorkflowBudgetAdmissionError::UnboundedOutputNodes {
            node_ids: unbounded_output_nodes,
        });
    }

    // `estimate_workflow` intentionally has no prior node outputs to
    // substitute. Capped admission therefore permits only the exact input
    // placeholder that it can resolve deterministically; all other closed
    // template references would make its cost projection undercount the real
    // prompt. An unclosed `{{` remains a literal, matching engine behavior.
    let dynamic_prompt_nodes: Vec<String> = def
        .nodes
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::Model { prompt, .. } | NodeKind::Agent { prompt, .. }
                if has_dynamic_template_ref(prompt, variables) =>
            {
                Some(node.id.clone())
            }
            _ => None,
        })
        .collect();
    if !dynamic_prompt_nodes.is_empty() {
        return Err(WorkflowBudgetAdmissionError::DynamicPromptNodes {
            node_ids: dynamic_prompt_nodes,
        });
    }

    // `tt-preview` presently prices the message text plus completion ceiling,
    // not the serialized tool definitions or any gateway-tool work. Admitting
    // those nodes under a numeric cap would create a reservation known to be
    // incomplete. Uncapped execution retains the existing agent behavior.
    let tool_bearing_agent_nodes: Vec<String> = def
        .nodes
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::Agent { tools, .. } if !tools.is_empty() => Some(node.id.clone()),
            _ => None,
        })
        .collect();
    if !tool_bearing_agent_nodes.is_empty() {
        return Err(WorkflowBudgetAdmissionError::ToolBearingAgentNodes {
            node_ids: tool_bearing_agent_nodes,
        });
    }

    // The current estimator walks direct Model/Agent nodes once. A nested
    // workflow or loop hides its future graph, and a multi-turn agent can
    // dispatch more than the single turn this estimator prices. With a cap,
    // treating any of those as a complete projection would weaken the
    // fail-closed admission boundary. Keep uncapped legacy execution intact;
    // provider-hard settlement remains a separate runtime concern.
    let mut node_ids: Vec<String> = def
        .nodes
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::SubWorkflow { .. } | NodeKind::Loop { .. } => Some(node.id.clone()),
            NodeKind::Agent { max_turns, .. } if *max_turns != Some(1) => Some(node.id.clone()),
            _ => None,
        })
        .collect();

    let estimate = estimate_workflow_with_variables(def, inputs, variables);
    for node_id in estimate
        .per_node
        .iter()
        .filter(|node| node.cost_usd.is_none())
        .map(|node| node.node_id.clone())
    {
        if !node_ids.contains(&node_id) {
            node_ids.push(node_id);
        }
    }
    if !node_ids.is_empty() {
        return Err(WorkflowBudgetAdmissionError::UnpriceableNodes { node_ids });
    }
    if !estimate.projected_cost_usd.is_finite() {
        return Err(WorkflowBudgetAdmissionError::InvalidProjection);
    }
    if estimate.projected_cost_usd > max_cost_usd {
        return Err(WorkflowBudgetAdmissionError::ProjectedCostExceeds {
            projected_cost_usd: estimate.projected_cost_usd,
            max_cost_usd,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Cost estimate for a single Model or Agent node.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NodeEstimate {
    pub node_id: String,
    /// The model id that would be used, if statically determinable.
    pub model: Option<String>,
    /// Projected cost for this node. `None` when the model is unknown/dynamic
    /// or not present in the pricing catalog.
    pub cost_usd: Option<f64>,
}

/// Top-level pre-run cost projection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowEstimate {
    /// Sum of all `Some(cost_usd)` per-node values (MVP: linear sum).
    pub projected_cost_usd: f64,
    /// One entry per Model/Agent node in definition order.
    pub per_node: Vec<NodeEstimate>,
    /// Human-readable warnings (e.g. un-projectable nodes).
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public API (stub — tests written first; implementation fills this in)
// ---------------------------------------------------------------------------

/// Compute a pre-run cost projection for `def` given workflow `inputs`.
///
/// For each Model / Agent node:
/// - `ModelSelection::Model { model }` → substitute `{{input}}` in the prompt,
///   call `estimate_next_turn_cost`, add to sum.
/// - `ModelSelection::Route` / `ModelSelection::Auto` → `cost_usd = None` +
///   a warning.
///
/// Trigger / Transform / Branch / Output nodes are skipped (zero model cost).
pub fn estimate_workflow(def: &WorkflowDefinition, inputs: &serde_json::Value) -> WorkflowEstimate {
    estimate_workflow_with_variables(def, inputs, &BTreeMap::new())
}

pub fn estimate_workflow_with_variables(
    def: &WorkflowDefinition,
    inputs: &serde_json::Value,
    variables: &BTreeMap<String, String>,
) -> WorkflowEstimate {
    let mut per_node: Vec<NodeEstimate> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut projected_cost_usd = 0.0f64;

    for node in &def.nodes {
        let (selection, prompt, max_output_tokens) = match &node.kind {
            NodeKind::Model {
                selection,
                prompt,
                max_output_tokens,
                ..
            } => (selection, prompt.as_str(), *max_output_tokens),
            NodeKind::Agent {
                selection,
                prompt,
                max_output_tokens,
                ..
            } => (selection, prompt.as_str(), *max_output_tokens),
            // Trigger, Transform, Branch, Output → no model cost.
            _ => continue,
        };

        match selection {
            ModelSelection::Model { model } => {
                let subst = substitute_static(prompt, inputs, variables);
                let msg = Message::User {
                    content: MessageContent::Text(subst),
                    name: None,
                };
                match estimate_next_turn_cost(model, &[msg], max_output_tokens) {
                    Some(c) => {
                        projected_cost_usd += c;
                        per_node.push(NodeEstimate {
                            node_id: node.id.clone(),
                            model: Some(model.clone()),
                            cost_usd: Some(c),
                        });
                    }
                    None => {
                        warnings.push(format!(
                            "node {}: model {} not in pricing catalog",
                            node.id, model
                        ));
                        per_node.push(NodeEstimate {
                            node_id: node.id.clone(),
                            model: Some(model.clone()),
                            cost_usd: None,
                        });
                    }
                }
            }
            ModelSelection::Route { .. } | ModelSelection::Auto => {
                warnings.push(format!(
                    "node {}: cost not projected (route/auto selection)",
                    node.id
                ));
                per_node.push(NodeEstimate {
                    node_id: node.id.clone(),
                    model: None,
                    cost_usd: None,
                });
            }
        }
    }

    WorkflowEstimate {
        projected_cost_usd,
        per_node,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Template substitution (minimal, estimation-time only)
// ---------------------------------------------------------------------------

/// Replace `{{input}}` with the string representation of `inputs`.
/// All other `{{ref}}` tokens resolve to `""` (no prior outputs available at
/// estimate time).
#[cfg(test)]
fn substitute_input(template: &str, inputs: &serde_json::Value) -> String {
    substitute_static(template, inputs, &BTreeMap::new())
}

fn substitute_static(
    template: &str,
    inputs: &serde_json::Value,
    variables: &BTreeMap<String, String>,
) -> String {
    let input_str = match inputs {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    };

    let mut result = String::with_capacity(template.len() + 16);
    let mut remaining = template;

    while let Some(open) = remaining.find("{{") {
        result.push_str(&remaining[..open]);
        remaining = &remaining[open + 2..];

        if let Some(close) = remaining.find("}}") {
            let ref_str = remaining[..close].trim();
            let resolved = if ref_str == "input" {
                input_str.clone()
            } else if let Some(name) = ref_str.strip_prefix("variables.") {
                variables.get(name).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            result.push_str(&resolved);
            remaining = &remaining[close + 2..];
        } else {
            // Unclosed `{{` — emit as-is and stop.
            result.push_str("{{");
            break;
        }
    }
    result.push_str(remaining);
    result
}

/// Return whether `template` contains a complete `{{...}}` placeholder other
/// than the sole supported static input reference. This deliberately mirrors
/// [`substitute_input`]'s minimal scanner: an unclosed opener is literal text,
/// and whitespace around `input` is accepted by both paths.
#[cfg(test)]
fn has_non_input_template_ref(template: &str) -> bool {
    has_dynamic_template_ref(template, &BTreeMap::new())
}

fn has_dynamic_template_ref(template: &str, variables: &BTreeMap<String, String>) -> bool {
    let mut remaining = template;
    while let Some(open) = remaining.find("{{") {
        remaining = &remaining[open + 2..];
        let Some(close) = remaining.find("}}") else {
            return false;
        };
        let reference = remaining[..close].trim();
        if reference != "input"
            && !reference
                .strip_prefix("variables.")
                .is_some_and(|name| variables.contains_key(name))
        {
            return true;
        }
        remaining = &remaining[close + 2..];
    }
    false
}

// ---------------------------------------------------------------------------
// Tests (written first — TDD)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tt_shared::messages::{Tool, ToolFunction};
    use uuid::Uuid;

    use super::*;
    use crate::workflow::types::{
        BudgetPolicy, Edge, ModelSelection, Node, NodeKind, WorkflowDefinition,
    };

    // ---- helpers -----------------------------------------------------------

    /// T → m1 (gpt-4o-mini) → m2 (gpt-4o-mini) → o
    fn pinned_two_model_def() -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "estimate_test".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "gpt-4o-mini".into(),
                        },
                        prompt: "Summarize: {{input}}".into(),
                        max_output_tokens: Some(64),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "m2".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "gpt-4o-mini".into(),
                        },
                        prompt: "Translate this input: {{input}}".into(),
                        max_output_tokens: Some(64),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "m2".into(),
                    map: None,
                },
                Edge {
                    from: "m2".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    /// T → m_auto (Auto) → o
    fn auto_selection_def() -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "auto_test".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m_auto".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Auto,
                        prompt: "Do something: {{input}}".into(),
                        max_output_tokens: Some(64),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m_auto".into(),
                    map: None,
                },
                Edge {
                    from: "m_auto".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    fn def_with_first_model_replaced(kind: NodeKind) -> WorkflowDefinition {
        let mut def = pinned_two_model_def();
        def.nodes[1].kind = kind;
        def
    }

    // ---- tests -------------------------------------------------------------

    /// Two pinned gpt-4o-mini nodes → projected_cost_usd > 0, both per_node
    /// entries have Some(cost_usd), no warnings.
    #[test]
    fn estimate_pinned_two_nodes_projects_positive_cost() {
        let def = pinned_two_model_def();
        let est = estimate_workflow(&def, &json!("hello world"));

        assert!(
            est.projected_cost_usd > 0.0,
            "expected positive total cost, got {}",
            est.projected_cost_usd
        );
        // Only the two Model nodes should appear in per_node (Trigger + Output skipped).
        assert_eq!(est.per_node.len(), 2, "expected 2 per_node entries");
        assert!(est.per_node[0].cost_usd.is_some(), "m1 cost must be Some");
        assert!(est.per_node[1].cost_usd.is_some(), "m2 cost must be Some");
        assert!(
            est.warnings.is_empty(),
            "no warnings expected for pinned catalog model"
        );
    }

    /// Auto-selection node → cost_usd is None and a warning is present.
    #[test]
    fn estimate_auto_node_is_none_with_warning() {
        let def = auto_selection_def();
        let est = estimate_workflow(&def, &json!("test"));

        assert_eq!(est.per_node.len(), 1);
        assert!(
            est.per_node[0].cost_usd.is_none(),
            "Auto node must have None cost"
        );
        assert!(!est.warnings.is_empty(), "Auto node must emit a warning");
        assert!(
            est.warnings[0].contains("m_auto"),
            "warning must mention the node id; got: {:?}",
            est.warnings
        );
    }

    /// Route-selection node → cost_usd is None and a warning is present.
    #[test]
    fn estimate_route_node_is_none_with_warning() {
        let def = WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "route_test".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m_route".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Route {
                            route_ref: "my-route".into(),
                        },
                        prompt: "Route me: {{input}}".into(),
                        max_output_tokens: Some(64),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m_route".into(),
                    map: None,
                },
                Edge {
                    from: "m_route".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };
        let est = estimate_workflow(&def, &json!("test"));

        assert_eq!(est.per_node.len(), 1);
        assert!(est.per_node[0].cost_usd.is_none());
        assert!(!est.warnings.is_empty());
        assert!(
            est.warnings[0].contains("m_route"),
            "warning must mention node id; got: {:?}",
            est.warnings
        );
    }

    // ---- budget admission --------------------------------------------------

    #[test]
    fn budget_admission_keeps_uncapped_legacy_execution_permissive() {
        let def = auto_selection_def();

        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), None),
            Ok(()),
            "a missing cap must not turn the estimate endpoint's unknown-model warning into a new execution rejection"
        );
    }

    #[test]
    fn budget_admission_requires_explicit_output_caps_for_every_intelligence_node() {
        let mut def = pinned_two_model_def();
        if let NodeKind::Model {
            max_output_tokens, ..
        } = &mut def.nodes[1].kind
        {
            *max_output_tokens = None;
        }

        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), Some(1.0)),
            Err(WorkflowBudgetAdmissionError::UnboundedOutputNodes {
                node_ids: vec!["m1".to_string()],
            }),
            "the admission gate must fail before a run record or provider dispatch"
        );
        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), None),
            Ok(()),
            "omitted caps retain the legacy uncapped execution contract"
        );
    }

    #[test]
    fn budget_admission_rejects_upstream_template_references_without_dispatch() {
        let mut def = pinned_two_model_def();
        if let NodeKind::Model { prompt, .. } = &mut def.nodes[2].kind {
            *prompt = "Translate: {{m1}}".into();
        }

        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), Some(1.0)),
            Err(WorkflowBudgetAdmissionError::DynamicPromptNodes {
                node_ids: vec!["m2".to_string()],
            }),
            "the static estimator cannot substitute node outputs safely"
        );
        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), None),
            Ok(()),
            "upstream references retain legacy behavior when no run cap is requested"
        );
    }

    #[test]
    fn budget_admission_fails_closed_when_a_capped_run_has_an_unpriceable_node() {
        let def = auto_selection_def();

        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), Some(1.0)),
            Err(WorkflowBudgetAdmissionError::UnpriceableNodes {
                node_ids: vec!["m_auto".to_string()],
            })
        );
    }

    #[test]
    fn budget_admission_fails_closed_for_capped_execution_shapes_the_estimator_cannot_cover() {
        let cases = vec![
            (
                "nested workflow",
                NodeKind::SubWorkflow {
                    workflow_id: Uuid::from_u128(17),
                    version: None,
                },
            ),
            (
                "loop",
                NodeKind::Loop {
                    body_workflow_id: Uuid::from_u128(18),
                    cond: "{{input}}".into(),
                    max_iters: 2,
                },
            ),
            (
                "multi-turn agent",
                NodeKind::Agent {
                    selection: ModelSelection::Model {
                        model: "gpt-4o-mini".into(),
                    },
                    prompt: "Analyze: {{input}}".into(),
                    max_turns: Some(2),
                    max_output_tokens: Some(64),
                    max_cost_usd: None,
                    tools: vec![],
                },
            ),
        ];

        for (name, kind) in cases {
            let def = def_with_first_model_replaced(kind);
            assert_eq!(
                admit_budgeted_workflow(&def, &json!("test"), Some(1.0)),
                Err(WorkflowBudgetAdmissionError::UnpriceableNodes {
                    node_ids: vec!["m1".to_string()],
                }),
                "{name} must not be admitted under a static cap"
            );
            assert_eq!(
                admit_budgeted_workflow(&def, &json!("test"), None),
                Ok(()),
                "{name} retains uncapped legacy execution behavior"
            );
        }
    }

    #[test]
    fn budget_admission_still_prices_a_single_turn_agent() {
        let def = def_with_first_model_replaced(NodeKind::Agent {
            selection: ModelSelection::Model {
                model: "gpt-4o-mini".into(),
            },
            prompt: "Analyze: {{input}}".into(),
            max_turns: Some(1),
            max_output_tokens: Some(64),
            max_cost_usd: None,
            tools: vec![],
        });
        let inputs = json!("test");
        let projected = estimate_workflow(&def, &inputs).projected_cost_usd;

        assert!(projected > 0.0, "fixture must be priced");
        assert_eq!(
            admit_budgeted_workflow(&def, &inputs, Some(projected)),
            Ok(())
        );
    }

    #[test]
    fn budget_admission_rejects_tool_schemas_the_preview_does_not_price() {
        let def = def_with_first_model_replaced(NodeKind::Agent {
            selection: ModelSelection::Model {
                model: "gpt-4o-mini".into(),
            },
            prompt: "Analyze: {{input}}".into(),
            max_turns: Some(1),
            max_output_tokens: Some(64),
            max_cost_usd: None,
            tools: vec![Tool {
                r#type: "function".into(),
                function: ToolFunction {
                    name: "lookup".into(),
                    description: Some("Look up a value".into()),
                    parameters: json!({"type": "object"}),
                },
            }],
        });

        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), Some(1.0)),
            Err(WorkflowBudgetAdmissionError::ToolBearingAgentNodes {
                node_ids: vec!["m1".to_string()],
            })
        );
        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), None),
            Ok(()),
            "uncapped tool-bearing agents retain their existing behavior"
        );
    }

    #[test]
    fn budget_admission_rejects_a_projection_already_above_the_cap() {
        let def = pinned_two_model_def();
        let inputs = json!("test");
        let projected = estimate_workflow(&def, &inputs).projected_cost_usd;
        assert!(
            projected > 0.0,
            "fixture must be priced for this admission test"
        );

        assert_eq!(
            admit_budgeted_workflow(&def, &inputs, Some(projected / 2.0)),
            Err(WorkflowBudgetAdmissionError::ProjectedCostExceeds {
                projected_cost_usd: projected,
                max_cost_usd: projected / 2.0,
            })
        );
        assert_eq!(
            admit_budgeted_workflow(&def, &inputs, Some(projected)),
            Ok(()),
            "the static preflight preserves the existing strictly-over-cap boundary"
        );
    }

    #[test]
    fn budget_admission_rejects_invalid_caps_before_any_dispatch_path_can_use_them() {
        let def = pinned_two_model_def();

        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), Some(-0.01)),
            Err(WorkflowBudgetAdmissionError::InvalidCap)
        );
        assert_eq!(
            admit_budgeted_workflow(&def, &json!("test"), Some(f64::NAN)),
            Err(WorkflowBudgetAdmissionError::InvalidCap)
        );
    }

    #[test]
    fn estimate_uses_the_declared_output_cap() {
        let capped = pinned_two_model_def();
        let mut legacy_default = capped.clone();
        if let NodeKind::Model {
            max_output_tokens, ..
        } = &mut legacy_default.nodes[1].kind
        {
            *max_output_tokens = None;
        }
        if let NodeKind::Model {
            max_output_tokens, ..
        } = &mut legacy_default.nodes[2].kind
        {
            *max_output_tokens = None;
        }

        let input = json!("a moderately sized prompt for output estimation");
        let capped_cost = estimate_workflow(&capped, &input).projected_cost_usd;
        let default_cost = estimate_workflow(&legacy_default, &input).projected_cost_usd;
        assert!(
            capped_cost < default_cost,
            "an explicit 64-token cap must replace the preview default: {capped_cost} < {default_cost}"
        );
    }

    // ---- unit tests for substitute_input -----------------------------------

    #[test]
    fn substitute_input_replaces_input_placeholder() {
        let result = substitute_input("Hello {{input}}!", &json!("world"));
        assert_eq!(result, "Hello world!");
    }

    #[test]
    fn substitute_input_unknown_ref_is_empty() {
        let result = substitute_input("{{other_node}} text", &json!("x"));
        assert_eq!(result, " text");
    }

    #[test]
    fn environment_variables_are_static_for_estimation_and_budget_admission() {
        let variables = BTreeMap::from([("REGION".into(), "us-east".into())]);
        assert_eq!(
            substitute_static(
                "{{input}} in {{variables.REGION}}",
                &json!("deploy"),
                &variables,
            ),
            "deploy in us-east"
        );
        assert!(!has_dynamic_template_ref(
            "{{input}} {{variables.REGION}}",
            &variables,
        ));
        assert!(has_dynamic_template_ref(
            "{{variables.MISSING}}",
            &variables,
        ));
    }

    #[test]
    fn substitute_input_null_inputs_is_empty_string() {
        let result = substitute_input("{{input}}", &serde_json::Value::Null);
        assert_eq!(result, "");
    }

    #[test]
    fn capped_admission_template_scan_only_allows_input() {
        assert!(!has_non_input_template_ref("{{input}} and {{ input }}"));
        assert!(has_non_input_template_ref("{{m1}}"));
        assert!(has_non_input_template_ref("{{input.user}}"));
        assert!(has_non_input_template_ref("{{ }}"));
        assert!(
            !has_non_input_template_ref("literal unclosed {{m1"),
            "unclosed braces are literal text in the engine and estimator"
        );
    }

    // ---- serde round-trip --------------------------------------------------

    /// WorkflowEstimate + NodeEstimate serialize and deserialize successfully.
    /// Confirms that the pub + Serialize/Deserialize derives work end-to-end so
    /// the CLI can dump/read estimate JSON.
    #[test]
    fn workflow_estimate_serializes() {
        let def = pinned_two_model_def();
        let est = estimate_workflow(&def, &json!("hello"));

        // Serialize to JSON string.
        let json_str = serde_json::to_string(&est).expect("WorkflowEstimate must serialize");

        // Required top-level fields are present.
        assert!(
            json_str.contains("projected_cost_usd"),
            "expected 'projected_cost_usd' in JSON; got: {json_str}"
        );
        assert!(
            json_str.contains("per_node"),
            "expected 'per_node' in JSON; got: {json_str}"
        );
        assert!(
            json_str.contains("warnings"),
            "expected 'warnings' in JSON; got: {json_str}"
        );

        // Round-trip: deserialize back and check equality.
        let round_tripped: WorkflowEstimate =
            serde_json::from_str(&json_str).expect("WorkflowEstimate must deserialize");

        assert!(
            (round_tripped.projected_cost_usd - est.projected_cost_usd).abs() < 1e-12,
            "projected_cost_usd round-trip mismatch"
        );
        assert_eq!(
            round_tripped.per_node.len(),
            est.per_node.len(),
            "per_node length round-trip mismatch"
        );
        assert_eq!(
            round_tripped.per_node[0].node_id, est.per_node[0].node_id,
            "per_node[0].node_id round-trip mismatch"
        );
        assert_eq!(
            round_tripped.warnings, est.warnings,
            "warnings round-trip mismatch"
        );
    }
}
