//! Synchronous DAG workflow orchestrator (W1a Task 6).
//!
//! # Template syntax (Transform nodes + Model/Agent prompts)
//!
//! - `{{input}}` — the Trigger node's output content, coerced to a string.
//!   If the trigger content is a JSON string value the raw string is returned;
//!   other JSON values are serialized compactly.
//! - `{{node_id}}` — the full `content` of the named node's output, coerced
//!   to a string by the same rule.
//! - `{{node_id.field}}` — a single top-level field from a JSON-object output.
//!   Resolves to `""` when the node has not run, the content is not an object,
//!   or the field is absent.
//!
//! Substitution scans left-to-right; unclosed `{{` is passed through as-is.
//!
//! # Branch condition syntax
//!
//! - `{{ref}} == "literal"` — string equality after resolving `{{ref}}`.
//! - `{{ref}} != "literal"` — string inequality.
//! - `{{ref}}` — truthiness: non-empty / non-`"false"` / non-`"null"` /
//!   non-`"0"` string.
//!
//! The literal may be single- or double-quoted.  Numeric comparisons are not
//! supported in W1a; pin a string or use the truthiness form.
//!
//! # Auto model selection
//!
//! `ModelSelection::Auto` is not supported in W1a.  Encountering it fails the
//! run immediately with:
//! `"Auto model selection is not supported in W1a; pin a model or route_ref"`
//!
//! # Budget cap
//!
//! Before each Model or Agent node the engine calls
//! `budget_reached(accrued, run_max_cost_usd)`.  If the cap is already met the
//! run stops with `WfStatus::BudgetExhausted` without invoking the executor.
//! The budget cap is purely accrued-cost-based (no look-ahead estimate) so that
//! any node that exceeds the cap on its own still records a run; the NEXT node
//! is what gets blocked.
//!
//! # Branch reachability
//!
//! After a Branch node fires, only the chosen arm's target (`when_true` or
//! `when_false`) is added to the reachable set.  Nodes reachable exclusively
//! through the not-taken arm are silently skipped.  A merge node that has one
//! taken and one skipped incoming edge still executes; template refs to the
//! skipped node resolve to `null`/`""`.
//!
//! # Limitations (W1a MVP)
//!
//! - Strictly sequential execution within topological order (no parallelism).
//! - A single explicit `def.edges` arc from a Branch node is treated as
//!   unconditional; avoid adding explicit outgoing edges from Branch nodes.
//! - The topo order is defensive: if validate somehow missed a cycle the engine
//!   returns `WfStatus::Failed` rather than looping.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::routes::agent_run_budget::budget_reached;
use crate::workflow::executor::{IntelligenceSpec, NodeExecutor};
use crate::workflow::types::{ModelSelection, Node, NodeKind, NodeOutput, WorkflowDefinition};

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Terminal status of a workflow run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WfStatus {
    Succeeded,
    Failed,
    BudgetExhausted,
}

/// Returned by [`run_workflow`].
#[derive(Debug, Clone)]
pub(crate) struct WorkflowRunResult {
    pub status: WfStatus,
    pub cost_usd: f64,
    /// Named outputs collected from Output nodes (node_id, NodeOutput).
    pub node_outputs: Vec<(String, NodeOutput)>,
    pub error: Option<String>,
}

/// Per-node journal entry passed to the caller-supplied callback after each
/// node completes.  The handler can persist this to `workflow_node_runs`.
#[derive(Debug, Clone)]
pub(crate) struct NodeJournalEntry {
    pub node_id: String,
    /// `"completed"` or `"failed"`.
    pub status: String,
    pub output: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub model_used: Option<String>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Default max turns for Agent nodes (matches CreateRunRequest)
// ---------------------------------------------------------------------------

const DEFAULT_MAX_TURNS: u32 = 8;

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run a validated [`WorkflowDefinition`] to completion.
///
/// `executor` provides the Model/Agent node bridge to the gateway.
/// `inputs` is the workflow's trigger payload.
/// `run_max_cost_usd` is the run-level budget cap (supersedes node-level caps
/// for the stop decision; node caps are still passed to the executor for its
/// own per-node guard).
/// `journal` is called synchronously after every executed node.
pub(crate) async fn run_workflow(
    executor: &dyn NodeExecutor,
    def: &WorkflowDefinition,
    inputs: &serde_json::Value,
    run_max_cost_usd: Option<f64>,
    mut journal: impl FnMut(NodeJournalEntry),
) -> WorkflowRunResult {
    // ---- 1. Find the Trigger node -----------------------------------------
    let trigger_id = match def
        .nodes
        .iter()
        .find(|n| matches!(n.kind, NodeKind::Trigger))
    {
        Some(n) => n.id.clone(),
        None => {
            return WorkflowRunResult {
                status: WfStatus::Failed,
                cost_usd: 0.0,
                node_outputs: vec![],
                error: Some("workflow has no Trigger node".into()),
            };
        }
    };

    // ---- 2. Build union adjacency list ------------------------------------
    let adj = build_union_adj(def);

    // ---- 3. Topological sort (defensive; validate already checked) --------
    let topo_order = match topo_sort(def, &adj) {
        Ok(order) => order,
        Err(e) => {
            return WorkflowRunResult {
                status: WfStatus::Failed,
                cost_usd: 0.0,
                node_outputs: vec![],
                error: Some(e),
            };
        }
    };

    // ---- 4. Node lookup map -----------------------------------------------
    let node_map: HashMap<&str, &Node> = def.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    // ---- 5. Run state -------------------------------------------------------
    let mut outputs: HashMap<String, NodeOutput> = HashMap::new();
    let mut collected_outputs: Vec<(String, NodeOutput)> = Vec::new();
    let mut accrued: f64 = 0.0;
    // Nodes reachable along taken edges from Trigger.
    let mut reachable: HashSet<String> = HashSet::new();
    reachable.insert(trigger_id.clone());

    // ---- 6. Execute in topological order ------------------------------------
    for node_id in &topo_order {
        if !reachable.contains(node_id) {
            continue; // not-taken branch arm or orphan
        }

        let node = match node_map.get(node_id.as_str()) {
            Some(n) => n,
            None => continue, // shouldn't happen in a validated definition
        };

        match &node.kind {
            // ------------------------------------------------------------------
            NodeKind::Trigger => {
                let out = NodeOutput {
                    content: inputs.clone(),
                    cost_usd: 0.0,
                    model_used: None,
                };
                outputs.insert(node_id.clone(), out);
                propagate_edges(node_id, def, &mut reachable);
                // Trigger is not journaled (no model/cost).
            }

            // ------------------------------------------------------------------
            NodeKind::Transform { expr } => {
                let value = substitute(expr, &trigger_id, &outputs);
                let out = NodeOutput {
                    content: serde_json::Value::String(value.clone()),
                    cost_usd: 0.0,
                    model_used: None,
                };
                journal(NodeJournalEntry {
                    node_id: node_id.clone(),
                    status: "completed".into(),
                    output: Some(serde_json::Value::String(value)),
                    cost_usd: 0.0,
                    model_used: None,
                    error: None,
                });
                outputs.insert(node_id.clone(), out);
                propagate_edges(node_id, def, &mut reachable);
            }

            // ------------------------------------------------------------------
            NodeKind::Branch {
                cond,
                when_true,
                when_false,
            } => {
                let taken = if eval_cond(cond, &trigger_id, &outputs) {
                    when_true.clone()
                } else {
                    when_false.clone()
                };
                journal(NodeJournalEntry {
                    node_id: node_id.clone(),
                    status: "completed".into(),
                    output: Some(serde_json::Value::String(taken.clone())),
                    cost_usd: 0.0,
                    model_used: None,
                    error: None,
                });
                // Chosen arm + any unconditional explicit edges from this node.
                reachable.insert(taken.clone());
                propagate_edges(node_id, def, &mut reachable);
            }

            // ------------------------------------------------------------------
            NodeKind::Model {
                selection,
                prompt,
                max_cost_usd: node_cap,
            } => {
                // Budget check BEFORE calling the executor.
                if budget_reached(accrued, run_max_cost_usd) {
                    return WorkflowRunResult {
                        status: WfStatus::BudgetExhausted,
                        cost_usd: accrued,
                        node_outputs: collected_outputs,
                        error: None,
                    };
                }

                // Auto is not supported in W1a.
                if matches!(selection, ModelSelection::Auto) {
                    return WorkflowRunResult {
                        status: WfStatus::Failed,
                        cost_usd: accrued,
                        node_outputs: collected_outputs,
                        error: Some(
                            "Auto model selection is not supported in W1a; \
                             pin a model or route_ref"
                                .into(),
                        ),
                    };
                }

                let subst_prompt = substitute(prompt, &trigger_id, &outputs);
                let spec = IntelligenceSpec {
                    selection: selection.clone(),
                    prompt: subst_prompt,
                    tools: vec![],
                    max_turns: 1,
                    max_cost_usd: *node_cap,
                };

                match executor.run_intelligence(node_id, &spec).await {
                    Err(e) => {
                        return WorkflowRunResult {
                            status: WfStatus::Failed,
                            cost_usd: accrued,
                            node_outputs: collected_outputs,
                            error: Some(format!("node \"{node_id}\" failed: {e}")),
                        };
                    }
                    Ok(out) => {
                        accrued += out.cost_usd;
                        journal(NodeJournalEntry {
                            node_id: node_id.clone(),
                            status: "completed".into(),
                            output: Some(out.content.clone()),
                            cost_usd: out.cost_usd,
                            model_used: out.model_used.clone(),
                            error: None,
                        });
                        outputs.insert(node_id.clone(), out);
                        propagate_edges(node_id, def, &mut reachable);
                    }
                }
            }

            // ------------------------------------------------------------------
            NodeKind::Agent {
                selection,
                prompt,
                max_turns,
                max_cost_usd: node_cap,
                tools,
            } => {
                // Budget check BEFORE calling the executor.
                if budget_reached(accrued, run_max_cost_usd) {
                    return WorkflowRunResult {
                        status: WfStatus::BudgetExhausted,
                        cost_usd: accrued,
                        node_outputs: collected_outputs,
                        error: None,
                    };
                }

                // Auto is not supported in W1a.
                if matches!(selection, ModelSelection::Auto) {
                    return WorkflowRunResult {
                        status: WfStatus::Failed,
                        cost_usd: accrued,
                        node_outputs: collected_outputs,
                        error: Some(
                            "Auto model selection is not supported in W1a; \
                             pin a model or route_ref"
                                .into(),
                        ),
                    };
                }

                let subst_prompt = substitute(prompt, &trigger_id, &outputs);
                let spec = IntelligenceSpec {
                    selection: selection.clone(),
                    prompt: subst_prompt,
                    tools: tools.clone(),
                    max_turns: max_turns.unwrap_or(DEFAULT_MAX_TURNS),
                    max_cost_usd: *node_cap,
                };

                match executor.run_intelligence(node_id, &spec).await {
                    Err(e) => {
                        return WorkflowRunResult {
                            status: WfStatus::Failed,
                            cost_usd: accrued,
                            node_outputs: collected_outputs,
                            error: Some(format!("node \"{node_id}\" failed: {e}")),
                        };
                    }
                    Ok(out) => {
                        accrued += out.cost_usd;
                        journal(NodeJournalEntry {
                            node_id: node_id.clone(),
                            status: "completed".into(),
                            output: Some(out.content.clone()),
                            cost_usd: out.cost_usd,
                            model_used: out.model_used.clone(),
                            error: None,
                        });
                        outputs.insert(node_id.clone(), out);
                        propagate_edges(node_id, def, &mut reachable);
                    }
                }
            }

            // ------------------------------------------------------------------
            NodeKind::Output => {
                // Collect the last output from each incoming edge's source.
                for edge in &def.edges {
                    if edge.to == *node_id {
                        if let Some(src_out) = outputs.get(&edge.from) {
                            collected_outputs.push((node_id.clone(), src_out.clone()));
                        }
                    }
                }
                // Output nodes can theoretically have outgoing edges (to another
                // Output that aggregates); propagate for completeness.
                propagate_edges(node_id, def, &mut reachable);
            }
        }
    }

    WorkflowRunResult {
        status: WfStatus::Succeeded,
        cost_usd: accrued,
        node_outputs: collected_outputs,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Graph helpers
// ---------------------------------------------------------------------------

/// Build adjacency list (node_id → Vec<node_id>) over the UNION of
/// `def.edges` and each Branch node's `when_true`/`when_false` arcs.
/// This mirrors the cycle-detection graph used by `validate::check_cycles`.
fn build_union_adj(def: &WorkflowDefinition) -> HashMap<String, Vec<String>> {
    let mut adj: HashMap<String, Vec<String>> = def
        .nodes
        .iter()
        .map(|n| (n.id.clone(), Vec::new()))
        .collect();

    for edge in &def.edges {
        if adj.contains_key(&edge.from) {
            adj.get_mut(&edge.from).unwrap().push(edge.to.clone());
        }
    }

    for node in &def.nodes {
        if let NodeKind::Branch {
            when_true,
            when_false,
            ..
        } = &node.kind
        {
            if adj.contains_key(&node.id) {
                if adj.contains_key(when_true) {
                    adj.get_mut(&node.id).unwrap().push(when_true.clone());
                }
                if adj.contains_key(when_false) {
                    adj.get_mut(&node.id).unwrap().push(when_false.clone());
                }
            }
        }
    }

    adj
}

/// Kahn's topological sort over `adj`.  Returns the sorted node ids or an
/// error string if a cycle is detected.
fn topo_sort(
    def: &WorkflowDefinition,
    adj: &HashMap<String, Vec<String>>,
) -> Result<Vec<String>, String> {
    let mut in_degree: HashMap<&str, usize> =
        def.nodes.iter().map(|n| (n.id.as_str(), 0usize)).collect();

    for neighbors in adj.values() {
        for nbr in neighbors {
            if let Some(d) = in_degree.get_mut(nbr.as_str()) {
                *d += 1;
            }
        }
    }

    let mut queue: VecDeque<String> = def
        .nodes
        .iter()
        .filter(|n| *in_degree.get(n.id.as_str()).unwrap_or(&1) == 0)
        .map(|n| n.id.clone())
        .collect();

    let mut order: Vec<String> = Vec::with_capacity(def.nodes.len());

    while let Some(id) = queue.pop_front() {
        order.push(id.clone());
        if let Some(neighbors) = adj.get(&id) {
            for nbr in neighbors.clone() {
                if let Some(d) = in_degree.get_mut(nbr.as_str()) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(nbr.clone());
                    }
                }
            }
        }
    }

    if order.len() < def.nodes.len() {
        Err(format!(
            "workflow contains a cycle ({} of {} nodes could not be sorted)",
            def.nodes.len() - order.len(),
            def.nodes.len()
        ))
    } else {
        Ok(order)
    }
}

/// Propagate reachability along all explicit `def.edges` from `node_id`.
/// Called for every node kind (even Branch — the Branch arm propagation is
/// done separately in the main loop before calling this).
fn propagate_edges(node_id: &str, def: &WorkflowDefinition, reachable: &mut HashSet<String>) {
    for edge in &def.edges {
        if edge.from == node_id {
            reachable.insert(edge.to.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Template substitution
// ---------------------------------------------------------------------------

/// Substitute `{{...}}` references in `template` using the accumulated node
/// outputs.  `trigger_id` is the canonical name for the `{{input}}` alias.
fn substitute(template: &str, trigger_id: &str, outputs: &HashMap<String, NodeOutput>) -> String {
    let mut result = String::with_capacity(template.len() + 16);
    let mut remaining = template;

    while let Some(open) = remaining.find("{{") {
        result.push_str(&remaining[..open]);
        remaining = &remaining[open + 2..];

        if let Some(close) = remaining.find("}}") {
            let ref_str = remaining[..close].trim();
            let resolved = resolve_ref(ref_str, trigger_id, outputs);
            result.push_str(&resolved);
            remaining = &remaining[close + 2..];
        } else {
            // Unclosed `{{` — emit as-is and stop scanning.
            result.push_str("{{");
            break;
        }
    }
    result.push_str(remaining);
    result
}

/// Resolve a single `{{ref}}` token.
///
/// - `"input"` → the trigger node's content.
/// - `"node_id"` → the full content of that node.
/// - `"node_id.field"` → a top-level JSON object field of that node's content.
fn resolve_ref(ref_str: &str, trigger_id: &str, outputs: &HashMap<String, NodeOutput>) -> String {
    // Split on the first `.` to allow `node.field`.
    let (node_part, field_part) = match ref_str.find('.') {
        Some(pos) => (&ref_str[..pos], Some(&ref_str[pos + 1..])),
        None => (ref_str, None),
    };

    // `{{input}}` is an alias for the Trigger node.
    let node_key = if node_part == "input" {
        trigger_id
    } else {
        node_part
    };

    let content = match outputs.get(node_key) {
        Some(out) => &out.content,
        None => return String::new(),
    };

    match field_part {
        None => json_to_string(content),
        Some(field) => match content {
            serde_json::Value::Object(map) => {
                map.get(field).map(json_to_string).unwrap_or_default()
            }
            _ => String::new(),
        },
    }
}

/// Coerce a JSON value to a plain string for template output.
/// String values are unwrapped; other values are compactly serialized.
fn json_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Branch condition evaluation
// ---------------------------------------------------------------------------

/// Evaluate a branch condition string against accumulated outputs.
///
/// Supported forms:
/// - `{{ref}} == "literal"` — string equality.
/// - `{{ref}} != "literal"` — string inequality.
/// - `{{ref}}` — truthiness: non-empty / non-`"false"` / non-`"null"` /
///   non-`"0"` string.
fn eval_cond(cond: &str, trigger_id: &str, outputs: &HashMap<String, NodeOutput>) -> bool {
    let cond = cond.trim();

    if let Some((lhs, rhs)) = cond.split_once(" == ") {
        let lhs_val = substitute(lhs.trim(), trigger_id, outputs);
        let rhs_val = strip_quotes(rhs.trim());
        return lhs_val == rhs_val;
    }

    if let Some((lhs, rhs)) = cond.split_once(" != ") {
        let lhs_val = substitute(lhs.trim(), trigger_id, outputs);
        let rhs_val = strip_quotes(rhs.trim());
        return lhs_val != rhs_val;
    }

    // Truthiness fallback.
    let resolved = substitute(cond, trigger_id, outputs);
    is_truthy(&resolved)
}

/// Strip matching single or double quotes from a literal value.
fn strip_quotes(s: &str) -> String {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Falsy strings: empty, `"false"`, `"null"`, `"0"`.  Everything else is truthy.
fn is_truthy(s: &str) -> bool {
    !matches!(s, "" | "false" | "null" | "0")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        error::ApiError,
        workflow::types::{BudgetPolicy, Edge, ModelSelection, Node, NodeKind, WorkflowDefinition},
    };

    // ---- Stub NodeExecutor --------------------------------------------------

    /// A test-only executor with scripted per-node responses.
    struct StubExecutor {
        /// node_id → NodeOutput to return on the next call for that node.
        responses: HashMap<String, NodeOutput>,
        /// Append-only call log: (node_id, prompt).
        calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl StubExecutor {
        fn new(responses: Vec<(&str, NodeOutput)>) -> Self {
            StubExecutor {
                responses: responses
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn called_nodes(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _)| id.clone())
                .collect()
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl NodeExecutor for StubExecutor {
        async fn run_intelligence(
            &self,
            node_id: &str,
            spec: &IntelligenceSpec,
        ) -> Result<NodeOutput, ApiError> {
            self.calls
                .lock()
                .unwrap()
                .push((node_id.to_string(), spec.prompt.clone()));
            self.responses
                .get(node_id)
                .cloned()
                .ok_or_else(|| ApiError::Internal(format!("stub: no response for {node_id}")))
        }
    }

    // ---- Workflow definition helpers ----------------------------------------

    /// T → m1 → m2 → o (linear two-model chain)
    fn make_sequential_def() -> WorkflowDefinition {
        WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "seq".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "m2".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{m1}}".into(),
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
        }
    }

    /// T → br (cond: {{input}} == "yes") → m_yes / m_no → o
    fn make_branch_def() -> WorkflowDefinition {
        WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "branch".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "br".into(),
                    kind: NodeKind::Branch {
                        cond: r#"{{input}} == "yes""#.into(),
                        when_true: "m_yes".into(),
                        when_false: "m_no".into(),
                    },
                },
                Node {
                    id: "m_yes".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "yes path".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "m_no".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "no path".into(),
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
                    to: "br".into(),
                    map: None,
                },
                Edge {
                    from: "m_yes".into(),
                    to: "o".into(),
                    map: None,
                },
                Edge {
                    from: "m_no".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        }
    }

    /// T → tr (transform: "{{input}} processed") → m1 (prompt: "{{tr}}") → o
    fn make_transform_def() -> WorkflowDefinition {
        WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "transform".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "tr".into(),
                    kind: NodeKind::Transform {
                        expr: "{{input}} processed".into(),
                    },
                },
                Node {
                    id: "m1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{tr}}".into(),
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
                    to: "tr".into(),
                    map: None,
                },
                Edge {
                    from: "tr".into(),
                    to: "m1".into(),
                    map: None,
                },
                Edge {
                    from: "m1".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
        }
    }

    // ---- TDD: write tests FIRST, verify they compile before implementing ----

    /// sequential: trigger → m1 → m2 → output
    /// Both models run, costs accrue, status Succeeded.
    #[tokio::test]
    async fn test_sequential_run() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("response_1"),
                    cost_usd: 0.10,
                    model_used: Some("haiku".into()),
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("response_2"),
                    cost_usd: 0.15,
                    model_used: Some("haiku".into()),
                },
            ),
        ]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(&stub, &def, &json!("hello"), None, |e| {
            journal_entries.push(e)
        })
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert!(
            (result.cost_usd - 0.25).abs() < 1e-9,
            "expected 0.25 total cost, got {}",
            result.cost_usd
        );
        assert_eq!(
            stub.called_nodes(),
            vec!["m1", "m2"],
            "both models must run in order"
        );
        // m1 and m2 each emit one journal entry (Trigger and Output do not).
        let node_ids: Vec<_> = journal_entries.iter().map(|e| e.node_id.as_str()).collect();
        assert_eq!(node_ids, vec!["m1", "m2"]);
    }

    /// budget_cap: after m1 (cost 0.25) the cap of 0.20 is exceeded;
    /// m2 is refused → BudgetExhausted; journal has exactly 1 entry (m1).
    #[tokio::test]
    async fn test_budget_cap() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.25,
                    model_used: None,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.25,
                    model_used: None,
                },
            ),
        ]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        // cap = 0.20: before m1 accrued=0.0 < 0.20 → runs; after m1 accrued=0.25 >= 0.20
        // → before m2 budget_reached=true → BudgetExhausted.
        let result = run_workflow(&stub, &def, &json!("hi"), Some(0.20), |e| {
            journal_entries.push(e)
        })
        .await;

        assert_eq!(result.status, WfStatus::BudgetExhausted);
        assert!(
            (result.cost_usd - 0.25).abs() < 1e-9,
            "cost must reflect the one node that ran"
        );
        assert_eq!(
            stub.called_nodes(),
            vec!["m1"],
            "m2 must not be called after budget is exhausted"
        );
        assert_eq!(journal_entries.len(), 1, "only m1 should be journaled");
        assert_eq!(journal_entries[0].node_id, "m1");
    }

    /// branch: a Branch node routes on the trigger input; only the chosen
    /// arm's model must run; the other must NOT be called.
    #[tokio::test]
    async fn test_branch_takes_correct_arm() {
        let def = make_branch_def();
        let stub = StubExecutor::new(vec![
            (
                "m_yes",
                NodeOutput {
                    content: json!("yes_out"),
                    cost_usd: 0.05,
                    model_used: None,
                },
            ),
            (
                "m_no",
                NodeOutput {
                    content: json!("no_out"),
                    cost_usd: 0.05,
                    model_used: None,
                },
            ),
        ]);

        // Input "yes" → cond `{{input}} == "yes"` is true → when_true = m_yes.
        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(&stub, &def, &json!("yes"), None, |e| {
            journal_entries.push(e)
        })
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let called = stub.called_nodes();
        assert!(
            called.contains(&"m_yes".to_string()),
            "m_yes must run on 'yes' input; called: {called:?}"
        );
        assert!(
            !called.contains(&"m_no".to_string()),
            "m_no must NOT run on 'yes' input; called: {called:?}"
        );
    }

    /// transform: a Transform node maps an upstream output into a downstream
    /// model prompt; assert the substituted value propagated correctly.
    #[tokio::test]
    async fn test_transform_propagates_value() {
        let def = make_transform_def();
        let stub = StubExecutor::new(vec![(
            "m1",
            NodeOutput {
                content: json!("model_out"),
                cost_usd: 0.10,
                model_used: None,
            },
        )]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(&stub, &def, &json!("hello"), None, |e| {
            journal_entries.push(e)
        })
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let calls = stub.calls();
        assert_eq!(calls.len(), 1, "exactly one model call");
        // The Transform node produced "hello processed"; the model prompt
        // `{{tr}}` should have been substituted to that value.
        assert_eq!(
            calls[0].1, "hello processed",
            "transform output must propagate into the model prompt"
        );
    }

    // ---- Unit tests for substitution and condition helpers ------------------

    #[test]
    fn substitute_input_alias() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!("world"),
                cost_usd: 0.0,
                model_used: None,
            },
        );
        assert_eq!(substitute("hello {{input}}", "t", &outputs), "hello world");
    }

    #[test]
    fn substitute_node_field() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "n1".into(),
            NodeOutput {
                content: json!({"answer": "42"}),
                cost_usd: 0.0,
                model_used: None,
            },
        );
        assert_eq!(substitute("{{n1.answer}}", "t", &outputs), "42");
    }

    #[test]
    fn substitute_missing_ref_is_empty() {
        let outputs = HashMap::new();
        assert_eq!(substitute("{{missing}}", "t", &outputs), "");
    }

    #[test]
    fn eval_cond_equality_true() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!("yes"),
                cost_usd: 0.0,
                model_used: None,
            },
        );
        assert!(eval_cond(r#"{{input}} == "yes""#, "t", &outputs));
    }

    #[test]
    fn eval_cond_equality_false() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!("no"),
                cost_usd: 0.0,
                model_used: None,
            },
        );
        assert!(!eval_cond(r#"{{input}} == "yes""#, "t", &outputs));
    }

    #[test]
    fn eval_cond_truthiness() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!("something"),
                cost_usd: 0.0,
                model_used: None,
            },
        );
        assert!(eval_cond("{{input}}", "t", &outputs));
    }

    #[test]
    fn eval_cond_empty_is_falsy() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "t".into(),
            NodeOutput {
                content: json!(""),
                cost_usd: 0.0,
                model_used: None,
            },
        );
        assert!(!eval_cond("{{input}}", "t", &outputs));
    }
}
