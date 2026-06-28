//! Workflow DAG + model-resolution validation.
//!
//! `validate` is intentionally **pure**: it takes a `model_exists` closure
//! instead of a `&ProviderRegistry` so callers can stub it trivially in unit
//! tests.  Production callers pass `|m| state.registry.resolve(m).is_some()`.
//!
//! All errors are collected before returning so callers see the full picture.

use std::collections::{HashMap, HashSet, VecDeque};

use super::types::{ModelSelection, NodeKind, WorkflowDefinition};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Validate a [`WorkflowDefinition`] against structural and model constraints.
///
/// `model_exists(model_id)` should return `true` when the given model id is
/// known to the gateway (i.e. `registry.resolve(model_id).is_some()`).  Only
/// [`ModelSelection::Model`] nodes are checked; `Route` and `Auto` selections
/// are accepted without a model lookup.
///
/// Returns `Ok(())` when the definition is valid, or `Err(errors)` where
/// `errors` is a **complete** list of all violations found.
pub fn validate(
    def: &WorkflowDefinition,
    model_exists: &dyn Fn(&str) -> bool,
) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    // Build a set of all declared node ids for O(1) lookup.
    let node_ids: HashSet<&str> = def.nodes.iter().map(|n| n.id.as_str()).collect();

    // ------------------------------------------------------------------
    // 1. Exactly one Trigger node.
    // ------------------------------------------------------------------
    let trigger_count = def
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Trigger))
        .count();
    if trigger_count == 0 {
        errors.push("workflow must have exactly one Trigger node (found 0)".to_string());
    } else if trigger_count > 1 {
        errors.push(format!(
            "workflow must have exactly one Trigger node (found {trigger_count})"
        ));
    }

    // ------------------------------------------------------------------
    // 2. At least one Output node.
    // ------------------------------------------------------------------
    let output_count = def
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Output))
        .count();
    if output_count == 0 {
        errors.push("workflow must have at least one Output node".to_string());
    }

    // ------------------------------------------------------------------
    // 3. Edge endpoints reference existing node ids.
    // ------------------------------------------------------------------
    for edge in &def.edges {
        if !node_ids.contains(edge.from.as_str()) {
            errors.push(format!(
                "edge references unknown source node id \"{}\"",
                edge.from
            ));
        }
        if !node_ids.contains(edge.to.as_str()) {
            errors.push(format!(
                "edge references unknown target node id \"{}\"",
                edge.to
            ));
        }
    }

    // ------------------------------------------------------------------
    // 4. Branch when_true / when_false reference existing node ids.
    // ------------------------------------------------------------------
    for node in &def.nodes {
        if let NodeKind::Branch {
            when_true,
            when_false,
            ..
        } = &node.kind
        {
            if !node_ids.contains(when_true.as_str()) {
                errors.push(format!(
                    "Branch node \"{}\" when_true references unknown node id \"{}\"",
                    node.id, when_true
                ));
            }
            if !node_ids.contains(when_false.as_str()) {
                errors.push(format!(
                    "Branch node \"{}\" when_false references unknown node id \"{}\"",
                    node.id, when_false
                ));
            }
        }
    }

    // ------------------------------------------------------------------
    // 5. Model/Agent nodes with a pinned model id pass `model_exists`.
    // ------------------------------------------------------------------
    for node in &def.nodes {
        let selection = match &node.kind {
            NodeKind::Model { selection, .. } => Some(selection),
            NodeKind::Agent { selection, .. } => Some(selection),
            _ => None,
        };
        if let Some(ModelSelection::Model { model }) = selection {
            if !model_exists(model) {
                errors.push(format!(
                    "node \"{}\" references unknown model \"{}\"",
                    node.id, model
                ));
            }
        }
    }

    // ------------------------------------------------------------------
    // 6. No cycles — Kahn's algorithm (topological sort).
    //
    // The execution graph is the union of def.edges AND Branch arm targets:
    // a Branch node's when_true/when_false are outgoing edges the engine
    // will follow, so a cycle routed through a Branch arm is just as
    // infinite-loop-inducing as one through def.edges.  We treat both as
    // directed arcs in the adjacency map.
    // ------------------------------------------------------------------
    check_cycles(def, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ---------------------------------------------------------------------------
// Cycle detection via Kahn's algorithm
// ---------------------------------------------------------------------------

fn check_cycles(def: &WorkflowDefinition, errors: &mut Vec<String>) {
    // Build adjacency list and in-degree map over all declared nodes.
    let mut in_degree: HashMap<&str, usize> =
        def.nodes.iter().map(|n| (n.id.as_str(), 0usize)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = def
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), Vec::new()))
        .collect();

    for edge in &def.edges {
        // Skip edges that reference unknown nodes (already reported above).
        if let (Some(from_adj), Some(in_deg)) = (
            adj.get_mut(edge.from.as_str()),
            in_degree.get_mut(edge.to.as_str()),
        ) {
            from_adj.push(edge.to.as_str());
            *in_deg += 1;
        }
    }

    // Add Branch arm targets as directed edges (branch_id → when_true,
    // branch_id → when_false).  These are the arcs the engine follows at
    // runtime; omitting them makes cycles routed through a Branch arm
    // invisible to Kahn's algorithm.  Apply the same unknown-node-skip
    // safety as above.
    for node in &def.nodes {
        if let NodeKind::Branch {
            when_true,
            when_false,
            ..
        } = &node.kind
        {
            for target in [when_true.as_str(), when_false.as_str()] {
                // adj and in_degree are separate maps — simultaneous get_mut is fine.
                let has_from = adj.contains_key(node.id.as_str());
                let has_to = in_degree.contains_key(target);
                if has_from && has_to {
                    adj.get_mut(node.id.as_str()).unwrap().push(target);
                    *in_degree.get_mut(target).unwrap() += 1;
                }
            }
        }
    }

    // Kahn's: start with all zero-in-degree nodes.
    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut visited = 0usize;

    while let Some(node) = queue.pop_front() {
        visited += 1;
        if let Some(neighbors) = adj.get(node) {
            // Clone to avoid borrow-check issues while mutating `in_degree`.
            let neighbors: Vec<&str> = neighbors.clone();
            for &neighbor in &neighbors {
                if let Some(d) = in_degree.get_mut(neighbor) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(neighbor);
                    }
                }
            }
        }
    }

    if visited < def.nodes.len() {
        errors.push(format!(
            "workflow contains a cycle ({} of {} nodes unreachable via topological sort)",
            def.nodes.len() - visited,
            def.nodes.len()
        ));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::workflow::types::{
        BudgetPolicy, Edge, ModelSelection, Node, NodeKind, WorkflowDefinition,
    };

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn any_model(_: &str) -> bool {
        true
    }

    fn no_model(_: &str) -> bool {
        false
    }

    /// Minimal valid linear workflow: Trigger → Model → Output.
    fn linear_def(model: &str) -> WorkflowDefinition {
        WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "test".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: model.to_string(),
                        },
                        prompt: "hello".into(),
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
                    to: "m".into(),
                    map: None,
                },
                Edge {
                    from: "m".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        }
    }

    // ------------------------------------------------------------------
    // TDD tests — written BEFORE the implementation above.
    // ------------------------------------------------------------------

    /// A valid linear DAG must pass.
    #[test]
    fn valid_linear_def_ok() {
        let def = linear_def("gpt-4o");
        assert!(validate(&def, &|m| m == "gpt-4o").is_ok());
    }

    /// A Route selection skips the model-existence check.
    #[test]
    fn route_selection_skips_model_check() {
        let mut def = linear_def("ignored");
        if let NodeKind::Model { selection, .. } = &mut def.nodes[1].kind {
            *selection = ModelSelection::Route {
                route_ref: "my-route".into(),
            };
        }
        // Even with no_model, Route is accepted.
        assert!(validate(&def, &no_model).is_ok());
    }

    /// An Auto selection also skips the model-existence check.
    #[test]
    fn auto_selection_skips_model_check() {
        let mut def = linear_def("ignored");
        if let NodeKind::Model { selection, .. } = &mut def.nodes[1].kind {
            *selection = ModelSelection::Auto;
        }
        assert!(validate(&def, &no_model).is_ok());
    }

    /// An edge pointing at a missing node should produce an error naming that id.
    #[test]
    fn edge_to_missing_node_is_error() {
        let mut def = linear_def("gpt-4o");
        // Point the second edge at a non-existent node.
        def.edges[1].to = "missing_node".into();
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("missing_node"),
            "expected error to mention the unknown id; got: {combined}"
        );
    }

    /// An edge with a missing source should produce an error naming that id.
    #[test]
    fn edge_from_missing_node_is_error() {
        let mut def = linear_def("gpt-4o");
        def.edges[0].from = "ghost".into();
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("ghost"),
            "expected error to mention the unknown id; got: {combined}"
        );
    }

    /// A two-node cycle (a→b→a) must be reported with the word "cycle".
    #[test]
    fn two_node_cycle_is_error() {
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "cyclic".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "a".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
                    },
                },
                Node {
                    id: "b".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
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
                    to: "a".into(),
                    map: None,
                },
                Edge {
                    from: "a".into(),
                    to: "b".into(),
                    map: None,
                },
                // back-edge creates cycle
                Edge {
                    from: "b".into(),
                    to: "a".into(),
                    map: None,
                },
                Edge {
                    from: "b".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("cycle"),
            "expected error to mention cycle; got: {combined}"
        );
    }

    /// A self-loop (a→a) must also be caught.
    #[test]
    fn self_loop_is_cycle_error() {
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "self-loop".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "x".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
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
                    to: "x".into(),
                    map: None,
                },
                // self-loop
                Edge {
                    from: "x".into(),
                    to: "x".into(),
                    map: None,
                },
                Edge {
                    from: "x".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("cycle"),
            "expected self-loop to be reported as cycle; got: {combined}"
        );
    }

    /// A Model node with an unknown model id should be rejected.
    #[test]
    fn unknown_model_is_error() {
        let def = linear_def("definitely-not-a-model");
        let errs = validate(&def, &|m| m == "gpt-4o").unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("definitely-not-a-model"),
            "expected error to mention the bad model id; got: {combined}"
        );
    }

    /// An Agent node with a pinned unknown model is also rejected.
    #[test]
    fn unknown_model_in_agent_node_is_error() {
        let mut def = linear_def("gpt-4o");
        def.nodes[1] = Node {
            id: "m".into(),
            kind: NodeKind::Agent {
                selection: ModelSelection::Model {
                    model: "bad-agent-model".into(),
                },
                prompt: "go".into(),
                max_turns: None,
                max_cost_usd: None,
                tools: vec![],
            },
        };
        let errs = validate(&def, &|m| m == "gpt-4o").unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("bad-agent-model"),
            "expected error to mention the bad agent model; got: {combined}"
        );
    }

    /// Missing Trigger must be reported.
    #[test]
    fn missing_trigger_is_error() {
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "no-trigger".to_string(),
            nodes: vec![Node {
                id: "o".into(),
                kind: NodeKind::Output,
            }],
            edges: vec![],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("Trigger"),
            "expected error about missing Trigger; got: {combined}"
        );
    }

    /// Multiple Trigger nodes must be reported.
    #[test]
    fn multiple_triggers_is_error() {
        let mut def = linear_def("gpt-4o");
        def.nodes.push(Node {
            id: "t2".into(),
            kind: NodeKind::Trigger,
        });
        let errs = validate(&def, &|m| m == "gpt-4o").unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("Trigger"),
            "expected error about multiple Triggers; got: {combined}"
        );
    }

    /// Missing Output must be reported.
    #[test]
    fn missing_output_is_error() {
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "no-output".to_string(),
            nodes: vec![Node {
                id: "t".into(),
                kind: NodeKind::Trigger,
            }],
            edges: vec![],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("Output"),
            "expected error about missing Output; got: {combined}"
        );
    }

    /// Branch node whose when_true references a missing node must be reported.
    #[test]
    fn branch_when_true_missing_node_is_error() {
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "bad-branch".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "br".into(),
                    kind: NodeKind::Branch {
                        cond: ".score > 0.5".into(),
                        when_true: "missing_yes".into(),
                        when_false: "o".into(),
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
                    to: "br".into(),
                    map: None,
                },
                Edge {
                    from: "br".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("missing_yes"),
            "expected error about missing when_true node; got: {combined}"
        );
    }

    /// A converging diamond routed through a Branch MUST be valid (not a cycle).
    /// Topology: Trigger → Branch; Branch.when_true="a", when_false="b";
    /// edges: a→merge, b→merge, merge→o; plus Trigger→Branch edge.
    #[test]
    fn valid_branch_diamond_ok() {
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "diamond".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "br".into(),
                    kind: NodeKind::Branch {
                        cond: ".score > 0.5".into(),
                        when_true: "a".into(),
                        when_false: "b".into(),
                    },
                },
                Node {
                    id: "a".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
                    },
                },
                Node {
                    id: "b".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
                    },
                },
                Node {
                    id: "merge".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
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
                    to: "br".into(),
                    map: None,
                },
                Edge {
                    from: "a".into(),
                    to: "merge".into(),
                    map: None,
                },
                Edge {
                    from: "b".into(),
                    to: "merge".into(),
                    map: None,
                },
                Edge {
                    from: "merge".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        assert!(
            validate(&def, &any_model).is_ok(),
            "converging diamond is a valid DAG and must not be reported as a cycle"
        );
    }

    /// A cycle routed through a Branch arm (a→br, br.when_true="a") must be detected.
    /// This proves Fix 1: without branch arms in the adjacency map this cycle is invisible.
    #[test]
    fn branch_routed_cycle_is_error() {
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "branch-cycle".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "a".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
                    },
                },
                Node {
                    id: "br".into(),
                    kind: NodeKind::Branch {
                        cond: ".score > 0.5".into(),
                        // when_true loops back to "a" — this is the cycle
                        when_true: "a".into(),
                        when_false: "o".into(),
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
                    to: "a".into(),
                    map: None,
                },
                Edge {
                    from: "a".into(),
                    to: "br".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("cycle"),
            "expected cycle error for branch-arm loop a→br→(when_true)→a; got: {combined}"
        );
    }

    /// A three-node cycle (a→b→c→a) within an otherwise-valid workflow must be detected.
    #[test]
    fn three_node_cycle_is_error() {
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "three-cycle".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "a".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
                    },
                },
                Node {
                    id: "b".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
                    },
                },
                Node {
                    id: "c".into(),
                    kind: NodeKind::Transform {
                        expr: "identity".into(),
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
                    to: "a".into(),
                    map: None,
                },
                Edge {
                    from: "a".into(),
                    to: "b".into(),
                    map: None,
                },
                Edge {
                    from: "b".into(),
                    to: "c".into(),
                    map: None,
                },
                // back-edge closes the cycle
                Edge {
                    from: "c".into(),
                    to: "a".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("cycle"),
            "expected cycle error for three-node cycle a→b→c→a; got: {combined}"
        );
    }

    /// All errors are collected, not just the first.
    #[test]
    fn multiple_errors_all_collected() {
        // No Trigger, bad edge, bad model — should get ≥3 errors.
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "multi-error".to_string(),
            nodes: vec![
                Node {
                    id: "m".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "bad-model".into(),
                        },
                        prompt: "p".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![Edge {
                from: "ghost".into(),
                to: "o".into(),
                map: None,
            }],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        };
        let errs = validate(&def, &|m| m == "gpt-4o").unwrap_err();
        assert!(
            errs.len() >= 3,
            "expected ≥3 errors (no trigger, bad edge src, bad model), got {}: {:?}",
            errs.len(),
            errs
        );
    }
}
