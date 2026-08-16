//! Workflow graph traversal, template resolution, and branch evaluation.

use super::*;
// ---------------------------------------------------------------------------
// Graph helpers
// ---------------------------------------------------------------------------

/// Build adjacency list (node_id → Vec<node_id>) over the UNION of
/// `def.edges` and each Branch node's `when_true`/`when_false` arcs.
/// This mirrors the cycle-detection graph used by `validate::check_cycles`.
pub(super) fn build_union_adj(def: &WorkflowDefinition) -> HashMap<String, Vec<String>> {
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
pub(super) fn topo_sort(
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
pub(super) fn propagate_edges(
    node_id: &str,
    def: &WorkflowDefinition,
    reachable: &mut HashSet<String>,
) {
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
pub(super) fn substitute(
    template: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    variables: &BTreeMap<String, String>,
) -> String {
    let mut result = String::with_capacity(template.len() + 16);
    let mut remaining = template;

    while let Some(open) = remaining.find("{{") {
        result.push_str(&remaining[..open]);
        remaining = &remaining[open + 2..];

        if let Some(close) = remaining.find("}}") {
            let ref_str = remaining[..close].trim();
            let resolved = resolve_ref(ref_str, trigger_id, outputs, variables);
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
/// - `"secrets.*"` → **always** `"***"` (redaction marker, never the real value).
///   Secrets are resolved exclusively in `wf_http::substitute_with_secrets` so
///   that Model/Agent prompts, Transform exprs, and Branch conditions are always
///   secret-free.
fn resolve_ref(
    ref_str: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    variables: &BTreeMap<String, String>,
) -> String {
    // Split on the first `.` to allow `node.field`.
    let (node_part, field_part) = match ref_str.find('.') {
        Some(pos) => (&ref_str[..pos], Some(&ref_str[pos + 1..])),
        None => (ref_str, None),
    };

    // SECURITY: `{{secrets.*}}` / `{{secrets}}` must never return a real secret
    // value from the shared substitution path. Return an explicit redaction
    // marker so callers see "***" rather than "" (which could be confused with
    // "the secret is an empty string").
    if node_part == "secrets" {
        return "***".to_string();
    }
    if node_part == "variables" {
        return field_part
            .and_then(|name| variables.get(name))
            .cloned()
            .unwrap_or_default();
    }

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

/// Build a bounded, value-free input capture for one template evaluation.
///
/// Resolves every `{{ref}}` in `template` through the same secret-free resolver
/// the engine's substitution uses, so captured values exactly match what live
/// evaluation produced and secret refs arrive as `"***"` — never a real value.
/// Returns `None` when the template references nothing (nothing to replay), and
/// the JSON form is truncated by the replay module's hard bounds.
pub(super) fn capture_input(
    template: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    variables: &BTreeMap<String, String>,
) -> Option<serde_json::Value> {
    let captured = capture_node_input(template, |ref_str| {
        resolve_ref(ref_str, trigger_id, outputs, variables)
    })?;
    serde_json::to_value(captured).ok()
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
pub(super) fn eval_cond(
    cond: &str,
    trigger_id: &str,
    outputs: &HashMap<String, NodeOutput>,
    variables: &BTreeMap<String, String>,
) -> bool {
    let cond = cond.trim();

    if let Some((lhs, rhs)) = cond.split_once(" == ") {
        let lhs_val = substitute(lhs.trim(), trigger_id, outputs, variables);
        let rhs_val = strip_quotes(rhs.trim());
        return lhs_val == rhs_val;
    }

    if let Some((lhs, rhs)) = cond.split_once(" != ") {
        let lhs_val = substitute(lhs.trim(), trigger_id, outputs, variables);
        let rhs_val = strip_quotes(rhs.trim());
        return lhs_val != rhs_val;
    }

    // Truthiness fallback.
    let resolved = substitute(cond, trigger_id, outputs, variables);
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
