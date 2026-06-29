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
//!   node outputs are available before the run).

use crate::routes::agent_run_budget::estimate_next_turn_cost;
use crate::workflow::types::{ModelSelection, NodeKind, WorkflowDefinition};
use tt_shared::messages::{Message, MessageContent};

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
    let mut per_node: Vec<NodeEstimate> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut projected_cost_usd = 0.0f64;

    for node in &def.nodes {
        let (selection, prompt) = match &node.kind {
            NodeKind::Model {
                selection, prompt, ..
            } => (selection, prompt.as_str()),
            NodeKind::Agent {
                selection, prompt, ..
            } => (selection, prompt.as_str()),
            // Trigger, Transform, Branch, Output → no model cost.
            _ => continue,
        };

        match selection {
            ModelSelection::Model { model } => {
                let subst = substitute_input(prompt, inputs);
                let msg = Message::User {
                    content: MessageContent::Text(subst),
                    name: None,
                };
                match estimate_next_turn_cost(model, &[msg]) {
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
fn substitute_input(template: &str, inputs: &serde_json::Value) -> String {
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

// ---------------------------------------------------------------------------
// Tests (written first — TDD)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::workflow::types::{
        BudgetPolicy, Edge, ModelSelection, Node, NodeKind, WorkflowDefinition,
    };

    // ---- helpers -----------------------------------------------------------

    /// T → m1 (gpt-4o-mini) → m2 (gpt-4o-mini) → o
    fn pinned_two_model_def() -> WorkflowDefinition {
        WorkflowDefinition {
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
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "m2".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "gpt-4o-mini".into(),
                        },
                        prompt: "Translate: {{m1}}".into(),
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
        }
    }

    /// T → m_auto (Auto) → o
    fn auto_selection_def() -> WorkflowDefinition {
        WorkflowDefinition {
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
        }
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
    fn substitute_input_null_inputs_is_empty_string() {
        let result = substitute_input("{{input}}", &serde_json::Value::Null);
        assert_eq!(result, "");
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
