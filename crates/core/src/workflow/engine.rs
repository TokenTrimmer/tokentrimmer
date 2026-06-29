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
//! - Model/Agent nodes in the same wave run concurrently (W3a-1 Task 2);
//!   control nodes (Trigger, Transform, Branch, Output) are always sequential.
//! - A single explicit `def.edges` arc from a Branch node is treated as
//!   unconditional; avoid adding explicit outgoing edges from Branch nodes.
//! - The topo order is defensive: if validate somehow missed a cycle the engine
//!   returns `WfStatus::Failed` rather than looping.

use std::collections::{HashMap, HashSet, VecDeque};

use tt_shared::context::SecretString;

use crate::routes::agent_run_budget::budget_reached;
use crate::workflow::events::WfEvent;
use crate::workflow::executor::{IntelligenceSpec, NodeExecutor};
use crate::workflow::http::{self as wf_http, HttpReqSpec, DEFAULT_MAX_RESPONSE_BYTES};
use crate::workflow::schedule;
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
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkflowRunResult {
    pub status: WfStatus,
    pub cost_usd: f64,
    /// Sum of each executed node's `NodeOutput.baseline_cost_usd` — what this
    /// run would have cost without TokenTrimmer optimization.
    pub baseline_cost_usd: f64,
    /// `(baseline_cost_usd - cost_usd).max(0.0)`: USD saved by routing.
    /// Reported for all terminal paths (Succeeded, Failed, BudgetExhausted).
    pub saved_usd: f64,
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
/// `events` is an optional channel sink; when `Some`, the engine emits
/// [`WfEvent::NodeStart`] / [`WfEvent::NodeDone`] for every executed node and
/// a terminal [`WfEvent::RunDone`] before returning.  When `None` the sync
/// path is byte-identical.
pub(crate) async fn run_workflow(
    executor: &dyn NodeExecutor,
    def: &WorkflowDefinition,
    inputs: &serde_json::Value,
    run_max_cost_usd: Option<f64>,
    mut journal: impl FnMut(NodeJournalEntry),
    events: Option<&tokio::sync::mpsc::UnboundedSender<WfEvent>>,
    secrets: &HashMap<String, SecretString>,
) -> WorkflowRunResult {
    // No-op when events is None; Option<&T> is Copy so captured by value.
    let emit = |ev: WfEvent| {
        if let Some(tx) = events {
            let _ = tx.send(ev);
        }
    };
    // ---- 1. Find the Trigger node -----------------------------------------
    let trigger_id = match def
        .nodes
        .iter()
        .find(|n| matches!(n.kind, NodeKind::Trigger))
    {
        Some(n) => n.id.clone(),
        None => {
            emit(WfEvent::RunDone {
                status: "failed".to_string(),
                cost_usd: 0.0,
                baseline_cost_usd: 0.0,
                saved_usd: 0.0,
            });
            return WorkflowRunResult {
                status: WfStatus::Failed,
                cost_usd: 0.0,
                baseline_cost_usd: 0.0,
                saved_usd: 0.0,
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
            emit(WfEvent::RunDone {
                status: "failed".to_string(),
                cost_usd: 0.0,
                baseline_cost_usd: 0.0,
                saved_usd: 0.0,
            });
            return WorkflowRunResult {
                status: WfStatus::Failed,
                cost_usd: 0.0,
                baseline_cost_usd: 0.0,
                saved_usd: 0.0,
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
    let mut accrued_baseline: f64 = 0.0;
    // Nodes reachable along taken edges from Trigger.
    let mut reachable: HashSet<String> = HashSet::new();
    reachable.insert(trigger_id.clone());

    // ---- 6. Build wavefront scheduling data --------------------------------
    let topo_index: HashMap<String, usize> = topo_order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), i))
        .collect();
    let pred = schedule::build_rev_adj(&adj);
    let mut done: HashSet<String> = HashSet::new();

    // ---- 7. Execute via wavefront (concurrent within each wave) ---------------
    loop {
        let mut wave = schedule::ready_nodes(&reachable, &done, &pred);
        if wave.is_empty() {
            break;
        }
        // Stable topo order ensures deterministic fold and event ordering.
        wave.sort_by_key(|id| topo_index.get(id).copied().unwrap_or(usize::MAX));

        // Partition into Model/Agent (concurrent async) and Control (sequential).
        // Control nodes run AFTER the Model/Agent batch so any model outputs in
        // the same wave are available to them (mixed waves are rare but handled
        // deterministically).
        let (model_agent_wave, control_wave): (Vec<String>, Vec<String>) =
            wave.into_iter().partition(|id| {
                matches!(
                    node_map.get(id.as_str()).map(|n| &n.kind),
                    Some(NodeKind::Model { .. }) | Some(NodeKind::Agent { .. })
                )
            });

        // ==============================================================
        // A. Concurrent Model/Agent batch
        // ==============================================================
        if !model_agent_wave.is_empty() {
            // HARD BUDGET GATE — checked once before launching any node in the wave.
            //
            // GUARANTEE: no Model/Agent node is ever LAUNCHED when accrued >= cap.
            // OVERSHOOT BOUND: if the gate passes (accrued < cap), ALL nodes in
            // the wave launch concurrently. Their individual costs are unknown
            // pre-launch, so accrued may overshoot the cap by up to
            // sum(launched-node costs) before the gate is re-checked at the start
            // of the next wave. Per-node cost reservation is not attempted (costs
            // are unknown until completion). This is the endorsed bound for
            // concurrent execution; the guarantee is "never launch past cap".
            if budget_reached(accrued, run_max_cost_usd) {
                let saved_usd = (accrued_baseline - accrued).max(0.0);
                emit(WfEvent::RunDone {
                    status: "budget_exhausted".to_string(),
                    cost_usd: accrued,
                    baseline_cost_usd: accrued_baseline,
                    saved_usd,
                });
                return WorkflowRunResult {
                    status: WfStatus::BudgetExhausted,
                    cost_usd: accrued,
                    baseline_cost_usd: accrued_baseline,
                    saved_usd,
                    node_outputs: collected_outputs,
                    error: None,
                };
            }

            // Build IntelligenceSpecs sequentially — reads `outputs` and
            // `trigger_id` which are not touched by the concurrent futures.
            let mut specs: Vec<(String, IntelligenceSpec)> = Vec::new();
            for node_id in &model_agent_wave {
                let node = match node_map.get(node_id.as_str()) {
                    Some(n) => n,
                    None => {
                        done.insert(node_id.clone());
                        continue;
                    }
                };
                match &node.kind {
                    NodeKind::Model {
                        selection,
                        prompt,
                        max_cost_usd: node_cap,
                    } => {
                        if matches!(selection, ModelSelection::Auto) {
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            emit(WfEvent::RunDone {
                                status: "failed".to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: WfStatus::Failed,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: Some(
                                    "Auto model selection is not supported in W1a; \
                                     pin a model or route_ref"
                                        .into(),
                                ),
                            };
                        }
                        specs.push((
                            node_id.clone(),
                            IntelligenceSpec {
                                selection: selection.clone(),
                                prompt: substitute(prompt, &trigger_id, &outputs),
                                tools: vec![],
                                max_turns: 1,
                                max_cost_usd: *node_cap,
                            },
                        ));
                    }
                    NodeKind::Agent {
                        selection,
                        prompt,
                        max_turns,
                        max_cost_usd: node_cap,
                        tools,
                    } => {
                        if matches!(selection, ModelSelection::Auto) {
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            emit(WfEvent::RunDone {
                                status: "failed".to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: WfStatus::Failed,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: Some(
                                    "Auto model selection is not supported in W1a; \
                                     pin a model or route_ref"
                                        .into(),
                                ),
                            };
                        }
                        specs.push((
                            node_id.clone(),
                            IntelligenceSpec {
                                selection: selection.clone(),
                                prompt: substitute(prompt, &trigger_id, &outputs),
                                tools: tools.clone(),
                                max_turns: max_turns.unwrap_or(DEFAULT_MAX_TURNS),
                                max_cost_usd: *node_cap,
                            },
                        ));
                    }
                    _ => unreachable!("partitioned into model_agent_wave above"),
                }
            }

            // Fix #4: emit NodeStart for all concurrent nodes BEFORE launch
            // so streaming clients see every node start before any NodeDone.
            for (node_id, _) in &specs {
                emit(WfEvent::NodeStart {
                    node_id: node_id.clone(),
                });
            }
            // Run all specs concurrently; results returned in stable topo order.
            let results = schedule::run_concurrent_model_wave(executor, &specs).await;

            // DETERMINISTIC FOLD: NodeDone events + journal entries emitted
            // in stable topo order, never inside the concurrent futures.
            // Fix #1: drain the whole vec before bailing so successful
            // siblings' costs always accrue even on partial wave failure.
            let mut wave_error: Option<String> = None;
            for schedule::ConcurrentNodeResult { node_id, outcome } in results {
                match outcome {
                    Err(e) => {
                        if wave_error.is_none() {
                            wave_error = Some(format!("node \"{node_id}\" failed: {e}"));
                        }
                    }
                    Ok(out) => {
                        accrued += out.cost_usd;
                        accrued_baseline += out.baseline_cost_usd;
                        let node_cost_usd = out.cost_usd; // f64 is Copy; captured before move
                        journal(NodeJournalEntry {
                            node_id: node_id.clone(),
                            status: "completed".into(),
                            output: Some(out.content.clone()),
                            cost_usd: out.cost_usd,
                            model_used: out.model_used.clone(),
                            error: None,
                        });
                        outputs.insert(node_id.clone(), out);
                        propagate_edges(&node_id, def, &mut reachable);
                        emit(WfEvent::NodeDone {
                            node_id: node_id.clone(),
                            cost_usd: node_cost_usd,
                            run_cost_usd: accrued,
                            baseline_cost_usd: accrued_baseline,
                            saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                            budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                        });
                    }
                }
                done.insert(node_id.clone());
            }
            if let Some(error) = wave_error {
                let saved_usd = (accrued_baseline - accrued).max(0.0);
                emit(WfEvent::RunDone {
                    status: "failed".to_string(),
                    cost_usd: accrued,
                    baseline_cost_usd: accrued_baseline,
                    saved_usd,
                });
                return WorkflowRunResult {
                    status: WfStatus::Failed,
                    cost_usd: accrued,
                    baseline_cost_usd: accrued_baseline,
                    saved_usd,
                    node_outputs: collected_outputs,
                    error: Some(error),
                };
            }
        }

        // ==============================================================
        // B. Sequential Control nodes (Trigger, Transform, Branch, Output)
        // ==============================================================
        for node_id in &control_wave {
            let node = match node_map.get(node_id.as_str()) {
                Some(n) => n,
                None => {
                    done.insert(node_id.clone());
                    continue;
                }
            };

            emit(WfEvent::NodeStart {
                node_id: node_id.clone(),
            });

            match &node.kind {
                // ---------------------------------------------------------------
                NodeKind::Trigger => {
                    let out = NodeOutput {
                        content: inputs.clone(),
                        cost_usd: 0.0,
                        baseline_cost_usd: 0.0,
                        model_used: None,
                    };
                    outputs.insert(node_id.clone(), out);
                    propagate_edges(node_id, def, &mut reachable);
                    // Trigger is not journaled (no model/cost).
                    emit(WfEvent::NodeDone {
                        node_id: node_id.clone(),
                        cost_usd: 0.0,
                        run_cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                        budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                    });
                }

                // ---------------------------------------------------------------
                NodeKind::Transform { expr } => {
                    let value = substitute(expr, &trigger_id, &outputs);
                    let out = NodeOutput {
                        content: serde_json::Value::String(value.clone()),
                        cost_usd: 0.0,
                        baseline_cost_usd: 0.0,
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
                    emit(WfEvent::NodeDone {
                        node_id: node_id.clone(),
                        cost_usd: 0.0,
                        run_cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                        budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                    });
                }

                // ---------------------------------------------------------------
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
                    emit(WfEvent::NodeDone {
                        node_id: node_id.clone(),
                        cost_usd: 0.0,
                        run_cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                        budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                    });
                }

                // ---------------------------------------------------------------
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
                    emit(WfEvent::NodeDone {
                        node_id: node_id.clone(),
                        cost_usd: 0.0,
                        run_cost_usd: accrued,
                        baseline_cost_usd: accrued_baseline,
                        saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                        budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                    });
                }

                NodeKind::Model { .. } | NodeKind::Agent { .. } => {
                    unreachable!("partitioned into model_agent_wave above")
                }

                // Http node: guarded outbound HTTP call (W3b Task 3).
                NodeKind::Http {
                    method,
                    url,
                    headers,
                    body,
                    max_response_bytes,
                } => {
                    // SECURITY: Substitute templates using substitute_with_secrets so
                    // {{secrets.NAME}} refs resolve to real values on the wire spec.
                    // The resulting HttpReqSpec may contain secret values and MUST NOT
                    // be written to any journal, NodeOutput.content, or error string.
                    let sub_url =
                        wf_http::substitute_with_secrets(url, &trigger_id, &outputs, secrets);
                    let sub_headers: Vec<(String, String)> = headers
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                wf_http::substitute_with_secrets(v, &trigger_id, &outputs, secrets),
                            )
                        })
                        .collect();
                    let sub_body = body.as_ref().map(|b| {
                        wf_http::substitute_with_secrets(b, &trigger_id, &outputs, secrets)
                    });

                    let spec = HttpReqSpec {
                        method: method.clone(),
                        url: sub_url,
                        headers: sub_headers,
                        body: sub_body,
                        max_response_bytes: max_response_bytes
                            .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
                    };

                    match wf_http::run_http(spec, &def.allowed_hosts).await {
                        Ok(resp) => {
                            // SECURITY CHOKEPOINT: journal + output = RESPONSE ONLY.
                            // NEVER include the substituted url/headers/body (they may
                            // contain secrets). Http is zero-cost (no model call).
                            // Include status so downstream nodes can branch on it.
                            let resp_content = serde_json::json!({
                                "status": resp.status,
                                "body": resp.body,
                            });
                            let out = NodeOutput {
                                content: resp_content.clone(),
                                cost_usd: 0.0,
                                baseline_cost_usd: 0.0,
                                model_used: None,
                            };
                            journal(NodeJournalEntry {
                                node_id: node_id.clone(),
                                status: "completed".into(),
                                output: Some(resp_content),
                                cost_usd: 0.0,
                                model_used: None,
                                error: None,
                            });
                            outputs.insert(node_id.clone(), out);
                            propagate_edges(node_id, def, &mut reachable);
                            emit(WfEvent::NodeDone {
                                node_id: node_id.clone(),
                                cost_usd: 0.0,
                                run_cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd_so_far: (accrued_baseline - accrued).max(0.0),
                                budget_remaining_usd: run_max_cost_usd.map(|m| m - accrued),
                            });
                        }
                        Err(e) => {
                            // SECURITY: HttpError strings are sanitized (no url/headers/secrets).
                            let saved_usd = (accrued_baseline - accrued).max(0.0);
                            emit(WfEvent::RunDone {
                                status: "failed".to_string(),
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                            });
                            return WorkflowRunResult {
                                status: WfStatus::Failed,
                                cost_usd: accrued,
                                baseline_cost_usd: accrued_baseline,
                                saved_usd,
                                node_outputs: collected_outputs,
                                error: Some(format!("node \"{node_id}\": http error: {e}")),
                            };
                        }
                    }
                }
            }

            done.insert(node_id.clone());
        } // for node_id in &control_wave
    } // loop

    let saved_usd = (accrued_baseline - accrued).max(0.0);
    emit(WfEvent::RunDone {
        status: "completed".to_string(),
        cost_usd: accrued,
        baseline_cost_usd: accrued_baseline,
        saved_usd,
    });
    WorkflowRunResult {
        status: WfStatus::Succeeded,
        cost_usd: accrued,
        baseline_cost_usd: accrued_baseline,
        saved_usd,
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
/// - `"secrets.*"` → **always** `"***"` (redaction marker, never the real value).
///   Secrets are resolved exclusively in `wf_http::substitute_with_secrets` so
///   that Model/Agent prompts, Transform exprs, and Branch conditions are always
///   secret-free.
fn resolve_ref(ref_str: &str, trigger_id: &str, outputs: &HashMap<String, NodeOutput>) -> String {
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
    use tt_shared::context::SecretString;
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
        /// workflow_id → WorkflowDefinition registry for sub-workflow loading tests.
        subworkflows: HashMap<Uuid, WorkflowDefinition>,
    }

    impl StubExecutor {
        fn new(responses: Vec<(&str, NodeOutput)>) -> Self {
            StubExecutor {
                responses: responses
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                calls: std::sync::Mutex::new(Vec::new()),
                subworkflows: HashMap::new(),
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

        async fn load_subworkflow(&self, id: Uuid) -> Result<WorkflowDefinition, ApiError> {
            self.subworkflows
                .get(&id)
                .cloned()
                .ok_or_else(|| ApiError::NotFound(format!("stub: no subworkflow with id {id}")))
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
            allowed_hosts: vec![],
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
            allowed_hosts: vec![],
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
            allowed_hosts: vec![],
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
                    baseline_cost_usd: 0.0,
                    model_used: Some("haiku".into()),
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("response_2"),
                    cost_usd: 0.15,
                    baseline_cost_usd: 0.0,
                    model_used: Some("haiku".into()),
                },
            ),
        ]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("hello"),
            None,
            |e| journal_entries.push(e),
            None,
            &HashMap::new(),
        )
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
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.25,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
        ]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        // cap = 0.20: before m1 accrued=0.0 < 0.20 → runs; after m1 accrued=0.25 >= 0.20
        // → before m2 budget_reached=true → BudgetExhausted.
        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            Some(0.20),
            |e| journal_entries.push(e),
            None,
            &HashMap::new(),
        )
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
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
            (
                "m_no",
                NodeOutput {
                    content: json!("no_out"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
        ]);

        // Input "yes" → cond `{{input}} == "yes"` is true → when_true = m_yes.
        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("yes"),
            None,
            |e| journal_entries.push(e),
            None,
            &HashMap::new(),
        )
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
                baseline_cost_usd: 0.0,
                model_used: None,
            },
        )]);

        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("hello"),
            None,
            |e| journal_entries.push(e),
            None,
            &HashMap::new(),
        )
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
            },
        );
        assert!(!eval_cond("{{input}}", "t", &outputs));
    }

    // ---- Task 4: engine sums baseline + computes saved ----------------------

    /// sequential run with non-zero baseline: assert baseline_cost_usd sums
    /// both nodes' baselines, and saved_usd = (baseline - cost).max(0.0).
    #[tokio::test]
    async fn test_sequential_run_baseline_and_saved() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: None,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: None,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        // cost: 0.10 + 0.05 = 0.15
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd expected 0.15, got {}",
            result.cost_usd
        );
        // baseline: 0.20 + 0.15 = 0.35
        assert!(
            (result.baseline_cost_usd - 0.35).abs() < 1e-9,
            "baseline_cost_usd expected 0.35, got {}",
            result.baseline_cost_usd
        );
        // saved: (0.35 - 0.15).max(0.0) = 0.20
        assert!(
            (result.saved_usd - 0.20).abs() < 1e-9,
            "saved_usd expected 0.20, got {}",
            result.saved_usd
        );
    }

    /// When cost >= baseline, saved_usd must be 0.0 (no negative savings).
    #[tokio::test]
    async fn test_saved_usd_never_negative() {
        let def = make_sequential_def();
        // cost > baseline (pathological but must not produce negative saved)
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.30,
                    baseline_cost_usd: 0.10,
                    model_used: None,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.0,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("x"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert_eq!(result.saved_usd, 0.0, "saved_usd must not go negative");
    }

    // ---- Task 2: optional event sink ----------------------------------------

    /// Event sink emits NodeStart + NodeDone for every executed node (including
    /// Trigger and Output which are not journaled) and exactly one terminal
    /// RunDone whose cost_usd / saved_usd match the WorkflowRunResult.
    #[tokio::test]
    async fn run_workflow_emits_node_and_run_events() {
        // Sequential def: t → m1 → m2 → o (4 reachable nodes)
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: Some("haiku".into()),
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: Some("haiku".into()),
                },
            ),
        ]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WfEvent>();
        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            None,
            |_| {},
            Some(&tx),
            &HashMap::new(),
        )
        .await;
        drop(tx); // close the channel so try_recv returns Disconnected when drained

        assert_eq!(result.status, WfStatus::Succeeded);

        // Drain all events.
        let mut events: Vec<WfEvent> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        // Expect: NodeStart(t), NodeDone(t), NodeStart(m1), NodeDone(m1),
        //         NodeStart(m2), NodeDone(m2), NodeStart(o), NodeDone(o), RunDone
        assert_eq!(
            events.len(),
            9,
            "expected 9 events (4 nodes × 2 + RunDone); got {events:?}"
        );

        // Pair each NodeStart with the following NodeDone for expected node_ids.
        let node_order = ["t", "m1", "m2", "o"];
        for (i, expected_id) in node_order.iter().enumerate() {
            let start_idx = i * 2;
            let done_idx = start_idx + 1;
            match &events[start_idx] {
                WfEvent::NodeStart { node_id } => {
                    assert_eq!(node_id, expected_id, "NodeStart[{i}] wrong node_id")
                }
                other => panic!("events[{start_idx}] expected NodeStart, got {other:?}"),
            }
            match &events[done_idx] {
                WfEvent::NodeDone { node_id, .. } => {
                    assert_eq!(node_id, expected_id, "NodeDone[{i}] wrong node_id")
                }
                other => panic!("events[{done_idx}] expected NodeDone, got {other:?}"),
            }
        }

        // Final event must be RunDone matching WorkflowRunResult.
        match &events[8] {
            WfEvent::RunDone {
                status,
                cost_usd,
                baseline_cost_usd,
                saved_usd,
            } => {
                assert_eq!(status, "completed");
                assert!(
                    (cost_usd - result.cost_usd).abs() < 1e-9,
                    "RunDone cost_usd {cost_usd} != result {}",
                    result.cost_usd
                );
                assert!(
                    (baseline_cost_usd - result.baseline_cost_usd).abs() < 1e-9,
                    "RunDone baseline_cost_usd mismatch"
                );
                assert!(
                    (saved_usd - result.saved_usd).abs() < 1e-9,
                    "RunDone saved_usd {saved_usd} != result {}",
                    result.saved_usd
                );
            }
            other => panic!("events[8] expected RunDone, got {other:?}"),
        }

        // Verify burndown in NodeDone(m1): run_cost=0.10, saved_so_far=0.10 (0.20-0.10).
        if let WfEvent::NodeDone {
            run_cost_usd,
            saved_usd_so_far,
            ..
        } = &events[3]
        {
            assert!(
                (run_cost_usd - 0.10).abs() < 1e-9,
                "m1 NodeDone run_cost_usd expected 0.10, got {run_cost_usd}"
            );
            assert!(
                (saved_usd_so_far - 0.10).abs() < 1e-9,
                "m1 NodeDone saved_usd_so_far expected 0.10, got {saved_usd_so_far}"
            );
        } else {
            panic!("events[3] not NodeDone");
        }
    }

    /// With events=None the returned WorkflowRunResult is byte-identical to
    /// the Some(&tx) run — the event sink does not affect the sync result path.
    #[tokio::test]
    async fn run_workflow_none_events_is_sync_identical() {
        let def = make_sequential_def();
        let responses = vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: Some("haiku".into()),
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: Some("haiku".into()),
                },
            ),
        ];

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<WfEvent>();
        let result_with_events = run_workflow(
            &StubExecutor::new(responses.clone()),
            &def,
            &json!("hi"),
            None,
            |_| {},
            Some(&tx),
            &HashMap::new(),
        )
        .await;
        drop(tx);

        let result_none = run_workflow(
            &StubExecutor::new(responses),
            &def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(
            result_with_events.status, result_none.status,
            "status must be identical"
        );
        assert!(
            (result_with_events.cost_usd - result_none.cost_usd).abs() < 1e-9,
            "cost_usd must be identical"
        );
        assert!(
            (result_with_events.baseline_cost_usd - result_none.baseline_cost_usd).abs() < 1e-9,
            "baseline_cost_usd must be identical"
        );
        assert!(
            (result_with_events.saved_usd - result_none.saved_usd).abs() < 1e-9,
            "saved_usd must be identical"
        );
        assert_eq!(
            result_with_events.error, result_none.error,
            "error must be identical"
        );
    }

    // ---- Wavefront parity tests (W3a-1 Task 1) --------------------------------

    /// Diamond: t → {mb, mc} → out  (trigger fans out to two parallel model
    /// nodes, both converge on a single output node).
    fn make_diamond_def() -> WorkflowDefinition {
        WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "diamond".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "mb".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "mc".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub-model".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "out".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "mb".into(),
                    map: None,
                },
                Edge {
                    from: "t".into(),
                    to: "mc".into(),
                    map: None,
                },
                Edge {
                    from: "mb".into(),
                    to: "out".into(),
                    map: None,
                },
                Edge {
                    from: "mc".into(),
                    to: "out".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
        }
    }

    /// Parity: 4-node chain (trigger → m1 → m2 → output) must produce the
    /// same WorkflowRunResult as the former linear topo pass.
    #[tokio::test]
    async fn wavefront_linear_chain_matches_expected() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: None,
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: None,
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd expected 0.15, got {}",
            result.cost_usd
        );
        assert!(
            (result.baseline_cost_usd - 0.35).abs() < 1e-9,
            "baseline_cost_usd expected 0.35, got {}",
            result.baseline_cost_usd
        );
        assert!(
            (result.saved_usd - 0.20).abs() < 1e-9,
            "saved_usd expected 0.20, got {}",
            result.saved_usd
        );
        assert_eq!(
            stub.called_nodes(),
            vec!["m1", "m2"],
            "m1 then m2 in topo order"
        );
        // Output node collects m2's result.
        assert_eq!(result.node_outputs.len(), 1, "one output collected");
        assert_eq!(result.node_outputs[0].0, "o");
        assert_eq!(result.node_outputs[0].1.content, json!("r2"));
    }

    /// Diamond (t → {mb, mc} → out): all four nodes must execute; mb before mc
    /// (stable topo order); running three times yields identical results.
    #[tokio::test]
    async fn wavefront_diamond_executes_all_and_is_deterministic() {
        let def = make_diamond_def();

        let make_stub = || {
            StubExecutor::new(vec![
                (
                    "mb",
                    NodeOutput {
                        content: json!("mb_out"),
                        cost_usd: 0.05,
                        baseline_cost_usd: 0.10,
                        model_used: None,
                    },
                ),
                (
                    "mc",
                    NodeOutput {
                        content: json!("mc_out"),
                        cost_usd: 0.07,
                        baseline_cost_usd: 0.12,
                        model_used: None,
                    },
                ),
            ])
        };

        // First run: verify both models called in topo order, output collected.
        let stub1 = make_stub();
        let r1 = run_workflow(
            &stub1,
            &def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;
        assert_eq!(r1.status, WfStatus::Succeeded);
        let called = stub1.called_nodes();
        assert_eq!(called, vec!["mb", "mc"], "mb before mc (stable topo order)");
        // Output node has two incoming edges (mb→out, mc→out); both collected.
        assert_eq!(
            r1.node_outputs.len(),
            2,
            "output node collects from both branches"
        );
        assert!(
            (r1.cost_usd - 0.12).abs() < 1e-9,
            "cost_usd expected 0.12, got {}",
            r1.cost_usd
        );

        // Three runs must yield byte-identical results.
        let mut costs: Vec<f64> = Vec::new();
        for _ in 0..3 {
            let r = run_workflow(
                &make_stub(),
                &def,
                &json!("go"),
                None,
                |_| {},
                None,
                &HashMap::new(),
            )
            .await;
            assert_eq!(r.status, WfStatus::Succeeded);
            costs.push(r.cost_usd);
        }
        assert!(
            costs.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-9),
            "results must be identical across runs: {costs:?}"
        );
    }

    /// Skipped branch arm + merge node: Branch takes `m_yes`; `m_no` is
    /// unreachable.  The merge output node (`o`) has incoming edges from BOTH
    /// arms but must still execute, because its only *reachable* predecessor
    /// (`m_yes`) is done — wavefront readiness = reachable-preds, not raw
    /// in-degree.
    #[tokio::test]
    async fn wavefront_skipped_branch_merge_still_reachable() {
        let def = make_branch_def(); // t → br(cond={{input}}=="yes") → {m_yes, m_no} → o
        let stub = StubExecutor::new(vec![
            (
                "m_yes",
                NodeOutput {
                    content: json!("yes_out"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
            (
                "m_no",
                NodeOutput {
                    content: json!("no_out"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
        ]);

        // Input "yes" → branch takes m_yes arm; m_no is skipped.
        let result = run_workflow(
            &stub,
            &def,
            &json!("yes"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let called = stub.called_nodes();
        assert!(
            called.contains(&"m_yes".to_string()),
            "m_yes must run; called: {called:?}"
        );
        assert!(
            !called.contains(&"m_no".to_string()),
            "m_no must NOT run; called: {called:?}"
        );
        // The merge output node must have executed and collected m_yes's result.
        assert_eq!(
            result.node_outputs.len(),
            1,
            "merge output node must execute (reachable-preds readiness)"
        );
        assert_eq!(result.node_outputs[0].0, "o");
        assert_eq!(result.node_outputs[0].1.content, json!("yes_out"));
    }

    // ---- W3a-1 Task 2: concurrent wave tests -----------------------------------

    /// Proof-of-parallelism: mb and mc in the diamond must run concurrently.
    ///
    /// A stub that increments an AtomicUsize on entry, records the peak
    /// in-flight count via fetch_max, then yields before decrementing lets us
    /// observe overlap: when join_all polls mb (yield → pending), it then polls
    /// mc, bringing in_flight to 2 before either completes.
    #[tokio::test]
    async fn concurrent_wave_runs_nodes_in_parallel() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct ParallelProbe {
            in_flight: Arc<AtomicUsize>,
            max_in_flight: Arc<AtomicUsize>,
            responses: HashMap<String, NodeOutput>,
        }

        #[async_trait]
        impl NodeExecutor for ParallelProbe {
            async fn run_intelligence(
                &self,
                node_id: &str,
                _spec: &IntelligenceSpec,
            ) -> Result<NodeOutput, ApiError> {
                // Increment in-flight and record the peak BEFORE yielding so
                // that both mb and mc have incremented before either returns.
                let prev = self.in_flight.fetch_add(1, Ordering::SeqCst);
                self.max_in_flight.fetch_max(prev + 1, Ordering::SeqCst);
                // Yield control so the runtime can poll the other futures in
                // the wave; on the second poll we decrement and return.
                tokio::task::yield_now().await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.responses
                    .get(node_id)
                    .cloned()
                    .ok_or_else(|| ApiError::Internal(format!("no response for {node_id}")))
            }

            async fn load_subworkflow(&self, id: Uuid) -> Result<WorkflowDefinition, ApiError> {
                Err(ApiError::NotFound(format!(
                    "ParallelProbe stub: no subworkflow {id}"
                )))
            }
        }

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));

        let stub = ParallelProbe {
            in_flight: Arc::clone(&in_flight),
            max_in_flight: Arc::clone(&max_in_flight),
            responses: [
                (
                    "mb".to_string(),
                    NodeOutput {
                        content: json!("b"),
                        cost_usd: 0.05,
                        baseline_cost_usd: 0.0,
                        model_used: None,
                    },
                ),
                (
                    "mc".to_string(),
                    NodeOutput {
                        content: json!("c"),
                        cost_usd: 0.05,
                        baseline_cost_usd: 0.0,
                        model_used: None,
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };

        let def = make_diamond_def(); // t → {mb, mc} → out
        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        let peak = max_in_flight.load(Ordering::SeqCst);
        assert!(
            peak >= 2,
            "mb and mc must overlap in-flight (max_in_flight = {peak})"
        );
    }

    /// Hard-budget gate under concurrency.
    ///
    /// A "prior" node brings accrued to exactly the cap (1.0).  The next wave
    /// contains 3 Model nodes each costing 0.40.  The gate fires BEFORE
    /// launching any of them, so none are called and status is BudgetExhausted.
    ///
    /// # Gate guarantee
    /// No Model/Agent node is ever launched when `accrued >= run_max_cost_usd`.
    /// Overshoot is bounded by the costs of nodes launched while `accrued < cap`
    /// (pre-launch costs are unknown, so all-or-nothing per wave is enforced).
    #[tokio::test]
    async fn hard_budget_under_concurrency() {
        // Workflow: t → prior → {n1, n2, n3} → out
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "budget_gate".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "prior".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model { model: "s".into() },
                        prompt: "p".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "n1".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model { model: "s".into() },
                        prompt: "1".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "n2".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model { model: "s".into() },
                        prompt: "2".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "n3".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model { model: "s".into() },
                        prompt: "3".into(),
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "out".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "prior".into(),
                    map: None,
                },
                Edge {
                    from: "prior".into(),
                    to: "n1".into(),
                    map: None,
                },
                Edge {
                    from: "prior".into(),
                    to: "n2".into(),
                    map: None,
                },
                Edge {
                    from: "prior".into(),
                    to: "n3".into(),
                    map: None,
                },
                Edge {
                    from: "n1".into(),
                    to: "out".into(),
                    map: None,
                },
                Edge {
                    from: "n2".into(),
                    to: "out".into(),
                    map: None,
                },
                Edge {
                    from: "n3".into(),
                    to: "out".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
        };

        let stub = StubExecutor::new(vec![
            (
                "prior",
                NodeOutput {
                    content: json!("p"),
                    cost_usd: 1.0,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
            (
                "n1",
                NodeOutput {
                    content: json!("n1"),
                    cost_usd: 0.40,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
            (
                "n2",
                NodeOutput {
                    content: json!("n2"),
                    cost_usd: 0.40,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
            (
                "n3",
                NodeOutput {
                    content: json!("n3"),
                    cost_usd: 0.40,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
        ]);

        // cap = 1.0.  prior: accrued(0.0) < 1.0 → launches, costs 1.0 → accrued=1.0.
        // Wave {n1,n2,n3}: budget_reached(1.0, Some(1.0)) = true → NONE launched.
        let result = run_workflow(
            &stub,
            &def,
            &json!("x"),
            Some(1.0),
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.status, WfStatus::BudgetExhausted);
        assert!(
            (result.cost_usd - 1.0).abs() < 1e-9,
            "only prior's cost must be accrued; got {}",
            result.cost_usd
        );
        let called = stub.called_nodes();
        assert_eq!(
            called,
            vec!["prior"],
            "n1/n2/n3 must NOT be launched (hard budget gate fired); called: {called:?}"
        );
    }

    /// Determinism: running the diamond 5× must yield identical results.
    ///
    /// The post-join fold in stable topo-index order guarantees determinism
    /// regardless of which concurrent future completes first.
    #[tokio::test]
    async fn concurrent_result_is_deterministic() {
        let make_stub = || {
            StubExecutor::new(vec![
                (
                    "mb",
                    NodeOutput {
                        content: json!("b"),
                        cost_usd: 0.05,
                        baseline_cost_usd: 0.10,
                        model_used: None,
                    },
                ),
                (
                    "mc",
                    NodeOutput {
                        content: json!("c"),
                        cost_usd: 0.07,
                        baseline_cost_usd: 0.12,
                        model_used: None,
                    },
                ),
            ])
        };

        let def = make_diamond_def();
        let mut results = Vec::new();
        for _ in 0..5 {
            let r = run_workflow(
                &make_stub(),
                &def,
                &json!("go"),
                None,
                |_| {},
                None,
                &HashMap::new(),
            )
            .await;
            results.push(r);
        }

        let first = &results[0];
        assert_eq!(first.status, WfStatus::Succeeded);
        for r in &results[1..] {
            assert_eq!(
                r.status, first.status,
                "status must be identical across runs"
            );
            assert!(
                (r.cost_usd - first.cost_usd).abs() < 1e-9,
                "cost_usd must be identical across runs"
            );
            assert!(
                (r.baseline_cost_usd - first.baseline_cost_usd).abs() < 1e-9,
                "baseline_cost_usd must be identical across runs"
            );
            assert!(
                (r.saved_usd - first.saved_usd).abs() < 1e-9,
                "saved_usd must be identical across runs"
            );
            assert_eq!(
                r.node_outputs.len(),
                first.node_outputs.len(),
                "node_output count must match"
            );
        }
    }

    // ---- W3a-1 review: Fix #1 + Fix #4 guard tests ----------------------------

    /// Guards Fix #1 (drain-all cost on partial wave failure).
    ///
    /// In the diamond (t → {mb, mc} → out), mb is first in topo order and
    /// errors (no stub response); mc succeeds with cost 0.15.  Before Fix #1
    /// the fold returns on mb's error, so mc's cost is never accrued →
    /// cost_usd == 0.0, not 0.15.  After Fix #1 the fold drains the entire
    /// wave before bailing: mc accrues, result is Failed with cost_usd 0.15.
    #[tokio::test]
    async fn partial_wave_failure_reports_full_cost() {
        let def = make_diamond_def(); // t → {mb, mc} → out
                                      // mb has no stub response → Err; mc succeeds.
        let stub = StubExecutor::new(vec![(
            "mc",
            NodeOutput {
                content: json!("c"),
                cost_usd: 0.15,
                baseline_cost_usd: 0.30,
                model_used: None,
            },
        )]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.status, WfStatus::Failed, "run must be Failed");
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd must include mc's cost (0.15) even though mb failed first; got {}",
            result.cost_usd
        );
        assert!(
            (result.baseline_cost_usd - 0.30).abs() < 1e-9,
            "baseline_cost_usd must include mc's baseline (0.30); got {}",
            result.baseline_cost_usd
        );
        assert!(result.error.is_some(), "error field must be set");
        assert!(
            result.error.as_deref().unwrap().contains("mb"),
            "error must name the failing node (mb); got {:?}",
            result.error
        );
    }

    /// Guards Fix #4 (NodeStart emitted before concurrent launch).
    ///
    /// In the diamond (t → {mb, mc} → out), all wave-node NodeStart events
    /// must appear before any NodeDone for those nodes.  Before Fix #4 the
    /// fold emits NodeStart then NodeDone for each node in sequence; after
    /// Fix #4 all NodeStarts are pre-emitted and only NodeDones appear in
    /// the fold → all starts precede all dones.
    #[tokio::test]
    async fn concurrent_nodes_emit_nodestart_before_completion() {
        let def = make_diamond_def();
        let stub = StubExecutor::new(vec![
            (
                "mb",
                NodeOutput {
                    content: json!("b"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
            (
                "mc",
                NodeOutput {
                    content: json!("c"),
                    cost_usd: 0.07,
                    baseline_cost_usd: 0.0,
                    model_used: None,
                },
            ),
        ]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WfEvent>();
        let result = run_workflow(
            &stub,
            &def,
            &json!("go"),
            None,
            |_| {},
            Some(&tx),
            &HashMap::new(),
        )
        .await;
        drop(tx);
        assert_eq!(result.status, WfStatus::Succeeded);

        let mut events: Vec<WfEvent> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        // Keep only NodeStart / NodeDone events for the concurrent wave nodes.
        let wave_events: Vec<&WfEvent> = events
            .iter()
            .filter(|ev| match ev {
                WfEvent::NodeStart { node_id } => node_id == "mb" || node_id == "mc",
                WfEvent::NodeDone { node_id, .. } => node_id == "mb" || node_id == "mc",
                _ => false,
            })
            .collect();

        assert_eq!(
            wave_events.len(),
            4,
            "expected 4 wave events (2×NodeStart + 2×NodeDone); got {wave_events:?}"
        );

        let start_positions: Vec<usize> = wave_events
            .iter()
            .enumerate()
            .filter(|(_, ev)| matches!(ev, WfEvent::NodeStart { .. }))
            .map(|(i, _)| i)
            .collect();
        let done_positions: Vec<usize> = wave_events
            .iter()
            .enumerate()
            .filter(|(_, ev)| matches!(ev, WfEvent::NodeDone { .. }))
            .map(|(i, _)| i)
            .collect();

        assert_eq!(start_positions.len(), 2, "must have 2 NodeStart events");
        assert_eq!(done_positions.len(), 2, "must have 2 NodeDone events");

        let last_start = *start_positions.iter().max().unwrap();
        let first_done = *done_positions.iter().min().unwrap();

        assert!(
            last_start < first_done,
            "all wave NodeStart events must precede all wave NodeDone events; \
             starts={start_positions:?} dones={done_positions:?}"
        );
    }

    /// Regression: a purely sequential chain (waves of size 1) must produce
    /// the same result as the former sequential implementation — no regression
    /// from the concurrent-wave refactor.
    #[tokio::test]
    async fn concurrent_linear_chain_parity() {
        let def = make_sequential_def();
        let stub = StubExecutor::new(vec![
            (
                "m1",
                NodeOutput {
                    content: json!("r1"),
                    cost_usd: 0.10,
                    baseline_cost_usd: 0.20,
                    model_used: Some("haiku".into()),
                },
            ),
            (
                "m2",
                NodeOutput {
                    content: json!("r2"),
                    cost_usd: 0.05,
                    baseline_cost_usd: 0.15,
                    model_used: Some("haiku".into()),
                },
            ),
        ]);

        let result = run_workflow(
            &stub,
            &def,
            &json!("hi"),
            None,
            |_| {},
            None,
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert!(
            (result.cost_usd - 0.15).abs() < 1e-9,
            "cost_usd expected 0.15, got {}",
            result.cost_usd
        );
        assert!(
            (result.baseline_cost_usd - 0.35).abs() < 1e-9,
            "baseline_cost_usd expected 0.35, got {}",
            result.baseline_cost_usd
        );
        assert!(
            (result.saved_usd - 0.20).abs() < 1e-9,
            "saved_usd expected 0.20, got {}",
            result.saved_usd
        );
        assert_eq!(
            stub.called_nodes(),
            vec!["m1", "m2"],
            "m1 then m2 in sequential (topo) order"
        );
    }

    // ---- W3b Task 3: security guard tests -----------------------------------

    /// Verifies that the shared `substitute`/`resolve_ref` path NEVER returns a
    /// real secret value for `{{secrets.*}}` refs.  This ensures Model/Agent
    /// prompts, Transform exprs, and Branch conditions are always secret-free.
    #[test]
    fn shared_substitute_is_secret_free() {
        let outputs = HashMap::new();

        // `{{secrets.K}}` → "***" (explicit redaction marker, not the secret).
        let result = substitute("text {{secrets.K}} more", "t", &outputs);
        assert_eq!(
            result, "text *** more",
            "shared substitute must redact secrets.* refs; got: {result}"
        );

        // `{{secrets}}` (no dot) → "***".
        let result2 = substitute("{{secrets}}", "t", &outputs);
        assert_eq!(
            result2, "***",
            "shared substitute must redact bare {{secrets}}; got: {result2}"
        );

        // Confirm it is NOT the real secret value (belt-and-suspenders).
        assert_ne!(result, "sekret-value");
        assert_ne!(result2, "sekret-value");
    }

    /// THE REDACTION GUARD: asserts that a secret value NEVER appears in any
    /// observable output field after running a workflow with an Http node whose
    /// header references `{{secrets.K}}`.
    ///
    /// The Http node uses `allowed_hosts: ["other-host.com"]` but the URL host
    /// is `api.example.com` → `HostNotAllowed` fires immediately (no network
    /// call), giving us a deterministic pure-unit test.
    #[tokio::test]
    async fn http_node_never_journals_secret() {
        let secret_value = "sekret-value";
        let mut secrets = HashMap::new();
        secrets.insert("K".to_string(), SecretString::new(secret_value.to_string()));

        // Workflow: t → h (Http node referencing secret in header).
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "redaction_test".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "h".into(),
                    kind: NodeKind::Http {
                        method: "GET".into(),
                        url: "https://api.example.com/x".into(),
                        // x-auth-token is not in DENIED_HEADERS so it passes filter_extra_headers.
                        headers: vec![("x-auth-token".into(), "Bearer {{secrets.K}}".into())],
                        body: None,
                        max_response_bytes: None,
                    },
                },
            ],
            edges: vec![Edge {
                from: "t".into(),
                to: "h".into(),
                map: None,
            }],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            // "other-host.com" is in allowed_hosts, but URL host is "api.example.com"
            // → HostNotAllowed fires immediately without any network call.
            allowed_hosts: vec!["other-host.com".to_string()],
        };

        let stub = StubExecutor::new(vec![]);
        let mut journal_entries: Vec<NodeJournalEntry> = Vec::new();
        let result = run_workflow(
            &stub,
            &def,
            &json!("input"),
            None,
            |e| journal_entries.push(e),
            None,
            &secrets,
        )
        .await;

        // The run must have failed (Http node got HostNotAllowed).
        assert_eq!(
            result.status,
            WfStatus::Failed,
            "run should fail due to HostNotAllowed"
        );

        // ---- SECURITY INVARIANT: "sekret-value" must NOT appear anywhere ----

        // 1. result.error must not contain the secret.
        if let Some(ref err_str) = result.error {
            assert!(
                !err_str.contains(secret_value),
                "secret leaked into result.error: {err_str}"
            );
        }

        // 2. Journal entries (output + error fields) must not contain the secret.
        for entry in &journal_entries {
            let output_str = format!("{:?}", entry.output);
            assert!(
                !output_str.contains(secret_value),
                "secret leaked into journal output for {}: {output_str}",
                entry.node_id
            );
            if let Some(ref entry_err) = entry.error {
                assert!(
                    !entry_err.contains(secret_value),
                    "secret leaked into journal error for {}: {entry_err}",
                    entry.node_id
                );
            }
        }

        // 3. Collected node outputs must not contain the secret.
        for (nid, out) in &result.node_outputs {
            let content_str = format!("{:?}", out.content);
            assert!(
                !content_str.contains(secret_value),
                "secret leaked into node_outputs[{nid}]: {content_str}"
            );
        }
    }

    // ---- Task 1 (W3a-3): StubExecutor::load_subworkflow registry ----------------

    /// Registering a WorkflowDefinition in StubExecutor and calling
    /// `load_subworkflow` returns a clone of it; an unregistered id returns
    /// `Err(ApiError::NotFound)`.
    #[tokio::test]
    async fn stub_load_subworkflow_returns_registered_def() {
        let child_id = Uuid::new_v4();
        let child_def = WorkflowDefinition {
            id: child_id,
            version: 1,
            name: "child-wf".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![Edge {
                from: "t".into(),
                to: "o".into(),
                map: None,
            }],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
        };

        let mut stub = StubExecutor::new(vec![]);
        stub.subworkflows.insert(child_id, child_def.clone());

        // Registered id → Ok with the same definition.
        let result = stub.load_subworkflow(child_id).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let loaded = result.unwrap();
        assert_eq!(loaded.id, child_id);
        assert_eq!(loaded.name, "child-wf");

        // Unregistered id → Err(NotFound).
        let unknown_id = Uuid::new_v4();
        let miss = stub.load_subworkflow(unknown_id).await;
        assert!(
            matches!(miss, Err(ApiError::NotFound(_))),
            "expected NotFound for unknown id, got {miss:?}"
        );
    }
}
