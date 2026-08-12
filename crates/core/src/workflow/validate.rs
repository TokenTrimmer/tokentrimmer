//! Workflow DAG + model-resolution validation.
//!
//! `validate` is intentionally **pure**: it takes a `model_exists` closure
//! instead of a `&ProviderRegistry` so callers can stub it trivially in unit
//! tests.  Production callers pass `|m| state.registry.resolve(m).is_some()`.
//!
//! All errors are collected before returning so callers see the full picture.

use std::collections::{HashMap, HashSet, VecDeque};

use super::types::{ModelSelection, NodeKind, WorkflowDefinition, WorkflowTrigger};
use super::{environment_variables::required_variable_names, secrets::required_secret_names};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Validate a newly written [`WorkflowDefinition`] against structural and model
/// constraints.
///
/// `model_exists(model_id)` should return `true` when the given model id is
/// known to the gateway (i.e. `registry.resolve(model_id).is_some()`).  Only
/// [`ModelSelection::Model`] nodes are checked against the registry; `Route`
/// selections are accepted without a lookup.  [`ModelSelection::Auto`] is
/// rejected with an error — callers must pin a model or `route_ref` for W1a.
///
/// New and updated schedule triggers must meet the current one-hour minimum.
/// Stored definitions execute through `validate_for_execution` so a schedule
/// persisted before that write-time floor is not silently disabled.
///
/// Returns `Ok(())` when the definition is valid, or `Err(errors)` where
/// `errors` is a **complete** list of all violations found.
pub fn validate(
    def: &WorkflowDefinition,
    model_exists: &dyn Fn(&str) -> bool,
) -> Result<(), Vec<String>> {
    validate_with_schedule_floor(def, model_exists, true)
}

/// Validate a definition loaded from durable storage before it executes.
///
/// This preserves the structural, model, maximum-interval, and webhook checks
/// from [`validate`], but deliberately permits a sub-hour schedule written
/// before the current write contract. The only callers load a persisted
/// definition first; new and updated API writes always use [`validate`]. An
/// operator must explicitly migrate legacy schedules to one hour or longer.
pub(crate) fn validate_for_execution(
    def: &WorkflowDefinition,
    model_exists: &dyn Fn(&str) -> bool,
) -> Result<(), Vec<String>> {
    validate_with_schedule_floor(def, model_exists, false)
}

fn validate_with_schedule_floor(
    def: &WorkflowDefinition,
    model_exists: &dyn Fn(&str) -> bool,
    enforce_current_schedule_minimum: bool,
) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    // Validate node identity and ordinary-edge invariants before any graph
    // traversal. Kahn's maps are keyed by node id, so duplicate ids would
    // collapse distinct nodes and can manufacture a false cycle.
    let (node_ids, graph_structure_is_valid) = validate_graph_structure(def, &mut errors);

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
    let mut references_are_valid = true;
    for edge in &def.edges {
        if !node_ids.contains(edge.from.as_str()) {
            references_are_valid = false;
            errors.push(format!(
                "edge references unknown source node id \"{}\"",
                edge.from
            ));
        }
        if !node_ids.contains(edge.to.as_str()) {
            references_are_valid = false;
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
                references_are_valid = false;
                errors.push(format!(
                    "Branch node \"{}\" when_true references unknown node id \"{}\"",
                    node.id, when_true
                ));
            }
            if !node_ids.contains(when_false.as_str()) {
                references_are_valid = false;
                errors.push(format!(
                    "Branch node \"{}\" when_false references unknown node id \"{}\"",
                    node.id, when_false
                ));
            }
        }
    }

    // ------------------------------------------------------------------
    // 5. Model/Agent nodes:
    //    a. Auto selection is rejected at validate-time (W1a contract: must pin).
    //    b. Pinned model ids must pass `model_exists`.
    // ------------------------------------------------------------------
    for node in &def.nodes {
        let selection = match &node.kind {
            NodeKind::Model { selection, .. } => Some(selection),
            NodeKind::Agent { selection, .. } => Some(selection),
            _ => None,
        };
        match selection {
            Some(ModelSelection::Auto) => {
                errors.push(format!(
                    "node {}: Auto model selection is not supported in W1a; \
                     pin a model or route_ref",
                    node.id
                ));
            }
            Some(ModelSelection::Model { model }) => {
                if !model_exists(model) {
                    errors.push(format!(
                        "node \"{}\" references unknown model \"{}\"",
                        node.id, model
                    ));
                }
            }
            _ => {}
        }

        let output_cap = match &node.kind {
            NodeKind::Model {
                max_output_tokens, ..
            }
            | NodeKind::Agent {
                max_output_tokens, ..
            } => *max_output_tokens,
            _ => None,
        };
        if output_cap == Some(0) {
            errors.push(format!(
                "node \"{}\": max_output_tokens must be a positive integer when set",
                node.id
            ));
        }
    }

    // ------------------------------------------------------------------
    // 6. Http nodes: allowed_hosts (default-deny), https-only, denied headers.
    // ------------------------------------------------------------------
    for node in &def.nodes {
        if let NodeKind::Http { url, headers, .. } = &node.kind {
            let node_id = &node.id;

            // Extract the authority (everything after scheme://, before the first
            // '/') as a simple string operation so it succeeds even when the URL
            // contains curly braces (e.g. templated paths).
            let authority: &str = {
                let after_scheme = url
                    .find("://")
                    .map(|i| &url[i + 3..])
                    .unwrap_or(url.as_str());
                after_scheme.split('/').next().unwrap_or(after_scheme)
            };

            // ---- a-0. Reject userinfo in authority (SSRF allowlist-bypass) ----
            // A URL like `https://user:pass@host/` encodes credentials in the
            // authority.  The naive port-stripping extractor below would split on
            // ':' first, producing e.g. "allowed.example.com" from the authority
            // "allowed.example.com:@evil.com", which falsely passes the allowlist
            // while reqwest actually connects to evil.com.  Reject any URL whose
            // authority contains '@'.  Credentials must flow via
            // `{{secrets.NAME}}` Authorization headers instead.
            if authority.contains('@') {
                errors.push(format!(
                    "node \"{node_id}\": Http url must not contain userinfo \
                     ('@' in authority); pass credentials via a \
                     {{{{secrets.NAME}}}} header instead"
                ));
                continue;
            }

            // Strip port from the authority to get the raw hostname
            // (e.g. "api.example.com:8080" → "api.example.com").
            let raw_host: &str = authority.split(':').next().unwrap_or(authority);

            // ---- a. Reject templated hosts ----
            // Only the path / query-string / headers / body may contain
            // `{{...}}` template tokens.  The host must be a static literal so
            // the allowlist is unambiguous.
            if raw_host.contains("{{") {
                errors.push(format!(
                    "node \"{node_id}\": Http url host must be a literal hostname; \
                     templated hosts are not allowlistable \
                     (only path/query/headers/body may use {{{{...}}}} templates)"
                ));
                // Skip the allowlist + url-guard — they'd both fail on the same
                // malformed URL, producing redundant noise.
                continue;
            }

            // ---- b. Default-deny allowlist ----
            // Empty `allowed_hosts` ⇒ all Http nodes are rejected.
            if !def.allowed_hosts.iter().any(|h| h == raw_host) {
                errors.push(format!(
                    "node \"{node_id}\": Http url host \"{raw_host}\" is not in \
                     workflow allowed_hosts (default-deny); \
                     add the host to the workflow's allowed_hosts list"
                ));
            }

            // ---- c. validate_provider_url: https-only + IP/hostname denylist ----
            // Validate the static URL prefix (everything before the first `{{`)
            // so that scheme + host pass the guard even when the path is templated.
            let static_url = url.split("{{").next().unwrap_or(url.as_str());
            // Ensure the trimmed string ends with '/' so it parses as a valid URL.
            let static_url_owned;
            let check_url: &str = if static_url.contains("://") && !static_url.contains('/') {
                // e.g. "https://api.example.com" (host-only, no path slash yet)
                static_url_owned = format!("{static_url}/");
                &static_url_owned
            } else {
                static_url
            };
            if let Err(e) = tt_shared::url_guard::validate_provider_url(check_url, false) {
                errors.push(format!("node \"{node_id}\": Http url rejected: {e}"));
            }

            // ---- d. Denied headers (outbound policy: host + hop-by-hop only) ----
            if let Some(denied) = tt_shared::url_guard::find_outbound_denied_header(headers) {
                errors.push(format!(
                    "node \"{node_id}\": Http header \"{denied}\" is not allowed \
                     (outbound denied headers list)"
                ));
            }
        }
    }

    // Secret references are syntactic definition metadata. Reject malformed
    // names on both write and execution validation; actual availability is a
    // runtime preflight because secrets rotate independently of definitions.
    if let Err(secret_errors) = required_secret_names(def) {
        errors.extend(secret_errors);
    }
    if let Err(variable_errors) = required_variable_names(def) {
        errors.extend(variable_errors);
    }

    // ------------------------------------------------------------------
    // 6b. Loop nodes: max_iters must be 1..=100.
    // ------------------------------------------------------------------
    for node in &def.nodes {
        if let NodeKind::Loop { max_iters, .. } = &node.kind {
            if *max_iters == 0 || *max_iters > 100 {
                errors.push(format!(
                    "node \"{}\": loop max_iters must be 1..=100",
                    node.id
                ));
            }
        }
    }

    // ------------------------------------------------------------------
    // 6c. Document nodes: the source must be a non-empty inline base64 payload
    // (media_type + data) OR a static, credential-free HTTPS URL literal
    // passing the shared SSRF guard (see the Url arm). An optional cache_key,
    // if set, must be non-empty. No template tokens are allowed in a URL
    // source — the fetch target must be a static literal so authoring-time
    // validation is authoritative (no blind substitution-driven egress).
    // ------------------------------------------------------------------
    for node in &def.nodes {
        if let NodeKind::Document { source, cache_key } = &node.kind {
            match source {
                tt_shared::messages::DocumentSource::Base64 { media_type, data } => {
                    if media_type.trim().is_empty() {
                        errors.push(format!(
                            "node \"{}\": document source media_type must be non-empty",
                            node.id
                        ));
                    }
                    if data.trim().is_empty() {
                        errors.push(format!(
                            "node \"{}\": document source data must be non-empty base64",
                            node.id
                        ));
                    }
                }
                tt_shared::messages::DocumentSource::Url { url } => {
                    // URL document source (lossless adapters): admitted under a
                    // CLOSED contract — a static, credential-free HTTPS literal
                    // passing the gateway's shared SSRF guard. The fetched bytes
                    // are then media-set-vetted + byte-capped at run time by the
                    // guarded egress; nothing here opens a second egress path.
                    let trimmed = url.trim();
                    if trimmed.is_empty() {
                        errors.push(format!(
                            "node \"{}\": document source url must be non-empty",
                            node.id
                        ));
                        continue;
                    }
                    // Static literal only: template substitution would let an
                    // author pass a placeholder past this guard and inject the
                    // substituted value as the egress target (a blind SSRF
                    // channel). Reject any template token outright.
                    if trimmed.contains("{{") {
                        errors.push(format!(
                            "node \"{}\": document source url must be a static literal \
                             (no {{{{...}}}} templates); the fetch target cannot be templated",
                            node.id
                        ));
                        continue;
                    }
                    let parsed = match reqwest::Url::parse(trimmed) {
                        Ok(p) => p,
                        Err(_) => {
                            errors.push(format!(
                                "node \"{}\": document source url is not a valid URL",
                                node.id
                            ));
                            continue;
                        }
                    };
                    // Credential-free: userinfo (`user:pass@host`) is rejected —
                    // a URL document source must never carry credentials.
                    if !parsed.username().is_empty() || parsed.password().is_some() {
                        errors.push(format!(
                            "node \"{}\": document source url must not contain userinfo \
                             ('@' in authority)",
                            node.id
                        ));
                    }
                    // https-only + hostname/IP denylist + best-effort resolved-IP
                    // block — the exact shared guard the Http nodes run here.
                    if let Err(e) = tt_shared::url_guard::validate_provider_url(trimmed, false) {
                        errors.push(format!(
                            "node \"{}\": document source url rejected: {}",
                            node.id, e
                        ));
                    }
                }
            }
            if let Some(key) = cache_key {
                if key.trim().is_empty() {
                    errors.push(format!(
                        "node \"{}\": document cache_key, if set, must be non-empty",
                        node.id
                    ));
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // 7. No cycles — Kahn's algorithm (topological sort).
    //
    // The execution graph is the union of def.edges AND Branch arm targets:
    // a Branch node's when_true/when_false are outgoing edges the engine
    // will follow, so a cycle routed through a Branch arm is just as
    // infinite-loop-inducing as one through def.edges.  We treat both as
    // directed arcs in the adjacency map.
    // ------------------------------------------------------------------
    // A traversal is only meaningful once identity, ordinary-edge, and
    // endpoint invariants hold. In particular, duplicate ids would collapse
    // Kahn's HashMaps and report a fabricated cycle for malformed input.
    if graph_structure_is_valid && references_are_valid {
        check_cycles(def, &mut errors);
    }

    // ------------------------------------------------------------------
    // CO-2: triggers (schedule/webhook) — optional invokers. At most one
    // Schedule per workflow; interval parses to a bounded Duration; webhook
    // token_id is a non-empty URL-safe string. Empty `triggers` is the default
    // (human-Run only) and is fine.
    // ------------------------------------------------------------------
    validate_triggers(def, &mut errors, enforce_current_schedule_minimum);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate the graph facts that must be true before a node-id-keyed traversal
/// can safely reason about the definition. Ordinary `Edge`s are distinct from
/// Branch arms: branch arm targets remain part of cycle detection, while only
/// duplicate ordinary `from` → `to` pairs are rejected here.
fn validate_graph_structure<'a>(
    def: &'a WorkflowDefinition,
    errors: &mut Vec<String>,
) -> (HashSet<&'a str>, bool) {
    let mut node_ids = HashSet::with_capacity(def.nodes.len());
    let mut edge_pairs = HashSet::with_capacity(def.edges.len());
    let mut is_valid = true;

    for (index, node) in def.nodes.iter().enumerate() {
        if node.id.trim().is_empty() {
            is_valid = false;
            errors.push(format!(
                "node {} has a blank or whitespace-only id",
                index + 1
            ));
            continue;
        }
        if !node_ids.insert(node.id.as_str()) {
            is_valid = false;
            errors.push(format!("node id \"{}\" is duplicated", node.id));
        }
    }

    for (index, edge) in def.edges.iter().enumerate() {
        if edge.from == edge.to {
            is_valid = false;
            errors.push(format!(
                "edge {} connects node \"{}\" to itself",
                index + 1,
                edge.from
            ));
        }
        // The tuple is collision-free even when imported node ids contain
        // arbitrary separators; `map` does not make a second ordinary edge
        // distinct for execution ordering.
        if !edge_pairs.insert((edge.from.as_str(), edge.to.as_str())) {
            is_valid = false;
            errors.push(format!(
                "edge {} duplicates an ordinary connection from \"{}\" to \"{}\"",
                index + 1,
                edge.from,
                edge.to
            ));
        }
    }

    (node_ids, is_valid)
}

// ---------------------------------------------------------------------------
// CO-2: triggers (schedule/webhook) validation
// ---------------------------------------------------------------------------

/// The hosted schedule dispatcher normally sweeps hourly.  Accepting a shorter
/// write-time interval would promise a cadence the dispatcher cannot honor.
const MIN_SCHEDULE_INTERVAL_SECS: u64 = 60 * 60;

/// The floor enforced before the one-hour write contract. Execution retains it
/// for legacy definitions so corrupt or impossible sub-five-minute schedules
/// cannot become runnable through the compatibility path.
const LEGACY_MIN_SCHEDULE_INTERVAL_SECS: u64 = 5 * 60;

/// Validate the workflow's `triggers` invokers. Empty is the default
/// (human-Run only) and is fine. At most one `Schedule` (two cadences is
/// ambiguous). New or updated `Schedule.interval` values parse to a bounded
/// `Duration` (min 1 h, max 30 d — no unsupported sub-hour promise or
/// months-long silent gaps). Stored legacy definitions retain their former
/// five-minute safety floor during execution. Hosted pickup is an approximate
/// hourly sweep, not an exact-time trigger. `Webhook.token_id` is a non-empty
/// URL-safe string.
fn validate_triggers(
    def: &WorkflowDefinition,
    errors: &mut Vec<String>,
    enforce_current_schedule_minimum: bool,
) {
    let mut schedule_count = 0;
    for t in &def.triggers {
        match t {
            WorkflowTrigger::Schedule { interval, .. } => {
                schedule_count += 1;
                match parse_interval(interval) {
                    Some(d) => {
                        if enforce_current_schedule_minimum
                            && d.as_secs() < MIN_SCHEDULE_INTERVAL_SECS
                        {
                            errors.push(format!(
                                "schedule interval '{interval}' is below the 1-hour minimum; hosted schedules use an approximate hourly sweep, not exact-time delivery"
                            ));
                        } else if d.as_secs() < LEGACY_MIN_SCHEDULE_INTERVAL_SECS {
                            errors.push(format!(
                                "schedule interval '{interval}' is below the 5-minute legacy compatibility minimum"
                            ));
                        } else if d.as_secs() > 30 * 24 * 3600 {
                            errors.push(format!(
                                "schedule interval '{interval}' exceeds the 30-day maximum"
                            ));
                        }
                    }
                    None => errors.push(format!(
                        "schedule interval '{interval}' is not a valid duration (use e.g. '1h', '6h', '1d')"
                    )),
                }
            }
            WorkflowTrigger::Webhook { token_id, .. } => {
                if token_id.is_empty() {
                    errors.push("webhook trigger token_id must not be empty".to_string());
                } else if !token_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    errors.push(format!(
                        "webhook trigger token_id '{token_id}' must be URL-safe (alphanumeric / - / _)"
                    ));
                }
            }
        }
    }
    if schedule_count > 1 {
        errors.push(format!(
            "workflow must have at most one schedule trigger (found {schedule_count})"
        ));
    }
}

/// Parse a duration string (`"1h"`, `"1h30m"`, `"1d"`, or a sum like `"1d6h"`)
/// into a `Duration`. Returns `None` for garbage. The cloud sweep mirrors the
/// fixed-`Duration` cadence discipline (no cron crate), so this is the only
/// schedule grammar in v1.
fn parse_interval(s: &str) -> Option<std::time::Duration> {
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut saw_any = false;
    for ch in s.trim().chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            let unit = match ch {
                'h' => 3600,
                'm' => 60,
                'd' => 24 * 3600,
                _ => return None,
            };
            let n: u64 = digits.parse().ok()?;
            total = total.checked_add(n.checked_mul(unit)?)?;
            digits.clear();
            saw_any = true;
        }
    }
    if !digits.is_empty() || !saw_any {
        return None;
    }
    Some(std::time::Duration::from_secs(total))
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
        BudgetPolicy, Edge, ModelSelection, Node, NodeKind, WorkflowDefinition, WorkflowTrigger,
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
            triggers: vec![],
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
                        max_output_tokens: None,
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
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

    /// An Auto selection is rejected at validate-time (W1a contract).
    #[test]
    fn auto_selection_is_error() {
        let mut def = linear_def("ignored");
        if let NodeKind::Model { selection, .. } = &mut def.nodes[1].kind {
            *selection = ModelSelection::Auto;
        }
        let errs = validate(&def, &no_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("Auto"),
            "expected error about Auto selection; got: {combined}"
        );
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
            triggers: vec![],
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("cycle"),
            "expected error to mention cycle; got: {combined}"
        );
    }

    /// Ordinary self-edges are structural errors, not cycle-analysis input.
    #[test]
    fn ordinary_self_edge_is_rejected_before_cycle_processing() {
        let def = WorkflowDefinition {
            triggers: vec![],
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("connects node \"x\" to itself"),
            "expected self-edge rejection; got: {combined}"
        );
        assert!(
            !combined.contains("cycle"),
            "invalid ordinary edge must not enter cycle analysis; got: {combined}"
        );
    }

    #[test]
    fn blank_or_whitespace_node_ids_are_rejected() {
        for id in ["", " \t\n "] {
            let mut def = linear_def("gpt-4o");
            def.nodes[0].id = id.to_string();
            let errs = validate(&def, &any_model).unwrap_err();
            assert!(
                errs.iter()
                    .any(|error| error.contains("blank or whitespace-only id")),
                "id {id:?} should be rejected: {errs:?}"
            );
        }
    }

    #[test]
    fn duplicate_node_ids_do_not_enter_collapsed_cycle_analysis() {
        let mut def = linear_def("gpt-4o");
        // Retain an output node and only valid ordinary endpoints. Before the
        // identity gate, Kahn's id-keyed maps collapsed these two `m` nodes and
        // falsely reported a cycle because it visited two map entries for three
        // declared nodes.
        def.nodes[2].id = "m".into();
        def.edges.truncate(1);

        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("node id \"m\" is duplicated"),
            "expected duplicate-id rejection; got: {combined}"
        );
        assert!(
            !combined.contains("cycle"),
            "duplicate ids must not be passed to collapsed Kahn maps: {combined}"
        );
    }

    #[test]
    fn duplicate_ordered_ordinary_edges_are_rejected_before_cycle_processing() {
        let mut def = linear_def("gpt-4o");
        def.edges.push(Edge {
            from: "t".into(),
            to: "m".into(),
            map: Some("a separately configured map is still the same edge".into()),
        });

        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("duplicates an ordinary connection from \"t\" to \"m\""),
            "expected duplicate-edge rejection; got: {combined}"
        );
        assert!(
            !combined.contains("cycle"),
            "invalid ordinary edges must not enter cycle analysis; got: {combined}"
        );
    }

    #[test]
    fn validate_for_execution_shares_identity_and_ordinary_edge_invariants() {
        let mut def = linear_def("gpt-4o");
        def.nodes[0].id = "   ".into();
        def.nodes[2].id = "m".into();
        def.edges = vec![
            Edge {
                from: "m".into(),
                to: "m".into(),
                map: None,
            },
            Edge {
                from: "m".into(),
                to: "m".into(),
                map: Some("different map does not make a distinct edge".into()),
            },
        ];

        let errs = validate_for_execution(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        for expected in [
            "blank or whitespace-only id",
            "node id \"m\" is duplicated",
            "connects node \"m\" to itself",
            "duplicates an ordinary connection from \"m\" to \"m\"",
        ] {
            assert!(
                combined.contains(expected),
                "expected {expected:?} from execution validation; got: {combined}"
            );
        }
        assert!(
            !combined.contains("cycle"),
            "execution validation must also skip collapsed cycle analysis: {combined}"
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

    #[test]
    fn output_token_cap_must_be_positive_for_models_and_agents() {
        let mut model_def = linear_def("gpt-4o");
        if let NodeKind::Model {
            max_output_tokens, ..
        } = &mut model_def.nodes[1].kind
        {
            *max_output_tokens = Some(0);
        }
        let model_errors = validate(&model_def, &any_model).unwrap_err();
        assert!(model_errors
            .iter()
            .any(|error| error.contains("max_output_tokens must be a positive integer")));

        let mut agent_def = linear_def("gpt-4o");
        agent_def.nodes[1].kind = NodeKind::Agent {
            selection: ModelSelection::Model {
                model: "gpt-4o".into(),
            },
            prompt: "{{input}}".into(),
            max_turns: Some(1),
            max_output_tokens: Some(0),
            max_cost_usd: None,
            tools: vec![],
        };
        let agent_errors = validate(&agent_def, &any_model).unwrap_err();
        assert!(agent_errors
            .iter()
            .any(|error| error.contains("max_output_tokens must be a positive integer")));
    }

    /// An Auto selection on an Agent node is also rejected at validate-time.
    #[test]
    fn auto_selection_on_agent_node_is_error() {
        let mut def = linear_def("gpt-4o");
        def.nodes[1] = Node {
            id: "m".into(),
            kind: NodeKind::Agent {
                selection: ModelSelection::Auto,
                prompt: "go".into(),
                max_turns: None,
                max_output_tokens: None,
                max_cost_usd: None,
                tools: vec![],
            },
        };
        let errs = validate(&def, &|m| m == "gpt-4o").unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("Auto"),
            "expected Auto error for Agent node; got: {combined}"
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
                max_output_tokens: None,
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
            triggers: vec![],
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
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
            triggers: vec![],
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
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
            triggers: vec![],
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
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
            triggers: vec![],
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
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
            triggers: vec![],
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
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
            triggers: vec![],
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
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
            triggers: vec![],
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
                        max_output_tokens: None,
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
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        };
        let errs = validate(&def, &|m| m == "gpt-4o").unwrap_err();
        assert!(
            errs.len() >= 3,
            "expected ≥3 errors (no trigger, bad edge src, bad model), got {}: {:?}",
            errs.len(),
            errs
        );
    }

    // ------------------------------------------------------------------
    // Http node validation tests (W3b Task 2 — written TDD-first)
    // ------------------------------------------------------------------

    /// Build a minimal Trigger → Http → Output workflow for Http validation tests.
    fn http_def(
        url: &str,
        allowed_hosts: Vec<String>,
        headers: Vec<(String, String)>,
        body: Option<String>,
    ) -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "http-test".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "h".into(),
                    kind: NodeKind::Http {
                        method: "GET".to_string(),
                        url: url.to_string(),
                        headers,
                        body,
                        max_response_bytes: None,
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
                    to: "h".into(),
                    map: None,
                },
                Edge {
                    from: "h".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts,
            metadata: serde_json::Value::Null,
        }
    }

    /// An Http node with an empty allowed_hosts list must be rejected (default-deny).
    /// With the url host in allowed_hosts, validation must pass.
    #[test]
    fn http_url_host_must_be_in_allowed_hosts() {
        // Empty allowed_hosts → every Http url host is denied.
        let def = http_def("https://api.example.com/x", vec![], vec![], None);
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("allowed_hosts")
                || combined.contains("allowlist")
                || combined.contains("not in"),
            "expected allowlist error; got: {combined}"
        );

        // With the host in allowed_hosts → passes (no errors).
        let def2 = http_def(
            "https://api.example.com/x",
            vec!["api.example.com".to_string()],
            vec![],
            None,
        );
        assert!(
            validate(&def2, &any_model).is_ok(),
            "host in allowed_hosts should pass validation"
        );
    }

    #[test]
    fn http_secret_reference_names_are_validated_at_definition_write() {
        let def = http_def(
            "https://api.example.com/x",
            vec!["api.example.com".to_string()],
            vec![(
                "authorization".to_string(),
                "Bearer {{secrets.bad-name}}".to_string(),
            )],
            None,
        );
        let errors = validate(&def, &any_model).unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.contains("invalid secret reference")));
        assert!(errors.iter().all(|error| !error.contains("bad-name")));
    }

    /// An Http node with an http:// (non-https) url must be rejected.
    #[test]
    fn http_rejects_non_https() {
        let def = http_def(
            "http://api.example.com/path",
            vec!["api.example.com".to_string()],
            vec![],
            None,
        );
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("https")
                || combined.contains("insecure")
                || combined.contains("scheme"),
            "expected https-only error; got: {combined}"
        );
    }

    /// An Http node whose url host is a template token must be rejected.
    /// Only path/query/headers/body may be templated; the host must be literal.
    #[test]
    fn http_rejects_templated_host() {
        let def = http_def("https://{{inputs.host}}/x", vec![], vec![], None);
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("template")
                || combined.contains("literal")
                || combined.contains("allowlistable"),
            "expected templated-host error; got: {combined}"
        );
    }

    /// An Http node with a `host` header (outbound-denied) must be rejected.
    #[test]
    fn http_rejects_denied_header() {
        let def = http_def(
            "https://api.example.com/path",
            vec!["api.example.com".to_string()],
            vec![("host".to_string(), "evil.com".to_string())],
            None,
        );
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("host") || combined.contains("denied header"),
            "expected denied-header error; got: {combined}"
        );
    }

    /// An Http node with an `authorization` header must now PASS validation
    /// (outbound policy allows auth headers so HTTP nodes can call external APIs).
    #[test]
    fn http_node_auth_header_passes_validation() {
        let def = http_def(
            "https://api.example.com/path",
            vec!["api.example.com".to_string()],
            vec![
                ("authorization".to_string(), "Bearer token".to_string()),
                ("x-api-key".to_string(), "sk-test".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            None,
        );
        assert!(
            validate(&def, &any_model).is_ok(),
            "authorization/x-api-key/content-type must pass outbound validation"
        );
    }

    /// An Http node with a `host` header must still fail validation (even after
    /// outbound policy allows auth headers).
    #[test]
    fn http_node_host_header_fails_validation() {
        let def = http_def(
            "https://api.example.com/path",
            vec!["api.example.com".to_string()],
            vec![("host".to_string(), "evil.com".to_string())],
            None,
        );
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("host") || combined.contains("denied"),
            "expected denied-header error for host; got: {combined}"
        );
    }

    /// Http url with userinfo (`user:pass@host`) must be rejected regardless of
    /// allowed_hosts.  This guards the SSRF allowlist-bypass class:
    /// `https://allowed.example.com:@evil.com/` looks like host=allowed.example.com
    /// to the naive port-stripping parser (splits on ':' first, so `allowed.example.com`
    /// is extracted), yet reqwest actually connects to `evil.com`.  Before the fix this
    /// test FAILS (no error is returned because the naive check wrongly passes); after
    /// the fix the '@'-in-authority check rejects the url before the allowlist check.
    #[test]
    fn http_rejects_userinfo_in_url() {
        // `https://allowed.example.com:@evil.com/path`:
        //   username = "allowed.example.com", password = "", real host = "evil.com".
        // The naive raw_host extractor splits on ':' and gets "allowed.example.com",
        // which IS in allowed_hosts — so it would wrongly pass before the fix.
        let def = http_def(
            "https://allowed.example.com:@evil.com/path",
            vec!["allowed.example.com".to_string()],
            vec![],
            None,
        );
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("userinfo") || combined.contains('@'),
            "expected userinfo rejection error; got: {combined}"
        );
    }

    /// An Http node with a templated path and templated body (but a literal host
    /// in allowed_hosts) must pass validation — only the host must be literal.
    #[test]
    fn http_allows_templated_path_and_body() {
        let def = http_def(
            "https://api.example.com/{{inputs.id}}",
            vec!["api.example.com".to_string()],
            vec![("X-Custom".to_string(), "value".to_string())],
            Some("{{trigger.data}}".to_string()),
        );
        assert!(
            validate(&def, &any_model).is_ok(),
            "templated path + body with literal host in allowed_hosts must pass"
        );
    }

    /// A Document node with a base64 source + optional cache_key must validate.
    #[test]
    fn document_node_base64_validates() {
        let def = WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "doc".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "d".into(),
                    kind: NodeKind::Document {
                        source: tt_shared::messages::DocumentSource::Base64 {
                            media_type: "application/pdf".into(),
                            data: "JVBERi0=".into(),
                        },
                        cache_key: Some("{{trigger.input_id}}".into()),
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
                    to: "d".into(),
                    map: None,
                },
                Edge {
                    from: "d".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: Vec::new(),
            metadata: serde_json::Value::Null,
        };
        assert!(
            validate(&def, &any_model).is_ok(),
            "a base64 Document node must validate"
        );
    }

    /// A Document node with a credential-free HTTPS URL source must validate —
    /// the URL source is admitted under the same closed contract the rest of
    /// the document-source validation enforces (static literal, https-only,
    /// no userinfo, shared SSRF guard).
    #[test]
    fn document_node_validates_https_url_source() {
        for url in [
            "https://example.com/doc.pdf",
            "https://files.example.com/a/b/report.PDF?token=abc",
            "https://example.com/image.png",
        ] {
            let def = doc_def(tt_shared::messages::DocumentSource::Url {
                url: url.to_string(),
            });
            assert!(
                validate(&def, &any_model).is_ok(),
                "a credential-free HTTPS URL document source must validate; got errors for `{url}`"
            );
        }
    }

    /// URL sources that violate the closed admission contract must be rejected
    /// at authoring: non-https scheme, embedded userinfo, private/loopback/
    /// metadata IP, internal hostname, template tokens, or an unparseable URL.
    #[test]
    fn document_node_rejects_unsafe_url_sources() {
        for url in [
            "http://example.com/doc.pdf",                   // not https
            "https://user:pass@example.com/doc.pdf",        // credentials in URL
            "https://127.0.0.1/doc.pdf",                    // loopback
            "https://10.0.0.5/doc.pdf",                     // private RFC1918
            "https://192.168.1.1/doc.pdf",                  // private RFC1918
            "https://169.254.169.254/latest/meta-data/",    // cloud metadata
            "https://localhost/doc.pdf",                    // localhost hostname
            "https://metadata.google.internal/doc.pdf",     // internal hostname
            "https://myhost.local/doc.pdf",                 // mDNS `.local` hostname
            "ftp://example.com/doc.pdf",                    // non-http scheme
            "https://example.com/{{trigger.input_id}}.pdf", // templated target
            "not a url",                                    // unparseable
            "   ",                                          // blank
        ] {
            let def = doc_def(tt_shared::messages::DocumentSource::Url {
                url: url.to_string(),
            });
            let errs = validate(&def, &any_model).unwrap_err();
            assert!(
                !errs.is_empty(),
                "expected URL-source admission rejection for `{url}`"
            );
        }
    }

    /// A Document node with empty base64 data must be rejected.
    #[test]
    fn document_node_rejects_empty_data() {
        let def = doc_def(tt_shared::messages::DocumentSource::Base64 {
            media_type: "application/pdf".into(),
            data: "   ".into(),
        });
        let errs = validate(&def, &any_model).unwrap_err();
        let combined = errs.join("\n");
        assert!(
            combined.contains("data") && combined.contains("base64"),
            "expected empty-data rejection; got: {combined}"
        );
    }

    /// Build a minimal Trigger → Document → Output flow with the given source.
    fn doc_def(source: tt_shared::messages::DocumentSource) -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "doc".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "d".into(),
                    kind: NodeKind::Document {
                        source,
                        cache_key: None,
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
                    to: "d".into(),
                    map: None,
                },
                Edge {
                    from: "d".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: Vec::new(),
            metadata: serde_json::Value::Null,
        }
    }

    // ------------------------------------------------------------------
    // CO-2: triggers (schedule/webhook) validation
    // ------------------------------------------------------------------
    #[test]
    fn validate_triggers_empty_is_ok() {
        let mut def = linear_def("m");
        def.triggers = vec![];
        assert!(validate(&def, &any_model).is_ok());
    }

    #[test]
    fn validate_triggers_accepts_valid_schedule_and_webhook() {
        let mut def = linear_def("m");
        def.triggers = vec![
            WorkflowTrigger::Schedule {
                interval: "6h".into(),
                environment: None,
            },
            WorkflowTrigger::Webhook {
                token_id: "t-1_a".into(),
                environment: None,
            },
        ];
        assert!(validate(&def, &any_model).is_ok());
    }

    #[test]
    fn validate_triggers_accepts_the_one_hour_schedule_floor() {
        let mut def = linear_def("m");
        def.triggers = vec![WorkflowTrigger::Schedule {
            interval: "1h".into(),
            environment: None,
        }];
        assert!(validate(&def, &any_model).is_ok());
    }

    #[test]
    fn validate_triggers_rejects_garbage_interval() {
        let mut def = linear_def("m");
        def.triggers = vec![WorkflowTrigger::Schedule {
            interval: "soon".into(),
            environment: None,
        }];
        let errs = validate(&def, &any_model).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("not a valid duration")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_triggers_rejects_interval_below_minimum() {
        let mut def = linear_def("m");
        def.triggers = vec![WorkflowTrigger::Schedule {
            interval: "30m".into(),
            environment: None,
        }];
        let errs = validate(&def, &any_model).unwrap_err();
        assert!(
            errs.iter().any(|e| {
                e.contains("1-hour minimum") && e.contains("approximate hourly sweep")
            }),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_for_execution_keeps_legacy_sub_hour_schedule_runnable() {
        let mut def = linear_def("m");
        def.triggers = vec![WorkflowTrigger::Schedule {
            interval: "30m".into(),
            environment: None,
        }];
        assert!(validate_for_execution(&def, &any_model).is_ok());
    }

    #[test]
    fn validate_for_execution_keeps_the_preexisting_safety_floor() {
        let mut def = linear_def("m");
        def.triggers = vec![WorkflowTrigger::Schedule {
            interval: "1m".into(),
            environment: None,
        }];
        let errs = validate_for_execution(&def, &any_model).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("5-minute legacy compatibility minimum")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_triggers_rejects_two_schedules() {
        let mut def = linear_def("m");
        def.triggers = vec![
            WorkflowTrigger::Schedule {
                interval: "6h".into(),
                environment: None,
            },
            WorkflowTrigger::Schedule {
                interval: "12h".into(),
                environment: None,
            },
        ];
        let errs = validate(&def, &any_model).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("at most one schedule")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_triggers_rejects_empty_or_unsafe_webhook_token() {
        let mut def = linear_def("m");
        def.triggers = vec![WorkflowTrigger::Webhook {
            token_id: "".into(),
            environment: None,
        }];
        let errs = validate(&def, &any_model).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("must not be empty")),
            "{errs:?}"
        );

        def.triggers = vec![WorkflowTrigger::Webhook {
            token_id: "bad token!".into(),
            environment: None,
        }];
        let errs = validate(&def, &any_model).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("URL-safe")), "{errs:?}");
    }

    #[test]
    fn parse_interval_handles_compound_and_units() {
        use std::time::Duration;
        assert_eq!(parse_interval("6h"), Some(Duration::from_secs(6 * 3600)));
        assert_eq!(parse_interval("30m"), Some(Duration::from_secs(30 * 60)));
        assert_eq!(parse_interval("1d"), Some(Duration::from_secs(24 * 3600)));
        assert_eq!(parse_interval("1d6h"), Some(Duration::from_secs(30 * 3600)));
        assert_eq!(parse_interval("soon"), None);
        assert_eq!(parse_interval(""), None);
        assert_eq!(parse_interval("6"), None); // a number with no unit
    }
}
